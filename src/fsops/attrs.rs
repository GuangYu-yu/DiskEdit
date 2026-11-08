//! FS 属性操作：check、label 与 UUID。

use super::*;

/// FS 一致性检查（不修改分区边界；是否写盘取决于各工具的只读/修复开关）。
/// 各工具与开关：e2fsck -fp（man e2fsck）、ntfsfix -d/--clear-dirty（能修复并成功
/// 挂载时清除 NTFS dirty 标志，否则保持/置位 dirty 交 Windows 下次检查；ntfsfix 非
/// chkdsk，检查语义偏弱，man ntfsfix）、fsck.f2fs 无参 = 仅检查（man fsck.f2fs）、
/// xfs_repair -n = no modify（man xfs_repair）、btrfs check = 默认只读（man btrfs-check）、
/// fsck.vfat -n = no-operation 只读（man fsck.fat）、fsck.exfat -n = read-only 不修复（man fsck.exfat）
pub fn check_fs(src: &FileSource, part: u32, fstype: Option<ContentKind>) -> Result<(), FsError> {
    if fstype == Some(ContentKind::Lvm2Pv) {
        return Err(FsError::invalid(
            "target is an LVM2 PV, not a filesystem — PV metadata is checked with pvck(8), not fsck",
        ));
    }
    let cmd: (&str, Vec<&str>) = match fstype {
        // e2fsck -n：man e2fsck(8)「Open the file system read-only, and assume an
        // answer of `no' to all questions」；ntfsfix -n：man ntfsfix(8)
        // 「Do not write anything, just show what would have been done」
        Some(ContentKind::Filesystem(FsKind::Ext)) => ("e2fsck", vec!["-n"]),
        Some(ContentKind::Filesystem(FsKind::Ntfs)) => ("ntfsfix", vec!["-n"]),
        Some(ContentKind::Filesystem(FsKind::F2fs)) => ("fsck.f2fs", vec![]),
        Some(ContentKind::Filesystem(FsKind::Xfs)) => ("xfs_repair", vec!["-n"]),
        Some(ContentKind::Filesystem(FsKind::Btrfs)) => ("btrfs", vec!["check"]),
        Some(ContentKind::Filesystem(FsKind::Vfat)) => ("fsck.vfat", vec!["-n"]),
        // fsck.exfat man 未定义无参默认行为（存在 -r 交互修复），显式 -n = 仅检查不修复
        Some(ContentKind::Filesystem(FsKind::Exfat)) => ("fsck.exfat", vec!["-n"]),
        other => return Err(FsError::unsupported(format!(
            "no check tool wired for {}",
            content_name(other.unwrap_or(ContentKind::Unknown))
        ))),
    };
    with_partition_device(src, part, |dev| {
        let mut args = cmd.1;
        args.push(dev);
        let out = run(cmd.0, &args)?;
        if cmd.0 == "e2fsck" {
            // check 是只读命令，e2fsck 退出非零即检查失败。
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

/// 卷标落在定长字段里的类型按字段容量前置校验，超出即拒绝——截断与拒绝是两种结局，
/// 本层不把它们合并成一次"设置成功"。`set label` 与 `mkfs --label` 共用这一份判据：
/// 建 FS 时才写卷标的话，判据必须同样发生在盘上第一个字节被擦除之前。
/// 来源：tune2fs -L（man tune2fs）、xfs_admin -L ≤12 字节（man xfs_admin）、
/// btrfs 超级块 label（含结尾 NUL 共 256 字节）、ntfslabel 的 128 个 UTF-16 码元字段、
/// fatlabel 的 11 字节字段（man fatlabel）、exfatlabel 的 11 个 UTF-16 码元字段、
/// swaplabel -L 的 volume_name[16]（xstrncpy 至多 15 字节）
pub fn check_label_limits(fstype: Option<ContentKind>, label: &str) -> Result<(), FsError> {
    match fstype {
        // XFS 标签上限 12 字节（superblock s_fname[12]，man xfs_admin "twelve
        // characters"）：超长时 xfs_admin 静默截断，故提前拒绝
        Some(ContentKind::Filesystem(FsKind::Xfs)) => {
            if label.len() > 12 {
                return Err(FsError::invalid(format!(
                    "xfs label exceeds 12 bytes (got {}); the on-disk field holds 12",
                    label.len()
                )));
            }
        }
        // 超级块 label 含结尾 NUL 共 256 字节，超出时 btrfs 拒绝，故提前按字节拒绝
        Some(ContentKind::Filesystem(FsKind::Btrfs)) => {
            if label.len() > 255 {
                return Err(FsError::invalid(format!(
                    "btrfs label exceeds 255 bytes (got {}); btrfs would reject it",
                    label.len()
                )));
            }
        }
        // 卷标字段 128 个 UTF-16 码元，超出时 ntfslabel 截断，故提前拒绝
        Some(ContentKind::Filesystem(FsKind::Ntfs)) => {
            let units = label.encode_utf16().count();
            if units > 128 {
                return Err(FsError::invalid(format!(
                    "ntfs label exceeds 128 UTF-16 units (got {units}); ntfslabel would truncate it"
                )));
            }
        }
        // 卷标字段 11 字节，判据见 vfat_label_check
        Some(ContentKind::Filesystem(FsKind::Vfat)) => vfat_label_check(label)?,
        // 卷标字段 11 个 UTF-16 码元，超出时 exfatlabel 以 E2BIG 拒绝，故提前拒绝
        Some(ContentKind::Filesystem(FsKind::Exfat)) => {
            let units = label.encode_utf16().count();
            if units > 11 {
                return Err(FsError::invalid(format!(
                    "exfat label exceeds 11 UTF-16 units (got {units}); exfatlabel would reject it"
                )));
            }
        }
        // volume_name[16] 留结尾 NUL，xstrncpy 至多复制 n-1 字节，故 15 字节是上限；
        // 超出时 swaplabel 截断，故提前按字节拒绝
        Some(ContentKind::Swap) if label.len() > 15 => {
            return Err(FsError::invalid(format!(
                "swap label exceeds 15 bytes (got {}); swaplabel would truncate it",
                label.len()
            )));
        }
        _ => {}
    }
    Ok(())
}

/// 设置 FS label。值原样传给各 FS 官方工具；字段容量判据见 [`check_label_limits`]
pub fn set_label(src: &FileSource, part: u32, fstype: Option<ContentKind>, label: &str) -> Result<(), FsError> {
    check_label_limits(fstype, label)?;
    with_partition_device(src, part, |dev| {
        let (tool, args): (&str, Vec<String>) = match fstype {
            Some(ContentKind::Filesystem(FsKind::Ext)) => ("tune2fs", vec!["-L".into(), label.into(), dev.into()]),
            Some(ContentKind::Filesystem(FsKind::Xfs)) => ("xfs_admin", vec!["-L".into(), label.into(), dev.into()]),
            Some(ContentKind::Filesystem(FsKind::Btrfs)) => ("btrfs", vec!["filesystem".into(), "label".into(), dev.into(), label.into()]),
            Some(ContentKind::Filesystem(FsKind::Ntfs)) => ("ntfslabel", vec![dev.into(), label.into()]),
            Some(ContentKind::Filesystem(FsKind::Vfat)) => ("fatlabel", vec![dev.into(), label.into()]),
            Some(ContentKind::Filesystem(FsKind::Exfat)) => ("exfatlabel", vec![dev.into(), label.into()]),
            Some(ContentKind::Swap) => ("swaplabel", vec!["-L".into(), label.into(), dev.into()]),
            other => return Err(FsError::unsupported(format!(
                "no label tool wired for {}",
                content_name(other.unwrap_or(ContentKind::Unknown))
            ))),
        };
        let argrefs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
        let out = run_write(tool, &argrefs)?;
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

/// 该 FS 能接受什么样的 UUID 设置请求（四态，不折成布尔）：
/// - `ExplicitAndGenerated`：既能设指定值，也能生成新值（ext: tune2fs -U uuid|random；
///   xfs: xfs_admin -U uuid|generate；btrfs: btrfstune -f -U uuid / -u）
/// - `ExplicitOnly`：只有指定值形式（swaplabel -U UUID，没有 generate 形式）
/// - `GeneratedOnly`：只支持"生成新值"。ntfs 唯一可改的标识是 `ntfslabel --new-serial`
///   生成的 serial，而它**不是** Windows volume UUID（man ntfslabel）——用户给的具体值
///   无从落实，静默丢弃正是要防的漂移，故必须让调用方显式拒绝
/// - `No`：本工具不为这种类型接 UUID 工具
pub enum UuidSupport {
    ExplicitAndGenerated,
    ExplicitOnly,
    GeneratedOnly,
    No(&'static str),
}

pub fn uuid_support(fstype: Option<ContentKind>) -> UuidSupport {
    match fstype {
        Some(ContentKind::Filesystem(FsKind::Ext | FsKind::Xfs | FsKind::Btrfs)) => UuidSupport::ExplicitAndGenerated,
        Some(ContentKind::Swap) => UuidSupport::ExplicitOnly,
        Some(ContentKind::Filesystem(FsKind::Ntfs)) => UuidSupport::GeneratedOnly,
        _ => UuidSupport::No("not wired to a tool — use the filesystem's own utility"),
    }
}

/// 设置 FS UUID。来源：tune2fs -U uuid|random（man tune2fs）、
/// xfs_admin -U uuid|generate（man xfs_admin）、ntfslabel --new-serial 无值=随机 serial
/// （man ntfslabel）、btrfstune -f -U uuid / -u（-f：change fsid 属 dangerous changes，
/// man btrfstune）、swaplabel -U uuid（man swaplabel，无生成形式）。
/// 请求形式与 FS 能力的匹配由调用方先按 [`uuid_support`] 判定；此处只做工具映射，
/// 落不到工具的组合同样拒绝，不静默降级
pub fn set_uuid(src: &FileSource, part: u32, fstype: Option<ContentKind>, req: &UuidRequest) -> Result<(), FsError> {
    with_partition_device(src, part, |dev| {
        let no_tool = |what: &str| {
            FsError::unsupported(format!(
                "no uuid tool wired for {}{what}",
                content_name(fstype.unwrap_or(ContentKind::Unknown))
            ))
        };
        let (tool, args): (&str, Vec<String>) = match req {
            UuidRequest::Explicit(u) => match fstype {
                Some(ContentKind::Filesystem(FsKind::Ext)) => ("tune2fs", vec!["-U".into(), u.clone(), dev.into()]),
                Some(ContentKind::Filesystem(FsKind::Xfs)) => ("xfs_admin", vec!["-U".into(), u.clone(), dev.into()]),
                Some(ContentKind::Filesystem(FsKind::Btrfs)) => ("btrfstune", vec!["-f".into(), "-U".into(), u.clone(), dev.into()]),
                Some(ContentKind::Swap) => ("swaplabel", vec!["-U".into(), u.clone(), dev.into()]),
                _ => return Err(no_tool("")),
            },
            // "生成新值"在各工具里的写法不同：tune2fs -U random、xfs_admin -U generate、
            // btrfstune -u、ntfslabel --new-serial；swaplabel 没有生成形式
            UuidRequest::NewRandom => match fstype {
                Some(ContentKind::Filesystem(FsKind::Ext)) => ("tune2fs", vec!["-U".into(), "random".into(), dev.into()]),
                Some(ContentKind::Filesystem(FsKind::Xfs)) => ("xfs_admin", vec!["-U".into(), "generate".into(), dev.into()]),
                Some(ContentKind::Filesystem(FsKind::Btrfs)) => ("btrfstune", vec!["-f".into(), "-u".into(), dev.into()]),
                Some(ContentKind::Filesystem(FsKind::Ntfs)) => ("ntfslabel", vec!["--new-serial".into(), dev.into()]),
                _ => return Err(no_tool(" that generates a new value")),
            },
        };
        let argrefs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
        let out = run_write(tool, &argrefs)?;
        if !out.status.success() {
            return Err(FsError::command(tool, &out));
        }
        Ok(())
    })
}