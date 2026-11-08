//! 事务的控制层：目标独占所有权、生命周期、活动现场发现，以及"未收尾时谁能继续"的闸口。
//!
//! durable history 由单文件恢复记录承担——本层**不新增第二份持久化事实**，
//! 只是把"谁在独占目标、这次事务算不算开着、开着时谁能动手"这三件事收到一处：
//!
//! ```text
//! begin  = 打开目标 + 取独占所有权 + 在首次写盘前建记录
//! commit = 事务完成 ⇒ 记录被清理（由收尾路径负责）
//! active = 记录落点在、且可解码 ⇒ 只有 undo / abandon / 续跑原命令能继续碰这个目标
//! ```
//!
//! **不保存 completion_state**：能不能算"完成"由记录与盘上状态共同决定；
//! 本层只保管盘上推导不出来的东西（独占所有权），不复制一份可能与盘上分叉的副本

use std::path::Path;

#[cfg(target_os = "linux")]
use std::os::unix::fs::FileTypeExt;

use crate::cli::TargetRequest;
use crate::dev::{self, FileSource, TargetIdentity, WriteSource};
use crate::outcome::RunOutcome;

/// 一份**还活着的**恢复现场。它是"目标上还有什么没做完"的唯一枚举口径：闸口
/// （拒绝别的写命令）与 `abandon`（放弃它）都从这里取，两处不各自判断"什么算现场"
pub(crate) struct RecoveryRecord {
    /// 记录落点（镜像侧 `<image>.diskedit/record`，块设备侧
    /// `<state_root>/recovery/<identity-hash>/record`）
    pub(crate) path: std::path::PathBuf,
    /// `probe` 只回答"落点上是什么"；身份由 `undo` / `abandon` 在打开目标后按当前场景再比
    pub(crate) probe: Box<crate::recovery::CompanionProbe>,
}

impl RecoveryRecord {
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// 报告用的单条描述
    fn describe(&self) -> String {
        let path = &self.path;
        match self.probe.as_ref() {
            crate::recovery::CompanionProbe::Decoded(record) => format!(
                "{} (recovery record: {:?}, {} written range(s))",
                path.display(),
                record.op,
                record.restore.len()
            ),
            crate::recovery::CompanionProbe::Damaged(why) => {
                format!("{} (recovery record, unreadable: {why})", path.display())
            }
            crate::recovery::CompanionProbe::Foreign => {
                format!("{} (recovery record layout not recognized by this build)", path.display())
            }
            crate::recovery::CompanionProbe::NotOurs(shape) => {
                format!("{} ({shape}, not a recovery file)", path.display())
            }
            crate::recovery::CompanionProbe::Unreadable(why) => {
                format!("{} (recovery record, cannot be read: {why})", path.display())
            }
            crate::recovery::CompanionProbe::Absent => path.display().to_string(),
        }
    }

    /// 外来条目的拒绝文案；不是这一类时返回 None
    pub(crate) fn foreign_message(&self) -> Option<String> {
        match self.probe.as_ref() {
            crate::recovery::CompanionProbe::NotOurs(shape) => Some(format!(
                "{} is {shape}, not a recovery file written by this tool",
                self.path.display()
            )),
            _ => None,
        }
    }
}

/// 枚举目标上还活着的恢复现场。
///
/// 判据只看记录落点：只有普通文件且可解码（或可被识别为损坏 / 布局不认识 / 外来文件）才算
/// 现场；缺席不是。旧 journal 与旧 checkpoint 不在这里认领——它们不是记录，不进本枚举，
/// 也不阻塞任何命令（不认领、不改名、不删除、不进扫描）。
pub(crate) fn active_recovery_records(identity: &TargetIdentity) -> Vec<RecoveryRecord> {
    let mut out = Vec::new();
    // 单文件记录只在镜像上有已定的落点；块设备的记录状态根未定，写路径在迁移完成前拒绝
    if let Some(path) = identity.record_path() {
        let probe = crate::recovery::probe_companion(path);
        if !matches!(probe, crate::recovery::CompanionProbe::Absent) {
            out.push(RecoveryRecord { path: path.to_path_buf(), probe: Box::new(probe) });
        }
    }
    out
}

