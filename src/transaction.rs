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

use std::num::NonZeroUsize;
use std::path::Path;
use std::sync::Mutex;

#[cfg(target_os = "linux")]
use std::os::unix::fs::FileTypeExt;

use crate::args::Args;
use crate::dev::{FileSource, Journal, PartSelector, TargetIdentity, WriteSource};
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
/// journal 的判据集中在 [`dev::classify_journal`]（absent / shell / damaged / records 四态）：
/// 有记录或读不出来 ⇒ 活着；空壳与 0 字节残骸 ⇒ 不是现场。`undo` 的候选挑选与 `abandon`
/// 的枚举都消费同一个分类，三处对"什么算现场"不会给出不同答案。
///
/// checkpoint 没有"空壳"一说：它一落盘就描述一次中断的搬移，故只看存在性——
/// **只有 `NotFound` 算不存在**，属性读取的其他错误按存在处理
pub(crate) fn active_recovery_records(
    identity: &TargetIdentity,
    legacy_disk_guid: Option<[u8; 16]>,
) -> Vec<RecoveryRecord> {
    let mut out = Vec::new();
    for p in identity.journal_candidates() {
        match dev::classify_journal(p) {
            dev::JournalCandidate::Absent | dev::JournalCandidate::Shell => {}
            dev::JournalCandidate::Damaged(_) => {
                out.push(RecoveryRecord::Journal { path: p.clone(), entries: None });
            }
            dev::JournalCandidate::Records(read) => {
                let entries = match read {
                    dev::JournalRead::Complete(v) => v,
                    dev::JournalRead::TruncatedTail { records, .. } => records,
                };
                out.push(RecoveryRecord::Journal { path: p.clone(), entries: Some(entries) });
            }
        }
    }
    for p in identity.checkpoint_candidates(legacy_disk_guid) {
        // 目录等非普通文件不是本工具写下的任何东西，留着它会把一个无关路径顶成 busy 现场
        match std::fs::metadata(&p) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            // 读不出来不是"没有"：折叠成 absent 会让闸口在 state_dir 不可读时放行写命令
            Err(_) => out.push(RecoveryRecord::Checkpoint { path: p }),
            Ok(m) if !m.is_file() => continue,
            Ok(_) => out.push(RecoveryRecord::Checkpoint { path: p }),
        }
    }
    out
}

/// 事务的控制层。它不持状态——"这次事务开到哪了"的唯一事实就是盘上那份 journal
/// 与目标本身；本类型只提供入口，使生命周期只有一个地方可改
pub(crate) struct TransactionManager;

/// 本次进程内那次写事务的身份：**在事务开始时（锁下、目标句柄已打开）记下**，
/// `commit` 只消费它。收尾时不再按命令行路径重新解析——命令返回后路径可能已经指向另一个
/// 文件（改名或替换），重新解析会把清理落到别人的 journal 上。
///
/// `journal_fp` 是 attach 那一刻首项落点已存在时的文件指纹（不存在为 None）：事务开始时
/// 就存在的 journal 属于本次要续跑/收尾的作业；不存在的则由本次的记录创建（见
/// `dev::take_created_journal_fp`）。两个事实都缺席时 commit 不动任何候选——命令返回与
/// 重新取锁之间，另一进程可以在同一落点建出它自己的 journal，按路径或记录身份都分不开
/// 这两者
pub(crate) struct SessionJournal {
    pub(crate) identity: TargetIdentity,
    pub(crate) fingerprint: dev::TargetFingerprint,
    pub(crate) journal_fp: Option<dev::TargetFingerprint>,
    /// 事务开始时采纳的 journal 有撕裂尾时，被丢掉的字节数（`None` = 文件本就在记录边界
    /// 上）。那段记录没写完，对应的写入没有发生；但同样的形状也可能是记录被外部截掉的
    /// 残骸。两种情况都无从回放，故本次运行不能报成完整收尾（见 [`TransactionManager::commit`]）
    pub(crate) truncated: Option<NonZeroUsize>,
}

/// 一个进程只跑一条命令，因此单槽位足够；`begin_or_resume` 是唯一的写入点
static TRANSACTION_IDENTITY: Mutex<Option<SessionJournal>> = Mutex::new(None);

