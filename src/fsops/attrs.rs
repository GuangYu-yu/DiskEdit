//! FS 属性操作：check、label 与 UUID。

use super::*;

/// FS 一致性检查（不修改分区边界；是否写盘取决于各工具的只读/修复开关）。
/// 各工具与开关：e2fsck -fp（man e2fsck）、ntfsfix -d/--clear-dirty（能修复并成功
/// 挂载时清除 NTFS dirty 标志，否则保持/置位 dirty 交 Windows 下次检查；ntfsfix 非
/// chkdsk，检查语义偏弱，man ntfsfix）、fsck.f2fs 无参 = 仅检查（man fsck.f2fs）、
/// xfs_repair -n = no modify（man xfs_repair）、btrfs check = 默认只读（man btrfs-check）、
/// fsck.vfat -n = no-operation 只读（man fsck.fat）、fsck.exfat -n = read-only 不修复（man fsck.exfat）
pub fn check_fs(src: &FileSource, part: u32, fstype: Option<FsKind>) -> Result<(), FsError> {
    if fstype == Some(FsKind::Lvm2Pv) {
        return Err(FsError::unsupported(
            "target is an LVM2 PV, not a filesystem — PV metadata is checked with pvck(8), not fsck",
        ));
    }
    let cmd: (&str, Vec<&str>) = match fstype {
        Some(FsKind::Ext) => ("e2fsck", vec!["-fp"]),
        Some(FsKind::Ntfs) => ("ntfsfix", vec!["-d"]),
        Some(FsKind::F2fs) => ("fsck.f2fs", vec![]),
        Some(FsKind::Xfs) => ("xfs_repair", vec!["-n"]),
        Some(FsKind::Btrfs) => ("btrfs", vec!["check"]),
        Some(FsKind::Vfat) => ("fsck.vfat", vec!["-n"]),
        // fsck.exfat man 未定义无参默认行为（存在 -r 交互修复），显式 -n = 仅检查不修复
        Some(FsKind::Exfat) => ("fsck.exfat", vec!["-n"]),
        other => return Err(FsError::unsupported(format!("no check tool wired for {}", fs_name(other)))),
    };
    with_partition_device(src, part, |dev| {
        let mut args = cmd.1;
        args.push(dev);
        let out = run(cmd.0, &args)?;
        if cmd.0 == "e2fsck" {
            // 被信号杀死时没有退出码：那是基础设施故障，不能拿 -1 冒充一个退出码
            return match out.status.code() {
                Some(code) => check_e2fsck_for_check(code),
                None => Err(FsError::CommandFailed(format!("e2fsck {}", describe_exit(&out.status)))),
            };
        }
        if !out.status.success() {
            return Err(FsError::command(cmd.0, &out));
        }
        Ok(())
    })
}

/// VFAT 卷标的准入判据。字段容量是 11 **字节**（man fatlabel），而 VFAT 按 DOS/OEM
/// 代码页存取标签，所以判据必须是编码后的字节数。本工具不做代码页转换（无编码库依赖），
/// 前置准入因此只收 ASCII：ASCII 每字符恰好一字节，`len()` 即编码后的长度；非 ASCII
/// 标签在本层拒绝。这是本工具自身的限制，不是字段的限制——fatlabel 支持按代码页
/// 设置非 ASCII 标签，需要时直接用 fatlabel
pub(crate) fn vfat_label_check(label: &str) -> Result<(), FsError> {
    if !label.is_ascii() {
        return Err(FsError::invalid(format!(
            "vfat label {label:?} cannot be represented in a DOS code page by this tool \
             (labels here are accepted as ASCII only); set it with fatlabel directly"
        )));
    }
    if label.len() > 11 {
        return Err(FsError::invalid(format!(
            "vfat label exceeds the 11-byte on-disk limit (got {} bytes); fatlabel would reject it",
            label.len()
        )));
    }
    Ok(())
}