/// 事务的控制层。它不持状态——"这次事务开到哪了"的唯一事实就是盘上那份记录
/// 与目标本身；本类型只提供入口，使生命周期只有一个地方可改
pub(crate) struct TransactionManager;

/// `resize` 的进入现场。单分区 resize 与搬移作业（`--allow-move`）同属一份记录：
/// `resumed` 表示落点上就是本命令未收尾的记录，搬移状态则由记录的
/// [`crate::recovery::Relocation`] 给出
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct ResizeEntry {
    pub(crate) resumed: bool,
}

/// `--sector-size` 只对镜像有意义：镜像不携带扇区信息，故由用户给出。块设备的逻辑扇区
/// 大小由内核经 BLKSSZGET 给出，覆盖值不参与打开——静默接受一个不生效的旗标，等于让
/// 用户以为扇区大小改了。块设备上显式给出即拒绝（前置条件不满足，退 3）
fn reject_sector_size_on_block(a: &impl TargetRequest) -> Result<(), RunOutcome> {
    if a.sector_size().is_some() {
        return Err(RunOutcome::refused(
            "--sector-size applies to image files only; a block device's logical sector size is reported by the kernel (BLKSSZGET) and cannot be overridden",
        ));
    }
    Ok(())
}

/// 目标是否为块设备。只看路径元数据、不打开目标：`--sector-size` 的适用范围要在打开
/// 之前定，否则盘被占用时先报 open 失败（4），用法错误（2）就推不出来
#[cfg(target_os = "linux")]
fn target_is_block(path: &str) -> bool {
    std::fs::metadata(path).is_ok_and(|m| m.file_type().is_block_device())
}

/// 非 Linux 没有块设备节点这种目标形态
#[cfg(not(target_os = "linux"))]
fn target_is_block(_path: &str) -> bool {
    false
}

impl TransactionManager {
    /// 没有稳定身份证据的目标不能开事务：记录头会把 `Unavailable` 记成
    /// "哪一类字段才是身份证据"，而恢复端无法据此证明"还是那个目标"——写下的现场既
    /// 不能续跑也不能回滚。故在任何字节落盘之前拒绝，而不是等恢复时才发现已经无路可走。
    /// "身份不可证明"归入 6：换参数或重试都不会让目标长出身份
    fn refuse_unidentifiable(src: &FileSource) -> Result<(), RunOutcome> {
        if matches!(src.scene().proof, dev::LiveProof::Unavailable) {
            return Err(RunOutcome::unsupported(
                "this target exposes no stable identity (no device ID and no file identity): an interrupted write \
                 here could neither be resumed nor undone, so it is refused before any byte is written",
            ));
        }
        Ok(())
    }

