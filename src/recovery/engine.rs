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
    /// 载荷身份与当前目标不符的原因。`None` 才是本目标可认领的记录；`Some` 只用于展示：
    /// 不阻塞写命令、不回放、不隔离
    pub(crate) foreign: Option<String>,
}

impl RecoveryRecord {
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// 本目标能否认领这条记录：身份必须严格相等（或身份读不出时按落点认领）
    pub(crate) fn claimable(&self) -> bool {
        self.foreign.is_none()
    }

    /// 报告用的单条描述
    pub(crate) fn describe(&self) -> String {
        let path = &self.path;
        let line = match self.probe.as_ref() {
            crate::recovery::CompanionProbe::Decoded(record) => format!(
                "{} (recovery record: {:?}, {} written range(s))",
                path.display(),
                record.op,
                record.restore.len()
            ),
            crate::recovery::CompanionProbe::Damaged(why) => {
                format!("{} (recovery record, unreadable: {why})", path.display())
            }
            crate::recovery::CompanionProbe::Foreign { magic } => {
                format!(
                    "{} ({})",
                    path.display(),
                    match crate::recovery::legacy_layout(*magic) {
                        Some(name) => format!(
                            "previous recovery-record layout ({name}), not readable by this build"
                        ),
                        None => "recovery record layout not recognized by this build".to_string(),
                    }
                )
            }
            crate::recovery::CompanionProbe::NotOurs(shape) => {
                format!("{} ({shape}, not a recovery file)", path.display())
            }
            crate::recovery::CompanionProbe::Unreadable(why) => {
                format!("{} (recovery record, cannot be read: {why})", path.display())
            }
            crate::recovery::CompanionProbe::Absent => path.display().to_string(),
        };
        match &self.foreign {
            Some(why) => format!("{line} — identity does not match the current target ({why})"),
            None => line,
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

/// 枚举目标上还活着的恢复现场，是「这个目标有哪些现场」的唯一入口：
/// `abandon` 的候选列表与身份不可用时的候选展示都从它取，调用方不各自拼落点。
///
/// 落点有两类，合并成一册再按记录载荷里的身份归并：
/// - 身份派生的落点（镜像旁的 `<image>.diskedit/record`，块设备的
///   `<state_root>/recovery/<identity-hash>/record`）；
/// - 同目录下的兄弟落点（块设备扫记录根，镜像扫目标所在目录里的 `*.diskedit`）。
///
/// 镜像被改名后，其落点目录名仍是旧名，只有扫目录才看得见；`src` 给出当前场景时，
/// 兄弟落点按载荷身份归并——身份相符的进入本目标的候选集，其余留在别处、
/// 既不列出也不参与释放。身份派生的落点无论能否解码都列出：它就在本目标名下。
///
/// 只有普通文件且可解码（或可被识别为损坏 / 布局不认识 / 外来文件）才算现场；缺席不是。
/// 旧 journal 与旧 checkpoint 不在这里认领——它们不是记录，不进本枚举，也不阻塞任何命令。
/// 节点消失的块设备身份不可恢复（拓扑派生），其落点随身份失去引用，不在本入口的
/// 可枚举范围内
pub(crate) fn active_recovery_records(identity: &TargetIdentity, src: Option<&FileSource>) -> Vec<RecoveryRecord> {
    let mut out = Vec::new();
    let current = src.and_then(current_scene);
    if let Some(path) = identity.record_path() {
        // 身份派生的落点：镜像落在镜像旁的记录目录，块设备落在整盘键派生的记录目录
        let probe = crate::recovery::probe_companion(path);
        if !matches!(probe, crate::recovery::CompanionProbe::Absent) {
            out.push(RecoveryRecord {
                path: path.to_path_buf(),
                probe: Box::new(probe),
                foreign: mismatch_reason(path, current.as_ref()),
            });
        }
    }
    scan_record_root(identity, current.as_ref(), &mut out);
    scan_image_directory(identity, current.as_ref(), &mut out);
    out
}

/// 归属判定的共同入口：只认目标当前能给出的身份证据。
/// 打不开、探测失败、身份材料为 `Unavailable` 时，任何记录都无法被证明属于它——
/// `abandon`、`undo` 的清理与记录枚举共用这里，不各自解释身份证据
pub(crate) fn ownership_scene(
    src: &crate::dev::FileSource,
) -> Result<crate::recovery::SceneIdentity, crate::platform::PlatformError> {
    let probe = crate::platform::probe::probe_of(src)?;
    let scene = crate::recovery::SceneIdentity::of_source(&probe, src);
    match scene.proof() {
        crate::recovery::SceneProof::Unavailable => Err(crate::platform::PlatformError::Unidentifiable(
            "no device id and no backing-file id".to_string(),
        )),
        _ => Ok(scene),
    }
}

/// 目标当前的场景身份；身份读不出时返回 `None`，调用方据此区分"不符"与"无法比对"
fn current_scene(src: &crate::dev::FileSource) -> Option<crate::recovery::SceneIdentity> {
    ownership_scene(src).ok()
}

/// 记录载荷声明的身份与当前场景不符的原因；身份读不出、记录不可解码或不是身份问题时
/// 返回 `None`——那些类别由 `probe` 的形态说明，不是"身份不符"
fn mismatch_reason(path: &Path, current: Option<&crate::recovery::SceneIdentity>) -> Option<String> {
    let current = current?;
    match crate::recovery::read_companion(path, current) {
        Ok(crate::recovery::Evidence::ForeignTarget { recorded, current }) => Some(
            recorded
                .mismatch_reason(&current)
                .unwrap_or_else(|| "the recorded identity differs from the target".to_string()),
        ),
        _ => None,
    }
}

/// 扫记录根补充身份派生路径找不到的记录。只有块设备的落点是
/// `<state_root>/recovery/<identity-hash>/record`，根因此有可扫的兄弟目录；
/// 镜像的落点旁没有这样的根，且其路径本身不随身份漂移。
///
/// 身份派生路径拼不出来时（块设备身份读不出），根退回主机仓库：候选按结构列出并标为
/// 不可认领，而不是整批消失
fn scan_record_root(
    identity: &TargetIdentity,
    current: Option<&crate::recovery::SceneIdentity>,
    out: &mut Vec<RecoveryRecord>,
) {
    use crate::dev::TargetKind;
    if identity.kind != TargetKind::Block {
        return;
    }
    let derived = identity.record_path();
    let root = derived
        .and_then(|p| p.parent().and_then(Path::parent))
        .map(Path::to_path_buf)
        .unwrap_or_else(|| crate::dev::host_state_root().join("recovery"));
    let Ok(entries) = std::fs::read_dir(&root) else {
        return;
    };
    for entry in entries.flatten() {
        let candidate = entry.path().join(crate::dev::RECORD_NAME);
        if derived == Some(candidate.as_path()) || out.iter().any(|r| r.path == candidate) {
            continue;
        }
        let probe = crate::recovery::probe_companion(&candidate);
        if matches!(probe, crate::recovery::CompanionProbe::Absent) {
            continue;
        }
        // 身份可读：候选由载荷认领。身份相符的候选同样进入枚举（可认领），
        // 身份不符的候选列出原因
        let foreign = match current {
            Some(current) => match crate::recovery::read_companion(&candidate, current) {
                Ok(crate::recovery::Evidence::Recognized(_)) => None,
                Ok(crate::recovery::Evidence::ForeignTarget { recorded, current }) => Some(
                    recorded
                        .mismatch_reason(&current)
                        .unwrap_or_else(|| "the recorded identity differs from the target".to_string()),
                ),
                _ => continue,
            },
            // 身份读不出：无法比对。现场一律列出——读不懂的也列，否则它们对诊断与清理
            // 都消失了；不可认领的标记仍照写
            None => Some(
                "the current target identity could not be established, so this record cannot be matched to it"
                    .to_string(),
            ),
        };
        out.push(RecoveryRecord { path: candidate, probe: Box::new(probe), foreign });
    }
}

/// 扫镜像所在目录，补充身份派生路径找不到的兄弟落点：镜像改名后落点目录名仍是旧名，
/// 按新路径拼出的落点不存在，只有扫目录才看得见旧现场。
///
/// 兄弟落点由**记录载荷**归并：只有身份与当前场景相符的才进入候选集。别的主体的落点
/// 不列出，也不参与释放——`abandon` 是显式放弃本目标的场景，不是清空整个目录。
/// 身份不可得时无从归并，兄弟落点全部略过（身份派生的落点照旧列出）
fn scan_image_directory(
    identity: &TargetIdentity,
    current: Option<&crate::recovery::SceneIdentity>,
    out: &mut Vec<RecoveryRecord>,
) {
    use crate::dev::TargetKind;
    if identity.kind != TargetKind::Image {
        return;
    }
    let Some(current) = current else {
        return;
    };
    let Some(dir) = identity.canonical.parent() else {
        return;
    };
    let derived = identity.record_path();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let Some(stem) = name.strip_suffix(crate::dev::RECORD_SUFFIX) else {
            continue;
        };
        if stem.is_empty() {
            continue;
        }
        let candidate = entry.path().join(crate::dev::RECORD_NAME);
        if derived == Some(candidate.as_path()) || out.iter().any(|r| r.path == candidate) {
            continue;
        }
        // 身份不符或读不出来：这是别的主体的落点（或无从归并的条目），不进本目标的候选集
        if !matches!(
            crate::recovery::read_companion(&candidate, current),
            Ok(crate::recovery::Evidence::Recognized(_))
        ) {
            continue;
        }
        let probe = crate::recovery::probe_companion(&candidate);
        if matches!(probe, crate::recovery::CompanionProbe::Absent) {
            continue;
        }
        out.push(RecoveryRecord { path: candidate, probe: Box::new(probe), foreign: None });
    }
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
                    !record.restore_blocked || crate::recovery::table_only_replay(record.op),
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
            crate::recovery::CompanionProbe::Foreign { magic } => Some(match crate::recovery::legacy_layout(*magic) {
                Some(name) => format!(
                    "the recovery record there was written in the previous layout ({name}); this build reads the current layout only"
                ),
                None => "the recovery record there uses a layout this build does not recognize".to_string(),
            }),
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

    /// 记录准入前的两道只读核验：占用探测与块设备只读检查。它们先于记录目录与锁
    /// 执行——目录校验要求能进目录，root 建出的 0700 目录对降权用户不可入，而
    /// mountinfo 读不到这类环境故障必须以 4 浮出，不能被目录拒绝遮住
    fn pre_admission(a: &impl TargetRequest, allow_dm_held: bool) -> Result<(), RunOutcome> {
        let peek = Self::open_plain(a)?;
        let shape = if allow_dm_held {
            crate::dev::OpShape::Extend
        } else {
            crate::dev::OpShape::Overwrite
        };
        crate::dev::refuse_if_occupied(&peek, shape)?;
        crate::dev::refuse_read_only_block(&peek)?;
        Ok(())
    }

    /// 表类写命令的公共打开段：取所有权、定位记录落点、清残留临时文件。
    /// 现场判定留给调用方——占用与可写性已在 [`Self::pre_admission`] 过闸
    fn record_target(a: &impl TargetRequest) -> Result<(WriteSource, std::path::PathBuf), RunOutcome> {
        let src = Self::open_rw(a)?;
        Self::refuse_unidentifiable(&src)?;
        // 块设备的记录目录由 WWN / EUI / serial 派生的 identity-hash
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
    /// （不是命令行的第二份解析），读不出时不在写盘前猜测
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
    /// 写路径上只有恢复记录一条凭据；记录本身在第一次写盘前才落盘（见 [`crate::recovery::RecordJournal`]）：
    /// 写盘前就被拒的命令不会留下空记录占住落点
    pub(crate) fn begin_table_record(
        a: &impl TargetRequest,
        op: crate::recovery::OpKind,
    ) -> Result<WriteSource, RunOutcome> {
        Self::pre_admission(a, false)?;
        let (mut src, record_path) = Self::record_target(a)?;
        let active = Self::claimed_records(&src);
        if !active.is_empty() {
            return Err(Self::busy(&active));
        }
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
        Self::pre_admission(a, false)?;
        let (mut src, record_path) = Self::record_target(a)?;
        let active = Self::claimed_records(&src);
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
        allow_dm_held: bool,
    ) -> Result<(WriteSource, ResizeEntry), RunOutcome> {
        Self::pre_admission(a, allow_dm_held)?;
        let (mut src, record_path) = Self::record_target(a)?;
        let active = Self::claimed_records(&src);
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

    /// 目标上还活着的恢复现场（含身份不符的候选：供展示，供调用方过滤）
    pub(crate) fn active_records(src: &FileSource) -> Vec<RecoveryRecord> {
        active_recovery_records(&src.identity, Some(src))
    }

    /// 本目标可认领的恢复现场：闸口只看这一份。身份不符/不可比的候选不阻塞目标，
    /// 也不参与续跑判定
    pub(crate) fn claimed_records(src: &FileSource) -> Vec<RecoveryRecord> {
        Self::active_records(src).into_iter().filter(RecoveryRecord::claimable).collect()
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
        let active = Self::claimed_records(src);
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
        RecoveryRecord { path: "r".into(), probe: Box::new(probe), foreign: None }
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

        let text = way_out(&[record(crate::recovery::CompanionProbe::Foreign {
            magic: *b"JRNL",
        })]);
        assert!(text.contains("does not"), "an unknown layout must say it is unreadable: {text}");
        assert!(text.contains("abandon"), "{text}");

        let text = way_out(&[record(crate::recovery::CompanionProbe::NotOurs("a directory".into()))]);
        assert!(text.contains("by hand"), "a foreign entry is left for manual handling: {text}");
        assert!(!text.contains("abandon"), "this tool will not release a file it did not write: {text}");
    }
}
