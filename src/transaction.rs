//! 事务的控制层：目标独占所有权、生命周期、活动现场发现，以及"未收尾时谁能继续"的闸口。
//!
//! durable history 由 [`crate::dev::Journal`] 承担——本层**不新增第二份持久化事实**，
//! 只是把"谁在独占目标、这次事务算不算开着、开着时谁能动手"这三件事收到一处：
//!
//! ```text
//! begin  = 打开目标 + 取独占所有权（Journal 惰性创建：这一刻盘上还没有它）
//! commit = 事务完成 ⇒ 关闭 active 状态（删掉 journal 的那一步）
//! active = journal 在、且含记录 ⇒ 只有 undo / abandon / 续跑原命令能继续碰这个目标
//! ```
//!
//! **不保存 completion_state**：能不能算"完成"由 durable journal 与盘上状态共同决定；
//! 本层只保管盘上推导不出来的东西（独占所有权），不复制一份可能与盘上分叉的副本

use std::path::Path;

#[cfg(target_os = "linux")]
use std::os::unix::fs::FileTypeExt;

use crate::args::Args;
use crate::dev::{FileSource, Journal, TargetIdentity};
use crate::outcome::Fail;
use crate::targetlock::TargetLock;
use crate::{dev, table};

/// 一份**还活着的**恢复现场。它是"目标上还有什么没做完"的唯一枚举口径：闸口
/// （拒绝别的写命令）与 `abandon`（放弃它）都从这里取，两处不各自判断"什么算现场"
pub(crate) enum RecoveryRecord {
    /// undo journal。`entries` 为 None 表示**读不出来**（陌生文件 / magic 不符 /
    /// 中途损坏）——它同样是现场，只是已经无法判断能回滚什么
    Journal { path: std::path::PathBuf, entries: Option<Vec<dev::JournalRecord>> },
    /// 搬移 checkpoint：描述一次中断的搬移。它本身不含可回放的字节，
    /// 出路只有续跑原命令或放弃
    Checkpoint { path: std::path::PathBuf },
}

impl RecoveryRecord {
    pub(crate) fn path(&self) -> &Path {
        match self {
            RecoveryRecord::Journal { path, .. } | RecoveryRecord::Checkpoint { path } => path,
        }
    }
}

/// 盘上表里的 Disk GUID，用来认**历史命名**的 checkpoint 落点。
///
/// 读不出来只降级为 `None` 并告警，**不改变任何判定**：目标身份取自设备层拓扑
/// （见 [`TargetIdentity`]），与表内容无关，损坏 GPT 因此照样能 abandon；`None` 仅仅
/// 意味着"按 GUID 命名的那份历史候选这一轮列举不到"。静默降级则会让用户以为盘上
/// 只有一份现场，而报告里一条警告都没有
pub(crate) fn legacy_disk_guid(src: &FileSource) -> Option<[u8; 16]> {
    match table::load_gpt(src) {
        Ok(Some(g)) => Some(g.header.disk_guid),
        // 无表（MBR / 裸盘）是常态而非故障：历史命名那份本来就不存在
        Ok(None) => None,
        Err(e) => {
            eprintln!(
                "warning: cannot read the GPT Disk GUID ({e}) — a checkpoint stored under the \
                 legacy GUID-based name cannot be enumerated in this run"
            );
            None
        }
    }
}