    /// 目标被一件没做完的事占着。**出路必须按现场性质分路**，给同一句话就是把用户
    /// 送进一条注定失败的路：
    /// - 记录可续跑 ⇒ 重跑原命令
    /// - 记录未越过不可回滚点 ⇒ 可以回滚（`undo`）
    /// - 其余（越过回滚点、损坏、布局不认识、外来文件）⇒ 只有 `abandon` 能释放
    ///
    /// 退出码按证据类别分：`Recognized + Provisional` ⇒ 5（先于"目标忙"）；
    /// 其余类别 ⇒ 3。`Provisional` 表示记录尚未标为提交，盘上字节可能已部分写入
    ///
    /// 现场里只剩记录一类：旧 journal 与旧 checkpoint 不参与枚举，也不阻塞命令
    fn busy(active: &[RecoveryRecord]) -> RunOutcome {
        let listed: Vec<String> = active.iter().map(RecoveryRecord::describe).collect();
        let rollbackable = active
            .iter()
            .find_map(|r| match r.probe.as_ref() {
                // move/copy 的数据搬移越过不可逆点后，表区仍在 preimage 里，
                // undo 仍可回写——"可否撤销"因此按操作类型判，不只看屏障位
                crate::recovery::CompanionProbe::Decoded(record) => Some(
                    !record.restore_blocked
                        || matches!(record.op, crate::recovery::OpKind::Move | crate::recovery::OpKind::Copy),
                ),
                _ => None,
            })
            .unwrap_or(false);
        // 只有一份记录时才谈得上"重跑哪个命令"；多份记录没有可唯一指认的请求
        let recorded = active.iter().find_map(|r| match r.probe.as_ref() {
            crate::recovery::CompanionProbe::Decoded(record) if active.len() == 1 => Some(record),
            _ => None,
        });
        let resumable = recorded.is_some_and(|r| r.op.resumable());
        let foreign_format = active.iter().find_map(|r| match r.probe.as_ref() {
            crate::recovery::CompanionProbe::Foreign => {
                Some("the recovery record there uses a layout this build does not recognize".to_string())
            }
            _ => None,
        });
        let foreign = active.iter().find_map(|r| match r.probe.as_ref() {
            crate::recovery::CompanionProbe::NotOurs(shape) => Some(shape.as_str()),
            _ => None,
        });
        // `Recognized + Provisional` 先于"目标忙"判定，落 5 而非 3
        let provisional = active.iter().any(|r| {
            matches!(
                r.probe.as_ref(),
                crate::recovery::CompanionProbe::Decoded(record)
                    if record.commit == crate::recovery::CommitState::Prepared
            )
        });
        let way_out: String = if let Some(what) = foreign {
            // 本工具不会删除或改名不是恢复文件的条目：它不是本工具写下的任何东西
            format!(
                "the recovery path holds {what}, which is not a recovery file written by this tool; no diskedit \
                 command will delete or rename it — move or remove it by hand, then retry"
            )
        } else if let Some(record) = recorded {
            // 文案必须与真实出路一致：能续跑的命令才说"重跑"，不能续跑的命令不许给这条指引
            let cmd = record.op.command_name();
            if resumable && rollbackable {
                format!(
                    "re-run `diskedit {cmd}` with the same request to continue it, roll it back with \
                     `diskedit undo`, or release it with `diskedit abandon`"
                )
            } else if resumable {
                format!(
                    "re-run `diskedit {cmd}` with the same request to continue it, or release it with \
                     `diskedit abandon`"
                )
            } else if rollbackable {
                "this operation has no resume path; roll it back with `diskedit undo`, or release it \
                 with `diskedit abandon`"
                    .to_string()
            } else {
                "this operation has no resume path and is already past the point of rolling back, so \
                 `diskedit abandon` is the only way to release it"
                    .to_string()
            }
        } else if rollbackable {
            "roll it back or settle it with `diskedit undo`, or release it with `diskedit abandon`".to_string()
        } else if let Some(what) = foreign_format {
            // 布局读不懂不是"越过回滚点"：说成越过回滚点会让用户以为 undo 有能力处理它
            format!("{what}; it is left in place, and `diskedit abandon` is the only way to release it")
        } else {
            "it is already past the point of rolling back, so `diskedit abandon` is the only way to release it"
                .to_string()
        };
        let report = format!(
            "an unfinished operation still owns this target ({}); {way_out}",
            listed.join(", ")
        );
        if provisional {
            // 5 的文案要说明目标可能已改变；证据路径在上面的列举里
            RunOutcome::partial(format!("{report} — the target may already have changed"))
        } else {
            RunOutcome::refused(report)
        }
    }

    /// 开一次**只持有所有权、不建记录**的写事务。两类调用方：
    /// - `undo`：它要回放的是**已经存在**的那份记录，不能自己去建一份
    /// - `check`：它动手前先过闸口，动手时才由自己落记录
    pub(crate) fn begin_without_history(a: &impl TargetRequest) -> Result<WriteSource, RunOutcome> {
        Self::open_rw(a)
    }

    /// 表类写命令的公共打开段：取所有权、定位记录落点、清残留临时文件。
    /// 现场判定与占用探测留给调用方——两者的先后是判据，不在这里调换
    fn record_target(a: &impl TargetRequest) -> Result<(WriteSource, std::path::PathBuf), RunOutcome> {
        let src = Self::open_rw(a)?;
        Self::refuse_unidentifiable(&src)?;
        // 第 24、25 条：块设备的记录目录由 WWN / EUI / serial 派生的 identity-hash
        // 决定。dm、md、只有 major:minor、或 serial 读数不一致的目标没有目录，
        // 写命令退 6，不降级到旧 journal
        let Some(record_path) = src.identity.record_path().map(Path::to_path_buf) else {
            return Err(RunOutcome::unsupported(
                "this block device exposes no WWN, EUI, or repeatable serial (dm / md / major:minor \
                 are not accepted as identity); the recovery record has no directory to live under, \
                 so table writes to this target stay refused",
            ));
        };
        // 记录目录（连同所有者与目录形状校验）已在
        // `WriteSource::acquire` 取锁前准备好，这里只使用它的落点
        // 锁已在本事务手上，先清掉本目标目录里的残留临时文件，
        // 它们不参与记录枚举与判定
        crate::recovery::sweep_record_temps(&record_path);
        Ok((src, record_path))
    }