/// 设置 FS label。值原样传给各 FS 官方工具；卷标落在定长字段里的类型在此按字段容量前置
/// 校验，超出即拒绝——截断与拒绝是两种结局，本层不把它们合并成一次"设置成功"。
/// 来源：tune2fs -L（man tune2fs）、xfs_admin -L ≤12 字节（man xfs_admin）、
/// btrfs 超级块 label（含结尾 NUL 共 256 字节）、ntfslabel 的 128 个 UTF-16 码元字段、
/// fatlabel 的 11 字节字段（man fatlabel）、exfatlabel 的 11 个 UTF-16 码元字段、
/// swaplabel -L 的 volume_name[16]（xstrncpy 至多 15 字节）
pub fn set_label(src: &FileSource, part: u32, fstype: Option<FsKind>, label: &str) -> Result<(), FsError> {
    with_partition_device(src, part, |dev| {
        let (tool, args): (&str, Vec<String>) = match fstype {
            Some(FsKind::Ext) => ("tune2fs", vec!["-L".into(), label.into(), dev.into()]),
            // XFS 标签上限 12 字节（superblock s_fname[12]，man xfs_admin "twelve
            // characters"）：超长时 xfs_admin 静默截断，故提前拒绝
            Some(FsKind::Xfs) => {
                if label.len() > 12 {
                    return Err(FsError::invalid(format!(
                        "xfs label exceeds 12 bytes (got {}); xfs_admin would silently truncate",
                        label.len()
                    )));
                }
                ("xfs_admin", vec!["-L".into(), label.into(), dev.into()])
            }
            // 超级块 label 含结尾 NUL 共 256 字节，超出时 btrfs 拒绝，故提前按字节拒绝
            Some(FsKind::Btrfs) => {
                if label.len() > 255 {
                    return Err(FsError::invalid(format!(
                        "btrfs label exceeds 255 bytes (got {}); btrfs would reject it",
                        label.len()
                    )));
                }
                ("btrfs", vec!["filesystem".into(), "label".into(), dev.into(), label.into()])
            }
            // 卷标字段 128 个 UTF-16 码元，超出时 ntfslabel 截断，故提前拒绝
            Some(FsKind::Ntfs) => {
                let units = label.encode_utf16().count();
                if units > 128 {
                    return Err(FsError::invalid(format!(
                        "ntfs label exceeds 128 UTF-16 units (got {units}); ntfslabel would truncate it"
                    )));
                }
                ("ntfslabel", vec![dev.into(), label.into()])
            }
            // 卷标字段 11 字节，判据见 vfat_label_check
            Some(FsKind::Vfat) => {
                vfat_label_check(label)?;
                ("fatlabel", vec![dev.into(), label.into()])
            }
            // 卷标字段 11 个 UTF-16 码元，超出时 exfatlabel 以 E2BIG 拒绝，故提前拒绝
            Some(FsKind::Exfat) => {
                let units = label.encode_utf16().count();
                if units > 11 {
                    return Err(FsError::invalid(format!(
                        "exfat label exceeds 11 UTF-16 units (got {units}); exfatlabel would reject it"
                    )));
                }
                ("exfatlabel", vec![dev.into(), label.into()])
            }
            // volume_name[16] 留结尾 NUL，xstrncpy 至多复制 n-1 字节，故 15 字节是上限；
            // 超出时 swaplabel 截断，故提前按字节拒绝
            Some(FsKind::Swap) => {
                if label.len() > 15 {
                    return Err(FsError::invalid(format!(
                        "swap label exceeds 15 bytes (got {}); swaplabel would truncate it",
                        label.len()
                    )));
                }
                ("swaplabel", vec!["-L".into(), label.into(), dev.into()])
            }
            other => return Err(FsError::unsupported(format!("no label tool wired for {}", fs_name(other)))),
        };
        let argrefs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
        let out = run(tool, &argrefs)?;
        if !out.status.success() {
            return Err(FsError::command(tool, &out));
        }
        Ok(())
    })
}

/// 一次 UUID 设置请求的两种形式。**不含"目标能不能接受"**——那是 [`uuid_support`] 回答的，
/// 由调用方比对后决定拒还是做（命令层是唯一能给出"改参数也许有解"式拒绝的地方）
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UuidRequest {
    /// 写入调用方给定的值
    Explicit(String),
    /// 要求目标生成一个新的随机值
    NewRandom,
}

/// 该 FS 能接受什么样的 UUID 设置请求（三态，不折成布尔）：
/// - `Yes`：值由调用方给定（tune2fs -U / xfs_admin -U / btrfstune -U / swaplabel -U）
/// - `RandomOnly`：只支持"生成新随机值"。ntfs 唯一可改的标识是 `ntfslabel --new-serial`
///   生成的 serial，而它**不是** Windows volume UUID（man ntfslabel）——用户给的具体值
///   无从落实，静默丢弃正是要防的漂移，故必须让调用方显式拒绝
/// - `No`：本工具不为这种类型接 UUID 工具
pub enum UuidSupport {
    Yes,
    RandomOnly,
    No(&'static str),
}

pub fn uuid_support(fstype: Option<FsKind>) -> UuidSupport {
    if matches!(fstype, Some(FsKind::Ext | FsKind::Xfs | FsKind::Btrfs | FsKind::Swap)) {
        UuidSupport::Yes
    } else if fstype == Some(FsKind::Ntfs) {
        UuidSupport::RandomOnly
    } else {
        UuidSupport::No("not wired to a tool — use the filesystem's own utility")
    }
}

/// 设置 FS UUID。来源：tune2fs -U（man tune2fs）、xfs_admin -U（man xfs_admin）、
/// ntfslabel --new-serial 无值=随机 serial（man ntfslabel）、
/// btrfstune -f -U（-f：change fsid 属 dangerous changes，man btrfstune）、
/// swaplabel -U（man swaplabel）。
/// 请求形式与 FS 能力的匹配由调用方先按 [`uuid_support`] 判定；此处只做工具映射，
/// 落不到工具的组合同样拒绝，不静默降级
pub fn set_uuid(src: &FileSource, part: u32, fstype: Option<FsKind>, req: &UuidRequest) -> Result<(), FsError> {
    with_partition_device(src, part, |dev| {
        let no_tool = |what: &str| {
            FsError::unsupported(format!("no uuid tool wired for {}{what}", fs_name(fstype)))
        };
        let (tool, args): (&str, Vec<String>) = match req {
            UuidRequest::Explicit(u) => match fstype {
                Some(FsKind::Ext) => ("tune2fs", vec!["-U".into(), u.clone(), dev.into()]),
                Some(FsKind::Xfs) => ("xfs_admin", vec!["-U".into(), u.clone(), dev.into()]),
                Some(FsKind::Btrfs) => ("btrfstune", vec!["-f".into(), "-U".into(), u.clone(), dev.into()]),
                Some(FsKind::Swap) => ("swaplabel", vec!["-U".into(), u.clone(), dev.into()]),
                _ => return Err(no_tool("")),
            },
            // 只有 ntfs 的工具提供"生成新值"这一用法
            UuidRequest::NewRandom => match fstype {
                Some(FsKind::Ntfs) => ("ntfslabel", vec!["--new-serial".into(), dev.into()]),
                _ => return Err(no_tool(" that generates a random value")),
            },
        };
        let argrefs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
        let out = run(tool, &argrefs)?;
        if !out.status.success() {
            return Err(FsError::command(tool, &out));
        }
        Ok(())
    })
}