/// 枚举目标上还活着的恢复现场。
///
/// journal 的判据不是"文件在不在"，而是**有没有至少一条完整记录**：
/// - 有记录（含只余完整前缀的截断尾部）⇒ 活着
/// - **读不出来**（陌生文件 / magic 不符 / 中途 CRC 损坏）⇒ 也按活着处理。把"读不出来"
///   当作"没有"，正是把一次中断的操作降级成一次全新操作
/// - 0 字节，或只写了 magic 就在 `ensure` 中途中断留下的空壳 ⇒ 它描述的是"零次写入"，
///   没有任何东西依赖它：既不该挡住别的命令，也不该被报成现场。否则一次在创建
///   journal 时掉电就会把目标永久锁死
///
/// checkpoint 没有"空壳"一说：它一落盘就描述一次中断的搬移，故只看存在性
pub(crate) fn active_recovery_records(
    identity: &TargetIdentity,
    legacy_disk_guid: Option<[u8; 16]>,
) -> Vec<RecoveryRecord> {
    let mut out = Vec::new();
    for p in identity.journal_candidates() {
        if !p.exists() {
            continue;
        }
        // 0 字节：连 magic 都没写完的创建残骸。它与"只含 magic 的壳"一样描述**零次写入**，
        // 故不是现场。这条判据必须与 `Journal::open` 一致——那边对 0 字节的处理是
        // "就地补 magic 后照常使用"（详见其注释）；否则一次在创建 journal 时掉电会出现
        // 两种结局：开 journal 的命令放行，走闸口的命令被永久挡住
        if std::fs::metadata(p).map(|m| m.len()).unwrap_or(0) == 0 {
            continue;
        }
        let entries = match Journal::read_entries(p) {
            Ok(dev::JournalRead::Complete(v)) | Ok(dev::JournalRead::TruncatedTail(v)) => {
                if v.is_empty() {
                    continue; // 零记录的壳：不是现场
                }
                Some(v)
            }
            Err(_) => None,
        };
        out.push(RecoveryRecord::Journal { path: p.clone(), entries });
    }
    for p in identity.checkpoint_candidates(legacy_disk_guid) {
        if p.exists() {
            out.push(RecoveryRecord::Checkpoint { path: p });
        }
    }
    out
}

/// 事务的控制层。它不持状态——"这次事务开到哪了"的唯一事实就是盘上那份 journal
/// 与目标本身；本类型只提供入口，使生命周期只有一个地方可改
pub(crate) struct TransactionManager;

/// 入口对"目标上已有现场"的处置声明——由调用方**显式选择**，事务层只执行。
/// 这是"每个入口接受哪几态"的类型化：判据不以闭包形式下沉（压成 bool 会让
/// 领域结论在闸口处丢失），事务层也不为任何入口猜"这是不是续跑"
pub(crate) enum ResumeClaim {
    /// 不接受任何现场：目标上有 active 现场即拒绝。无续跑能力的入口（`copy`）用它——
    /// 接管别人的现场再成功，会把人家的 journal 当自己的清掉
    FreshOnly,
    /// 存在 checkpoint 即按续跑进入（`resize-part` / `move`）：是否真是同一件事，
    /// 由领域层的恢复校验比对参数后裁决（不匹配即 Divergent 拒绝）
    AnyCheckpoint,
    /// 只认本分区的 relocation 作业（`resize` / `apply`）。槽位被别的分区的作业占着
    /// 时由领域层给出针对性拒绝理由；被另一族作业（单分区 resize 的 RsCheckpoint）
    /// 占着时拒绝——本入口续不了它，出路是 `resize-part` / `move`
    OwnRelocation(u32),
}

impl TransactionManager {
    /// 开一次**新的**写事务：读写打开、取独占所有权、把 journal 接到目标上。
    ///
    /// 目标上已有 active transaction 时**拒绝**（30）：一个普通 mutation 永远不能隐式
    /// 接管别人没做完的事务。接着做要走 [`Self::begin_or_resume`]，放弃要走 `abandon`——
    /// 让 `begin` 去猜"这是不是续跑"，必然导致"是不是续跑"要在两处保持一致
    ///
    /// journal 是**惰性**的：`Journal::open` 只做只读校验，此刻盘上没有任何新痕迹；
    /// 真正的记录要等到第一次真正的写入（见 [`FileSource::write_at`]）。
    /// 因此一条在校验阶段就被拒的命令不会留下空 journal，也就不会占住候选落点
    pub(crate) fn begin(a: &Args) -> Result<FileSource, Fail> {
        // FreshOnly：不接受任何现场。与 begin_or_resume 共用同一闸口——
        // "普通 mutation 永不接管别人的事务"只有一个执行处
        Self::begin_or_resume(a, ResumeClaim::FreshOnly).map(|(src, _)| src)
    }