    /// 记录头要用的两件事实：目标场景与盘身份。身份取自已打开的句柄
    /// （不是命令行的第二份解析），读不出时不在写盘前猜测（同旧 journal 的判据）
    fn record_scene(
        src: &WriteSource,
    ) -> Result<(crate::recovery::SceneIdentity, crate::dev::DiskGuidState), RunOutcome> {
        let probe = crate::platform::probe::probe_of(src).map_err(|e| {
            RunOutcome::refused(format!("cannot establish the target identity for the recovery record: {e:?}"))
        })?;
        let scene = crate::recovery::SceneIdentity::of_source(&probe, src);
        let credential = match crate::table::disk_guid_of(src) {
            Ok(Some(g)) => crate::dev::DiskGuidState::Present(g),
            Ok(None) => crate::dev::DiskGuidState::Unavailable,
            Err(e) => {
                return Err(RunOutcome::no_mutation(format!(
                    "the target's GPT disk identity cannot be read ({e}) — a recovery record written now \
                     could not identify this target later, so no byte is written"
                )))
            }
        };
        Ok((scene, credential))
    }

    /// 挂上写路径。
    ///
    /// 本次事务不建旧 journal（[`Self::begin_without_history`] 不采纳历史），于是写路径上
    /// 同时只有一条凭据。记录本身在第一次写盘前才落盘（见 [`crate::recovery::RecordJournal`]）：
    /// 写盘前就被拒的命令不会留下空记录占住落点
    pub(crate) fn begin_table_record(
        a: &impl TargetRequest,
        op: crate::recovery::OpKind,
    ) -> Result<WriteSource, RunOutcome> {
        let (mut src, record_path) = Self::record_target(a)?;
        let active = Self::active_records(&src);
        if !active.is_empty() {
            return Err(Self::busy(&active));
        }
        crate::dev::refuse_if_occupied(&src)?;
        let (scene, credential) = Self::record_scene(&src)?;
        let journal = crate::recovery::RecordJournal::create_image(
            &record_path,
            scene,
            op,
            src.sector_size,
            credential,
            src.owner,
        )
        .map_err(|e| RunOutcome::refused(format!("cannot write the recovery record beside the image: {e}")))?;
        src.adopt_record_journal(journal);
        Ok(src)
    }

    /// 可续跑的记录打开：落点上是**本命令**写下的完整记录时接着用它，否则等同严格打开。
    ///
    /// 只认"一条、操作类型相符"的记录。别的现场（多条记录、操作不符的记录、损坏或布局不认识）
    /// 一律按 busy 拒绝，既不删也不改——续跑不做接管
    pub(crate) fn begin_table_record_resume(
        a: &impl TargetRequest,
        op: crate::recovery::OpKind,
    ) -> Result<(WriteSource, bool), RunOutcome> {
        let (mut src, record_path) = Self::record_target(a)?;
        let active = Self::active_records(&src);
        let resumable = match active.as_slice() {
            [one] => match one.probe.as_ref() {
                crate::recovery::CompanionProbe::Decoded(record) => record.op == op,
                _ => false,
            },
            _ => false,
        };
        if !active.is_empty() && !resumable {
            return Err(Self::busy(&active));
        }
        crate::dev::refuse_if_occupied(&src)?;
        let (scene, credential) = Self::record_scene(&src)?;
        let journal = if resumable {
            let (journal, record) =
                crate::recovery::RecordJournal::open_image(&record_path, scene, src.sector_size, src.owner)
                    .map_err(|e| RunOutcome::refused(format!("cannot resume the recovery record: {e}")))?;
            // active 的判定已经核过操作类型；这里再核一次，防两处判据分叉
            if record.op != op {
                return Err(RunOutcome::refused(
                    "the existing recovery record belongs to a different operation — refusing to resume it",
                ));
            }
            journal
        } else {
            crate::recovery::RecordJournal::create_image(
                &record_path,
                scene,
                op,
                src.sector_size,
                credential,
                src.owner,
            )
            .map_err(|e| RunOutcome::refused(format!("cannot write the recovery record beside the image: {e}")))?
        };
        src.adopt_record_journal(journal);
        Ok((src, resumable))
    }