fn session_slot() -> std::sync::MutexGuard<'static, Option<SessionJournal>> {
    TRANSACTION_IDENTITY.lock().unwrap_or_else(|e| e.into_inner())
}

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
    /// 占着时拒绝——本入口续不了它，出路是 `resize-part` / `move`。
    /// 带的是分区选择器而非具体号：`:last` 落到具体号要读表，而那一刻目标刚在锁下打开
    OwnRelocation(PartSelector),
}

/// `--sector-size` 只对镜像有意义：镜像不携带扇区信息，故由用户给出。块设备的逻辑扇区
/// 大小由内核经 BLKSSZGET 给出，覆盖值被 `FileSource::open` 静默忽略——静默接受一个不
/// 生效的旗标，等于让用户以为扇区大小改了。块设备上显式给出即拒绝（改参数有解，退 10）
fn reject_sector_size_on_block(a: &Args, src: &FileSource) -> Result<(), Fail> {
    if src.is_block && a.sector_size.is_some() {
        return Err(Fail::refused(
            "--sector-size applies to image files only; a block device's logical sector size is reported by the kernel (BLKSSZGET) and cannot be overridden",
        ));
    }
    Ok(())
}

impl TransactionManager {
    /// 开一次**新的**写事务：读写打开、取独占所有权、把 journal 接到目标上。
    ///
    /// 目标上已有 active transaction 时**拒绝**（30）：一个普通 mutation 永远不能隐式
    /// 接管别人没做完的事务。接着做要走 [`Self::begin_or_resume`]，放弃要走 `abandon`——
    /// 让 `begin` 去猜"这是不是续跑"，必然导致"是不是续跑"要在两处保持一致
    ///
    /// journal 是**惰性**的：`Journal::open` 在文件不存在时什么都不建，采纳既有文件时
    /// 只做校验与撕裂尾的物理截断（截掉的是设计认定可丢弃的未完成追加，见其文档）；
    /// 真正的记录要等到第一次真正的写入（见 [`WriteSource::write_at`]）。
    /// 因此一条在校验阶段就被拒的命令不会留下空 journal，也就不会占住候选落点
    pub(crate) fn begin(a: &Args) -> Result<WriteSource, Fail> {
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
    ) -> Result<(WriteSource, bool), Fail> {
        let mut src = Self::open_rw(a)?;
        Self::refuse_unidentifiable(&src)?;
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
                ResumeClaim::OwnRelocation(r) => {
                    let grow_part = crate::gpt_policy::resolve_part(&src, r)?;
                    if !crate::movepart::relocation_ownership(&src, grow_part)? {
                        return Err(Self::busy(&active));
                    }
                    true
                }
            }
        };
        let (journal_fp, truncated) = Self::attach_journal(&mut src)?;
        // 身份在锁下、与已打开的句柄同一时刻定稿；`commit` 只消费这一个值
        *session_slot() = Some(SessionJournal {
            identity: src.identity.clone(),
            fingerprint: src.fingerprint,
            journal_fp,
            truncated,
        });
        Ok((src, resumed))
    }

    /// 没有稳定身份证据的目标不能开事务：journal/ckpt 的头会把 `Unavailable` 记成
    /// "哪一类字段才是身份证据"，而恢复端无法据此证明"还是那个目标"——写下的现场既
    /// 不能续跑也不能回滚。故在任何字节落盘之前拒绝，而不是等恢复时才发现已经无路可走
    fn refuse_unidentifiable(src: &FileSource) -> Result<(), Fail> {
        if matches!(src.scene().proof, dev::LiveProof::Unavailable) {
            return Err(Fail::refused(
                "this target exposes no stable identity (no device ID and no file identity): an interrupted write \
                 here could neither be resumed nor undone, so it is refused before any byte is written",
            ));
        }
        Ok(())
    }

    /// 返回 attach 时首项落点已存在时的文件指纹（见 [`SessionJournal`]），以及采纳时
    /// 被截掉的残尾字节数（`None` = 文件本就在记录边界上）。
    /// 属性读不出来时不认领它：紧随其后的 `open` 若也读不到，命令在写盘之前就已返回错误
    fn attach_journal(src: &mut WriteSource) -> Result<(Option<dev::TargetFingerprint>, Option<NonZeroUsize>), Fail> {
        let p = src.identity.journal_path().to_path_buf();
        // 指纹一律取自句柄（Windows 的文件身份只能从句柄问）；打不开就当"没有证据"，
        // 于是 commit 不会凭路径删它
        let adopted = std::fs::File::open(&p)
            .ok()
            .and_then(|f| dev::TargetFingerprint::from_file(&f).ok());
        // 身份随头落盘：采纳既有 journal 时 `open` 会拿它核对（见 `dev::scene_mismatch`），
        // 故新开的窗口也记的是"这个目标"
        let identity = src.scene();
        let (journal, truncated) = Journal::adopt(&p, identity)
            .map_err(|e| Fail::infra(format!("journal open failed: {e}")))?;
        src.adopt_journal(journal);
        Ok((adopted, truncated))
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
        // 恰好一份 journal 且它可回滚才给这条出路：两份并存时 undo 的 pick_journal
        // 会因无法抉择而拒绝（Ambiguous），按其中一份许诺"可以 undo"是句到不了的出路
        // 与 `pick_journal` 同分母：只有"读出来且非空"的 journal 参与计数；损坏候选
        // （entries: None）既不否决一份可用的记录，也不被当成第二份
        let usable: Vec<&Vec<dev::JournalRecord>> = active
            .iter()
            .filter_map(|r| match r {
                RecoveryRecord::Journal { entries: Some(v), .. } => Some(v),
                _ => None,
            })
            .collect();
        let single_rollbackable = usable.len() == 1
            && usable[0].iter().all(|e| !matches!(e.recovery, dev::RecoveryData::Barrier));
        let way_out = if resumable {
            // "重跑原命令即续跑"只对 relocation 作业成立；单分区 resize 的现场
            //（RecoveryRecord 只有路径，分不出两族）必须把另一条出路一并给出
            "re-run the command that started it to continue (an unfinished single-partition \
             resize is resumed with `resize-part` or `move`), or release it with `diskedit abandon`"
        } else if single_rollbackable {
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
    pub(crate) fn begin_without_history(a: &Args) -> Result<WriteSource, Fail> {
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
            let src = FileSource::open_read_only(Path::new(&a.target))
                .map_err(|e| Fail::infra(format!("open failed: {e}")))?;
            reject_sector_size_on_block(a, &src)?;
            return Ok(src);
        }
        FileSource::open_read_only_image(Path::new(&a.target), a.sector_size)
            .map_err(|e| Fail::infra(format!("open failed: {e}")))
    }

    /// 读写打开、不取所有权
    fn open_plain(a: &Args) -> Result<FileSource, Fail> {
        let src = FileSource::open(Path::new(&a.target), a.sector_size)
            .map_err(|e| Fail::infra(format!("open failed: {e}")))?;
        reject_sector_size_on_block(a, &src)?;
        Ok(src)
    }

    /// 读写打开 + 取目标的独占所有权（见 `targetlock`），返回**写句柄**。
    ///
    /// 三种写事务的入口都在本模块（`begin_or_resume` 的三个 claim），它们都经这里取
    /// 所有权；写方法只挂在 [`WriteSource`] 上（见 `dev::source`），而本函数是 crate 内
    /// 生产代码构造它的唯一途径——"写命令持锁"因此是类型保证，不是调用约定
    fn open_rw(a: &Args) -> Result<WriteSource, Fail> {
        WriteSource::acquire(Self::open_plain(a)?)
    }

    /// 提交：事务完成，关闭它的 active 状态。删除的是**本次事务确实拥有的**候选落点。
    ///
    /// `session` 来自 [`Self::begin_or_resume`]（见 [`Self::take_identity`]），
    /// **不得在这里按命令行路径重新解析**：命令返回时路径可能已被改名或替换，重新解析
    /// 会得到另一个文件的伴随文件，删掉的就是别人的回滚凭据。
    ///
    /// 首项落点的归属取两个事实之一：attach 时它已存在（要收尾的是那份既有 journal），
    /// 或本次进程用 `create_new` 建过它（`dev::take_created_journal_fp`）。两者都没有时，
    /// 路径上出现的文件必是命令返回之后别的进程新建的，一律不动。归属成立也只说明
    /// **路径**属于本次事务：删除前还要按当时的文件指纹核对 `dev` / `ino`——同路径文件
    /// 可以被 abandon 改名、再由另一进程新建，而两份 journal 记录的目标身份完全相同，
    /// 重读内容分不开它们
    ///
    /// 命令返回时 [`FileSource`]（连同它的所有权）已释放，故这里按同一身份重新取一次锁：
    /// 删除必须与 journal 的创建落在同一个独占窗口里。取不到锁即保留 journal
    /// （漏删会被下次运行报成 busy，但不会丢失别人的现场）
    /// 返回 `true` 表示现场已按本次事务的意图收干净；`false` 表示有文件该删而没删掉
    /// （没取到锁，或认不出它是否仍属于本次事务），或者采纳过的 journal 带着无法回放的
    /// 残尾——三种都属于"本次运行没有完全收尾"，调用方必须据此降级退出码，而不是照常给 0
    pub(crate) fn commit(session: &SessionJournal) -> bool {
        let identity = &session.identity;
        let mut clean = true;
        // 采纳的 journal 带着撕裂尾：那截字节不构成可回放记录，已被丢弃；但它对应的
        // 写入既不能回放也不能排除，而删掉 journal 之后这份不确定性再没有第二个载体
        if let Some(cut) = session.truncated {
            eprintln!(
                "warning: the journal this transaction resumed ended in {cut} byte(s) that form no complete record — they were discarded, and whatever was recorded after the last complete record cannot be replayed"
            );
            clean = false;
        }
        // 重新取锁后目标归本进程独占，下面两个动作（删除与目录落盘）才不会被别人的
        // 新建动作穿插；锁失败只影响"现在能不能关窗口"，不是数据错误
        let _lock = match TargetLock::acquire(identity, Some(&session.fingerprint)) {
            Ok(lock) => lock,
            Err(f) => {
                let why = match f {
                    Fail::Refused(m) | Fail::Infra(m) | Fail::Failed(m) => m,
                };
                eprintln!(
                    "warning: cannot re-take the target lock to drop the undo journal ({why}) — the journal is left in place"
                );
                return false;
            }
        };
        let primary = identity.journal_path().to_path_buf();
        // 两条证据，强度不同，删除要分别对待：
        // - 采纳（`session.journal_fp`）：文件在事务开始前就存在，只能靠文件身份认它；
        // - 自建（`CREATED_JOURNAL`）：本次运行 `create_new` 成功占下的路径，所有权来自那一刻
        //   而不是路径字符串——没有文件身份接口的平台（Windows）也只有这一条可用
        let created_here = dev::take_created_journal_fp(&primary);
        let ours_file = session.journal_fp.or(created_here);
        // 首项的**删除**只依据文件身份（或自建的所有权）；内容身份只用于比对其余候选
        let mut remove_primary = false;
        let mut ours_identity = None;
        if let Some(fp) = ours_file {
            let delete_primary = match fp.still_names(&primary) {
                Ok(Some(true)) => true,
                // 身份无从比对，但这份文件是本进程本次运行建出来的：删它依据的是所有权
                Ok(None) if created_here.is_some() => true,
                // 认不出这份文件是否仍属于本次事务：不删是 fail-safe，但"该收的没收掉"
                // 必须如实计入——否则本次运行按成功退出，这份 journal 却在下一轮把目标
                // 顶成 busy
                Ok(Some(false)) => {
                    eprintln!(
                        "warning: {} no longer names the journal this transaction opened — left in place",
                        primary.display()
                    );
                    clean = false;
                    false
                }
                Ok(None) => {
                    eprintln!(
                        "warning: no file identity was recorded for {} — left in place rather than removed on the strength of its path",
                        primary.display()
                    );
                    clean = false;
                    false
                }
                Err(e) => {
                    eprintln!("warning: cannot check the identity of {} ({e}) — left in place", primary.display());
                    clean = false;
                    false
                }
            };
            if delete_primary {
                remove_primary = true;
                match dev::Journal::read_identity(&primary) {
                    Ok(id) => ours_identity = Some(id),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => eprintln!(
                        "warning: cannot read the identity of {} ({e}) — only that file is removed",
                        primary.display()
                    ),
                }
            }
        }
        for p in identity.journal_candidates() {
            if *p == primary {
                if remove_primary && !dev::remove_recovery_file(p) {
                    eprintln!("warning: could not remove {} — it still names this target", p.display());
                    clean = false;
                }
                continue;
            }
            let Some(ours_identity) = &ours_identity else { continue };
            match dev::Journal::read_identity(p) {
                Ok(id) => {
                    if let Some(why) = dev::recorded_scenes_mismatch(&id, ours_identity) {
                        eprintln!("warning: {why} — {} is left in place", p.display());
                    } else if !dev::remove_recovery_file(p) {
                        eprintln!("warning: could not remove {} — it still names this target", p.display());
                        clean = false;
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => eprintln!(
                    "warning: cannot read the identity of {} ({e}) — left in place",
                    p.display()
                ),
            }
        }
        clean
    }

    /// 取走事务开始时记下的身份。没有走到 `begin_or_resume`（或它没成功）时为 None——
    /// 此时没有本进程开的撤销窗口可关
    pub(crate) fn take_identity() -> Option<SessionJournal> {
        session_slot().take()
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
        Self::refuse_if_active_at(src, src, what)
    }

    /// 现场身份与 Disk GUID 取自不同节点时的入口。在线 PV 路径按**分区**节点枚举现场
    /// （`from_block_keys` 在 self_key != disk_key 时同时列出盘级键与旧版按分区命名的
    /// 历史候选，这是**唯一**会列出后者的入口：undo / abandon 都要求整盘目标，整盘身份
    /// 的 self_key 与 disk_key 相同，旧版按分区键落的现场因此不在它们的候选里），而历史
    /// GUID 命名的 checkpoint 只能从**整盘**的表读——分区节点的 0 偏移不是 GPT 头
    pub(crate) fn refuse_if_active_at(
        identity: &FileSource,
        disk: &FileSource,
        what: &str,
    ) -> Result<(), Fail> {
        Self::refuse_unidentifiable(identity)?;
        let active = active_recovery_records(&identity.identity, legacy_disk_guid(disk));
        if active.is_empty() {
            return Ok(());
        }
        // 与 `begin` 同一句话（`busy`），只多一句本文命令为什么绕不开它
        Err(Self::busy(&active).context(&format!("{what} writes outside the undo journal")))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::let_underscore_must_use)] // 清理临时文件有意忽略失败
    use super::*;
    use crate::dev::{JournalRecord, Mutation, RecoveryData};

    /// 一份 journal 现场：`barrier` 决定它是否已越过不可回滚点
    fn journal(path: &str, barrier: bool) -> RecoveryRecord {
        RecoveryRecord::Journal {
            path: path.into(),
            entries: Some(vec![JournalRecord {
                mutation: Mutation::PartitionTable,
                recovery: if barrier {
                    RecoveryData::Barrier
                } else {
                    RecoveryData::PreImage { off: 0, bytes: vec![0u8; 4], after: vec![0u8; 4] }
                },
            }]),
        }
    }

    fn way_out(active: &[RecoveryRecord]) -> String {
        match TransactionManager::busy(active) {
            Fail::Infra(m) => m,
            other => panic!("busy must report the gate as Infra: {other:?}"),
        }
    }

    /// 出路按现场性质分三路，给同一句话就是把用户送进注定失败的路。undo 这条出路
    /// 要求**恰好一份可读 journal**：多份并存时 `pick_journal` 会因无法抉择而拒绝
    /// （Ambiguous），凭其中一份许诺"可以 undo"是句到不了的出路；读不出来的候选则
    /// 只告警、不否决那一份可用记录
    #[test]
    fn busy_offers_undo_only_when_a_single_usable_journal_is_rollbackable() {
        let text = way_out(&[journal("a", false)]);
        assert!(text.contains("undo"), "a rollbackable journal must offer undo: {text}");

        let text = way_out(&[journal("a", true)]);
        assert!(!text.contains("undo"), "a journal past the barrier must not offer undo: {text}");
        assert!(text.contains("abandon"), "{text}");

        // 两份并存、其一已越过不可回滚点：另一份可回滚也不足以许诺 undo
        let text = way_out(&[journal("a", false), journal("b", true)]);
        assert!(!text.contains("undo"), "one un-rollbackable journal must withdraw the undo way out: {text}");
        assert!(text.contains("abandon"), "{text}");

        // 两份均可回滚也不许诺 undo：pick_journal 对两份可用 journal 必拒 Ambiguous
        let text = way_out(&[journal("a", false), journal("b", false)]);
        assert!(!text.contains("undo"), "two journals leave undo no single choice: {text}");
        assert!(text.contains("abandon"), "{text}");

        // 一份可用 + 一份读不出来：pick_journal 按可用那份回放，损坏候选只告警，
        // 因此它不参与计数、也不否决 "undo"
        let damaged = RecoveryRecord::Journal { path: "b".into(), entries: None };
        let text = way_out(&[journal("a", false), damaged]);
        assert!(text.contains("undo"), "a damaged candidate must not withdraw the undo way out: {text}");
    }

    /// commit 只清**本次事务拥有**的候选：同一条命令行路径在不同时刻可以指向不同文件，
    /// 收尾时按路径重新解析会删到别人的 journal。这里用两个不同身份证明清理范围
    #[test]
    fn commit_drops_the_candidates_of_the_given_identity_only() {
        let dir = std::env::temp_dir().join(format!("diskedit_commitid_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let img = dir.join("a.img");
        std::fs::write(&img, b"x").unwrap();
        let id = TargetIdentity::resolve_image(&img);
        let mut mine = Vec::new();
        for p in id.journal_candidates() {
            // 头完整、零记录的空壳：read_identity 可读，身份与本次事务一致
            let mut j = dev::Journal::open(
                p,
                dev::LiveScene { fp: Default::default(), map: None, proof: dev::LiveProof::Inode },
            )
            .unwrap();
            j.ensure().unwrap();
            mine.push(p.clone());
        }
        let other = TargetIdentity::resolve_image(&dir.join("b.img"));
        std::fs::write(other.journal_path(), b"keep").unwrap();

        // 进程级的"本次运行自建"槽位只记最后一个 `ensure` 的文件；它是另一条独立的
        // 所有权证据，本用例要检验的是"采纳来的指纹"这一条路径，故先把它清掉
        let _ = dev::take_created_journal_fp(id.journal_path());

        // 首项要按"attach 时就是它"认领，指纹取该文件此刻的身份（从**句柄**问：
        // 有的平台只在句柄上有文件身份）
        let adopted = std::fs::File::open(id.journal_path())
            .ok()
            .and_then(|f| dev::TargetFingerprint::from_file(&f).ok());
        let has_identity = adopted.as_ref().is_some_and(|fp| fp.has_inode_evidence());
        TransactionManager::commit(&SessionJournal {
            identity: id.clone(),
            fingerprint: Default::default(),
            journal_fp: adopted,
            truncated: None,
        });

        // 交上去的指纹取自该文件此刻的身份：有证据即删；平台给不出文件身份时
        // "属于本次事务"没有可交付的判据，必须留下
        for p in &mine {
            if has_identity {
                assert!(!p.exists(), "{} belongs to the committed transaction and must be dropped", p.display());
            } else {
                assert!(p.exists(), "without file identity evidence an adopted journal must stay: {}", p.display());
            }
        }
        assert!(other.journal_path().exists(), "another target's journal must survive the commit");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// attach 时首项不存在、本进程也没建过它：路径上出现的文件是命令返回后别的进程
    /// 建出的，commit 必须原样留下——否则会删掉一个刚开的事务的回滚凭据
    #[test]
    fn commit_keeps_a_journal_it_neither_adopted_nor_created() {
        let dir = std::env::temp_dir().join(format!("diskedit_commitrace_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let img = dir.join("a.img");
        std::fs::write(&img, b"x").unwrap();
        let id = TargetIdentity::resolve_image(&img);
        let p = id.journal_path().to_path_buf();
        std::fs::write(&p, b"DEJL\x05").unwrap();

        TransactionManager::commit(&SessionJournal {
            identity: id.clone(),
            fingerprint: Default::default(),
            journal_fp: None,
            truncated: None,
        });

        assert!(p.exists(), "a journal created after this transaction returned must survive");

        let _ = std::fs::remove_dir_all(&dir);
    }
}