    /// 数据搬移类命令的入口：目标上有可续跑的作业时按续跑进入，否则开新事务。
    ///
    /// 接受哪几态由 `claim` 显式声明（见 [`ResumeClaim`]）——那是领域知识，事务层只执行，
    /// 不猜"这是不是续跑"。分类发生在**锁下**的已打开目标上：先开一次再判，而不是
    /// 先判一次再开第二次——后者的判据与开目标之间存在窗口，别人可以在窗口里改变现状
    ///
    /// 领域层判据返回 `Err` 时原样上抛：领域层知道的拒绝原因（例如"槽位被别的分区的
    /// 作业占着"）必须到达 boundary，压成一个 bool 只会让它退化成通用的 busy 文案
    ///
    /// 返回是否按续跑进入：调用方（如 resize）要拿它决定后续分支
    pub(crate) fn begin_or_resume(
        a: &Args,
        claim: ResumeClaim,
    ) -> Result<(FileSource, bool), Fail> {
        let mut src = Self::open_rw(a)?;
        let active = Self::active_records(&src);
        let resumed = if active.is_empty() {
            false
        } else {
            match claim {
                ResumeClaim::FreshOnly => return Err(Self::busy(&active)),
                ResumeClaim::AnyCheckpoint => {
                    if !active.iter().any(|r| matches!(r, RecoveryRecord::Checkpoint { .. })) {
                        return Err(Self::busy(&active));
                    }
                    true
                }
                ResumeClaim::OwnRelocation(grow_part) => {
                    if !crate::movepart::relocation_ownership(&src, grow_part)? {
                        return Err(Self::busy(&active));
                    }
                    true
                }
            }
        };
        Self::attach_journal(&mut src)?;
        Ok((src, resumed))
    }

    fn attach_journal(src: &mut FileSource) -> Result<(), Fail> {
        let p = src.identity.journal_path().to_path_buf();
        src.journal = Some(
            Journal::open(&p).map_err(|e| Fail::infra(format!("journal open failed: {e}")))?,
        );
        Ok(())
    }

    /// 目标被一件没做完的事占着。**出路必须按现场性质分三路**，给同一句话就是把用户
    /// 送进一条注定失败的路：
    /// - 有 ckpt ⇒ 那件事**还能接着做**（重跑原命令即续跑）
    /// - 无 ckpt、journal 未越过不可回滚点 ⇒ 可以**回滚**
    /// - 无 ckpt、journal 已越过（含读不出来的）⇒ 既续不了也回滚不了，只有 `abandon` 能释放
    ///
    /// 第三路是真实存在的：`copy` 在数据复制完后、表项提交前崩溃就没有 ckpt，
    /// 此时的 journal 带着屏障——`undo` 会拒绝，说"用 undo 回滚"是错的
    fn busy(active: &[RecoveryRecord]) -> Fail {
        let listed: Vec<String> = active.iter().map(|r| r.path().display().to_string()).collect();
        let resumable = active.iter().any(|r| matches!(r, RecoveryRecord::Checkpoint { .. }));
        let rollbackable = active.iter().any(|r| match r {
            RecoveryRecord::Journal { entries: Some(v), .. } => {
                v.iter().all(|e| !matches!(e.recovery, dev::RecoveryData::Barrier))
            }
            _ => false,
        });
        let way_out = if resumable {
            // "重跑原命令即续跑"只对 relocation 作业成立；单分区 resize 的现场
            //（RecoveryRecord 只有路径，分不出两族）必须把另一条出路一并给出
            "re-run the command that started it to continue (an unfinished single-partition \
             resize is resumed with `resize-part` or `move`), or release it with `diskedit abandon`"
        } else if rollbackable {
            "roll it back with `diskedit undo`, or release it with `diskedit abandon`"
        } else {
            "it is already past the point of rolling back, so `diskedit abandon` is the only way to release it"
        };
        Fail::infra(format!(
            "an unfinished operation still owns this target ({}); {way_out}",
            listed.join(", ")
        ))
    }