    /// `resize` 的入口。单分区 resize 与搬移作业（`--allow-move`）共用同一份记录，
    /// 搬移计划落在记录的 [`crate::recovery::Relocation`] 里，不再有第二族现场。
    ///
    /// 分类发生在锁下、已打开的那个句柄上，理由与记录型入口相同：先判一次再开第二次
    /// 会在判据与开目标之间留下窗口
    pub(crate) fn begin_resize(
        a: &impl TargetRequest,
    ) -> Result<(WriteSource, ResizeEntry), RunOutcome> {
        let (mut src, record_path) = Self::record_target(a)?;
        let active = Self::active_records(&src);
        let resumable = matches!(
            active.as_slice(),
            [one]
                if matches!(
                    one.probe.as_ref(),
                    crate::recovery::CompanionProbe::Decoded(record)
                        if record.op == crate::recovery::OpKind::Resize
                )
        );
        if !active.is_empty() && !resumable {
            return Err(Self::busy(&active));
        }
        crate::dev::refuse_if_occupied(&src)?;
        let (scene, credential) = Self::record_scene(&src)?;
        let journal = if resumable {
            let (journal, record) =
                crate::recovery::RecordJournal::open_image(&record_path, scene, src.sector_size, src.owner)
                    .map_err(|e| RunOutcome::refused(format!("cannot resume the recovery record: {e}")))?;
            // active 的判定已经核过操作类型；这里再核一次，防两处判据分叉
            if record.op != crate::recovery::OpKind::Resize {
                return Err(RunOutcome::refused(
                    "the existing recovery record belongs to a different operation — refusing to resume it",
                ));
            }
            journal
        } else {
            crate::recovery::RecordJournal::create_image(
                &record_path,
                scene,
                crate::recovery::OpKind::Resize,
                src.sector_size,
                credential,
                src.owner,
            )
            .map_err(|e| RunOutcome::refused(format!("cannot write the recovery record beside the image: {e}")))?
        };
        src.adopt_record_journal(journal);
        Ok((src, ResizeEntry { resumed: resumable }))
    }

    /// 只读打开：**不取所有权**。它不改变目标，所以既不与别的写者互斥，也不该被别人的
    /// 写挡住——`resize` 这类命令的只读阶段正是在自己随后要取锁之前调它。
    ///
    /// 块设备走只读打开：RW+O_EXCL 在盘被 claim 时会被内核拒绝，分区被占用会连同整盘
    /// 一起被 claim，而 `info` / `plan` 恰恰可能被用来查看一块正被使用的盘。
    /// 镜像同样真只读：调用面已核清无一处写盘，只读镜像/介质不被拒之门外
    pub(crate) fn read_only(a: &impl TargetRequest) -> Result<FileSource, RunOutcome> {
        #[cfg(target_os = "linux")]
        if let Ok(meta) = std::fs::metadata(a.target())
            && meta.file_type().is_block_device()
        {
            reject_sector_size_on_block(a)?;
            let src = FileSource::open_read_only(Path::new(a.target()))
                .map_err(|e| RunOutcome::no_mutation(format!("open failed: {e}")))?;
            return Ok(src);
        }
        FileSource::open_read_only_image(Path::new(a.target()), a.sector_size().map(|s| s.get()))
            .map_err(|e| RunOutcome::no_mutation(format!("open failed: {e}")))
    }

    /// 读写打开、不取所有权
    fn open_plain(a: &impl TargetRequest) -> Result<FileSource, RunOutcome> {
        if target_is_block(a.target()) {
            reject_sector_size_on_block(a)?;
        }
        let src = FileSource::open(Path::new(a.target()), a.sector_size().map(|s| s.get()))
            .map_err(|e| RunOutcome::no_mutation(format!("open failed: {e}")))?;
        Ok(src)
    }

    /// 读写打开 + 取目标的独占所有权（见 `targetlock`），返回**写句柄**。
    ///
    /// 三种写事务的入口都在本模块（`begin`、`begin_table_record*`、`begin_resize`），
    /// 它们都经这里取所有权；写方法只挂在 [`WriteSource`] 上（见 `dev::source`），而本函数
    /// 是 crate 内生产代码构造它的唯一途径——"写命令持锁"因此是类型保证，不是调用约定
    fn open_rw(a: &impl TargetRequest) -> Result<WriteSource, RunOutcome> {
        // 块设备的恢复状态落在 HostRepository，根与文件属 uid 0；
        // 非 root 的写命令在取锁前就退 6，而不是落成取锁失败的 I/O 错误
        #[cfg(target_os = "linux")]
        if target_is_block(a.target()) && !crate::dev::euid_is_root() {
            return Err(RunOutcome::unsupported(
                "the host recovery repository must be owned by uid 0; write commands on a block \
                 device are refused when this process is not root",
            ));
        }
        WriteSource::acquire(Self::open_plain(a)?)
    }

    /// 目标上还活着的恢复现场
    pub(crate) fn active_records(src: &FileSource) -> Vec<RecoveryRecord> {
        active_recovery_records(&src.identity)
    }

    /// 绕过事务入口的写盘命令（`resizefs` 与 `check` 的修复）动手前的唯一闸口：目标上若还
    /// 留着上一次操作的恢复现场，说明那件事没做完——此时扩 FS 或按修复结果写表会让那份
    /// journal 所指的旧布局彻底作废，而用户随后 undo 仍会把旧表字节回放上去，形成"表与盘上
    /// 内容自相矛盾"的状态（例如 FS 自述尺寸 > 分区尺寸）。
    ///
    /// 经过 `begin_without_history` 的写命令（add/delete/resize/mkfs…）不靠它：它们在目标被占用时
    /// 就已拒绝。本闸口覆盖的是只取所有权、不开事务的那些命令
    ///
    /// 现场落点由**整盘**键派生（见 `dev::TargetIdentity::from_block_keys`）：
    /// `/dev/sdb:2` 与 `/dev/sdb2` 因此共享同一处现场，只读分区节点即可枚举完
    pub(crate) fn refuse_if_active(src: &FileSource, what: &str) -> Result<(), RunOutcome> {
        Self::refuse_unidentifiable(src)?;
        let active = active_recovery_records(&src.identity);
        if active.is_empty() {
            return Ok(());
        }
        // 与 `begin` 同一句话（`busy`），只多一句本文命令为什么绕不开它
        Err(Self::busy(&active).context(&format!("{what} writes outside the recovery record")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(probe: crate::recovery::CompanionProbe) -> RecoveryRecord {
        RecoveryRecord { path: "r".into(), probe: Box::new(probe) }
    }

    fn way_out(active: &[RecoveryRecord]) -> String {
        match TransactionManager::busy(active) {
            RunOutcome::Refused(m) => m,
            other => panic!("busy must report the gate as Refused: {other:?}"),
        }
    }

    /// 出路按现场性质分路：损坏的记录不能许诺 undo 或续跑，只有 abandon；
    /// 布局读不懂的记录要说明原因；外来条目只让人手动处理
    #[test]
    fn busy_way_out_follows_the_state_of_the_record() {
        let text = way_out(&[record(crate::recovery::CompanionProbe::Damaged("bad payload".into()))]);
        assert!(!text.contains("undo"), "an unreadable record must not offer undo: {text}");
        assert!(!text.contains("re-run"), "an unreadable record must not offer resuming: {text}");
        assert!(text.contains("abandon"), "{text}");

        let text = way_out(&[record(crate::recovery::CompanionProbe::Foreign)]);
        assert!(text.contains("does not"), "an unknown layout must say it is unreadable: {text}");
        assert!(text.contains("abandon"), "{text}");

        let text = way_out(&[record(crate::recovery::CompanionProbe::NotOurs("a directory".into()))]);
        assert!(text.contains("by hand"), "a foreign entry is left for manual handling: {text}");
        assert!(!text.contains("abandon"), "this tool will not release a file it did not write: {text}");
    }
}