    /// 开一次**只持有所有权、不建 journal** 的写事务。两类调用方：
    /// - `undo`：它要回放的是**已经存在**的那份 journal，不能自己去建一份
    /// - `check` / `resizefs`：它们动手前先过闸口，动手时才由自己落记录
    pub(crate) fn begin_without_history(a: &Args) -> Result<FileSource, Fail> {
        Self::open_rw(a)
    }

    /// 只读打开：**不取所有权**。它不改变目标，所以既不与别的写者互斥，也不该被别人的
    /// 写挡住——`resize` 这类命令的只读阶段正是在自己随后要取锁之前调它。
    ///
    /// 块设备走只读打开：RW+O_EXCL 在盘被 claim 时会被内核拒绝，分区被占用会连同整盘
    /// 一起被 claim，而 `info` / `plan` 恰恰可能被用来查看一块正被使用的盘。
    /// 镜像同样真只读：调用面已核清无一处写盘，只读镜像/介质不被拒之门外
    pub(crate) fn read_only(a: &Args) -> Result<FileSource, Fail> {
        #[cfg(target_os = "linux")]
        if let Ok(meta) = std::fs::metadata(&a.target)
            && meta.file_type().is_block_device()
        {
            return FileSource::open_read_only(Path::new(&a.target))
                .map_err(|e| Fail::infra(format!("open failed: {e}")));
        }
        FileSource::open_read_only_image(Path::new(&a.target), a.sector_size)
            .map_err(|e| Fail::infra(format!("open failed: {e}")))
    }

    /// 读写打开、不取所有权
    fn open_plain(a: &Args) -> Result<FileSource, Fail> {
        FileSource::open(Path::new(&a.target), a.sector_size)
            .map_err(|e| Fail::infra(format!("open failed: {e}")))
    }

    /// 读写打开 + 取目标的独占所有权（见 `targetlock`）。
    ///
    /// 刻意**不对外**：命令层拿不到"不取锁就能读写目标"的路径，"写命令必须持锁"
    /// 于是由可见性保证，而不是靠每处记得调用哪一个
    fn open_rw(a: &Args) -> Result<FileSource, Fail> {
        let mut src = Self::open_plain(a)?;
        src.ownership = Some(TargetLock::acquire(&src.identity)?);
        Ok(src)
    }

    /// 提交：事务完成，关闭它的 active 状态。删的是本次身份的全部候选落点——含历史命名
    /// 那一份：留着它会被下次查找命中，把历史字节回放到一个已经改过的盘上。
    /// 不存在即无残留、无告警；删除真失败则由 `dev::warn_if_remove_failed` 告警
    pub(crate) fn commit(a: &Args) {
        let Some(id) = TargetIdentity::resolve_path(Path::new(&a.target)) else {
            eprintln!("warning: cannot re-resolve the target identity — the undo journal is left in place");
            return;
        };
        for p in id.journal_candidates() {
            dev::warn_if_remove_failed(p);
        }
    }

    /// 目标上还活着的恢复现场
    pub(crate) fn active_records(src: &FileSource) -> Vec<RecoveryRecord> {
        active_recovery_records(&src.identity, legacy_disk_guid(src))
    }

    /// 绕过事务入口的写盘命令（`resizefs` 与 `check` 的修复）动手前的唯一闸口：目标上若还
    /// 留着上一次操作的恢复现场，说明那件事没做完——此时扩 FS 或按修复结果写表会让那份
    /// journal 所指的旧布局彻底作废，而用户随后 undo 仍会把旧表字节回放上去，形成"表与盘上
    /// 内容自相矛盾"的状态（例如 FS 自述尺寸 > 分区尺寸）。
    ///
    /// 经过 [`Self::begin`] 的写命令（add/del/resize/mkfs…）不靠它：`begin` 在目标被占用时
    /// 就已拒绝。本闸口覆盖的是只取所有权、不开事务的那些命令
    pub(crate) fn refuse_if_active(src: &FileSource, what: &str) -> Result<(), Fail> {
        let active = Self::active_records(src);
        if active.is_empty() {
            return Ok(());
        }
        // 与 `begin` 同一句话（`busy`），只多一句本文命令为什么绕不开它
        Err(Self::busy(&active).context(&format!("{what} writes outside the undo journal")))
    }
}