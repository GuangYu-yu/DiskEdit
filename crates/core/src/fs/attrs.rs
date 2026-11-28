//! 文件系统内容属性的纯事实：卷标的字段容量与 UUID 的可写形式。
//!
//! 这里只回答"这个值落在该内容的容量内吗""该类内容能接受哪种 UUID 请求"，
//! 不拼命令行，也不判断宿主工具是否可用。能力是否落地由档案表的 `attrs` 列给出，
//! 命令行与执行层都消费同一份结论。

use super::catalog::{Support, profile};
use super::profile::{ContentKind, FsKind};
use super::program::Program;

/// 一类内容能接受何种 UUID 写入请求（四态，不折成布尔）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UuidSupport {
    /// 既能写入指定值，也能要求目标生成新值。
    ExplicitAndGenerated,
    /// 只有指定值形式。
    ExplicitOnly,
    /// 只能生成新值；具体值无从落实，必须拒绝而不能静默丢弃。
    GeneratedOnly,
    /// 本实现不为这种内容接 UUID 工具，理由随变体携带。
    No(&'static str),
}

impl UuidSupport {
    /// 能否写入调用方给定的值。
    pub const fn accepts_explicit(self) -> bool {
        matches!(self, Self::ExplicitAndGenerated | Self::ExplicitOnly)
    }

    /// 能否要求目标生成新值。
    pub const fn accepts_generated(self) -> bool {
        matches!(self, Self::ExplicitAndGenerated | Self::GeneratedOnly)
    }

    /// 不支持的固定理由；其余形态为 `None`。
    pub const fn reason(self) -> Option<&'static str> {
        match self {
            Self::No(why) => Some(why),
            Self::ExplicitAndGenerated | Self::ExplicitOnly | Self::GeneratedOnly => None,
        }
    }
}

/// 该内容能否接受 UUID 写入，以及接受哪些形式。
///
/// 来源：各文件系统手册页给出的写入形式——ext/xfs/btrfs 既能写指定值也能要求生成新值、
/// swap 只有指定值形式、ntfs 只能生成 serial（且不是 Windows 卷 UUID）。
pub const fn uuid_support(kind: ContentKind) -> UuidSupport {
    match kind {
        ContentKind::Filesystem(FsKind::Ext | FsKind::Xfs | FsKind::Btrfs) => {
            UuidSupport::ExplicitAndGenerated
        }
        ContentKind::Swap => UuidSupport::ExplicitOnly,
        ContentKind::Filesystem(FsKind::Ntfs) => UuidSupport::GeneratedOnly,
        ContentKind::Luks => UuidSupport::No(
            "target is a LUKS container, not a filesystem — the header UUID is changed with \
             `cryptsetup luksUUID --uuid`; this tool sets filesystem UUIDs only",
        ),
        ContentKind::BitLocker => UuidSupport::No(
            "target is a BitLocker volume, not a filesystem — Windows manages it (manage-bde); \
             this tool only identifies it",
        ),
        ContentKind::Filesystem(_) | ContentKind::Lvm2Pv | ContentKind::Unknown => {
            UuidSupport::No("not wired to a tool — use the filesystem's own utility")
        }
    }
}

/// 一次 UUID 写入请求需要的程序；请求形式落不到工具时为 `None`。
pub const fn uuid_program(kind: ContentKind, generate: bool) -> Option<Program> {
    match (kind, generate) {
        (ContentKind::Filesystem(FsKind::Ext), _) => Some(Program::Tune2fs),
        (ContentKind::Filesystem(FsKind::Xfs), _) => Some(Program::XfsAdmin),
        (ContentKind::Filesystem(FsKind::Btrfs), _) => Some(Program::Btrfstune),
        (ContentKind::Swap, false) => Some(Program::Swaplabel),
        (ContentKind::Filesystem(FsKind::Ntfs), true) => Some(Program::Ntfslabel),
        _ => None,
    }
}

/// 写卷标需要的程序；内容没有卷标工具时为 `None`。
pub const fn label_program(kind: ContentKind) -> Option<Program> {
    match profile(kind).attrs {
        Support::Tools(&[program]) => Some(program),
        Support::Tools(_) | Support::NotApplicable | Support::Unsupported(_) => None,
    }
}

/// 卷标是否落在该类内容盘上的定长字段里；越界给出拒绝理由。
///
/// 卷标落在定长字段里的类型按字段容量前置校验，超出即拒绝——截断与拒绝是两种结局，
/// 本层不把它们合并成一次"设置成功"。来源：ext 超级块 `s_volume_name[16]` 按字节、
/// xfs 超级块 `s_fname[12]` 按字节、btrfs 超级块 label 含结尾 NUL 共 256 字节、
/// ntfs 卷标字段 128 个 UTF-16 码元、VFAT 卷标字段 11 字节、exfat 11 个 UTF-16 码元、
/// swap 的 `volume_name[16]`（xstrncpy 至多 15 字节）。
pub fn label_problem(kind: ContentKind, label: &str) -> Option<String> {
    match kind {
        // ext 超级块卷标字段 s_volume_name[16] 按字节存放，不要求结尾 NUL：填满 16 字节
        // 是合法值，超出即拒绝，不把判决留给工具的参数错误。
        ContentKind::Filesystem(FsKind::Ext) if label.len() > 16 => Some(format!(
            "ext label exceeds the 16-byte s_volume_name field (got {} bytes); \
             the kernel and e2fsprogs read at most 16",
            label.len()
        )),
        // XFS 标签上限 12 字节（superblock s_fname[12]，手册页写明 "twelve
        // characters"）：超长时工具会静默截断，故提前拒绝。
        ContentKind::Filesystem(FsKind::Xfs) if label.len() > 12 => Some(format!(
            "xfs label exceeds 12 bytes (got {}); the on-disk field holds 12",
            label.len()
        )),
        // 超级块 label 含结尾 NUL 共 256 字节，超出时 btrfs 拒绝。
        ContentKind::Filesystem(FsKind::Btrfs) if label.len() > 255 => Some(format!(
            "btrfs label exceeds 255 bytes (got {}); btrfs would reject it",
            label.len()
        )),
        // 卷标字段 128 个 UTF-16 码元，超出时工具会截断。
        ContentKind::Filesystem(FsKind::Ntfs) => {
            let units = label.encode_utf16().count();
            (units > 128).then(|| {
                format!("ntfs label exceeds the 128 UTF-16 unit field (got {units} units)")
            })
        }
        // 字段容量是 11 字节；VFAT 按 DOS/OEM 代码页存取标签，所以判据是编码后的
        // 字节数。本实现不做代码页转换（无编码库依赖），因此只收 ASCII：ASCII 每
        // 字符恰好一字节，`len()` 即编码后的长度。
        ContentKind::Filesystem(FsKind::Vfat) => {
            if !label.is_ascii() {
                Some(format!(
                    "vfat label {label:?} cannot be represented in a DOS code page by this tool \
                     (labels here are accepted as ASCII only); set it with the filesystem's own \
                     utility"
                ))
            } else if label.len() > 11 {
                Some(format!(
                    "vfat label exceeds the 11-byte on-disk field (got {} bytes)",
                    label.len()
                ))
            } else {
                None
            }
        }
        // 卷标字段 11 个 UTF-16 码元，超出时工具以 E2BIG 拒绝。
        ContentKind::Filesystem(FsKind::Exfat) => {
            let units = label.encode_utf16().count();
            (units > 11).then(|| {
                format!("exfat label exceeds the 11 UTF-16 unit field (got {units} units)")
            })
        }
        // volume_name[16] 留结尾 NUL，xstrncpy 至多复制 n-1 字节，故 15 字节是上限。
        ContentKind::Swap if label.len() > 15 => Some(format!(
            "swap label exceeds the 15-byte volume_name field (got {} bytes)",
            label.len()
        )),
        ContentKind::Filesystem(_)
        | ContentKind::Swap
        | ContentKind::Lvm2Pv
        | ContentKind::Luks
        | ContentKind::BitLocker
        | ContentKind::Unknown => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uuid_forms_follow_the_family_matrix() {
        for kind in [
            ContentKind::Filesystem(FsKind::Ext),
            ContentKind::Filesystem(FsKind::Xfs),
            ContentKind::Filesystem(FsKind::Btrfs),
        ] {
            assert_eq!(
                uuid_support(kind),
                UuidSupport::ExplicitAndGenerated,
                "{kind:?}"
            );
            assert!(uuid_program(kind, false).is_some());
            assert!(uuid_program(kind, true).is_some());
        }
        assert_eq!(uuid_support(ContentKind::Swap), UuidSupport::ExplicitOnly);
        assert!(uuid_program(ContentKind::Swap, false).is_some());
        assert!(uuid_program(ContentKind::Swap, true).is_none());

        assert_eq!(
            uuid_support(ContentKind::Filesystem(FsKind::Ntfs)),
            UuidSupport::GeneratedOnly
        );
        assert!(uuid_program(ContentKind::Filesystem(FsKind::Ntfs), true).is_some());
        assert!(
            uuid_program(ContentKind::Filesystem(FsKind::Ntfs), false).is_none(),
            "ntfs 只能生成 serial，指定值必须拒绝"
        );

        for kind in [
            ContentKind::Filesystem(FsKind::Vfat),
            ContentKind::Filesystem(FsKind::Exfat),
            ContentKind::Filesystem(FsKind::F2fs),
            ContentKind::Lvm2Pv,
            ContentKind::Unknown,
        ] {
            assert!(matches!(uuid_support(kind), UuidSupport::No(_)), "{kind:?}");
            assert!(uuid_program(kind, false).is_none(), "{kind:?}");
            assert!(uuid_program(kind, true).is_none(), "{kind:?}");
        }
        assert!(matches!(
            uuid_support(ContentKind::Luks),
            UuidSupport::No(why) if why.contains("cryptsetup")
        ));
        assert!(matches!(
            uuid_support(ContentKind::BitLocker),
            UuidSupport::No(why) if why.contains("manage-bde")
        ));
    }

    #[test]
    fn label_capacities_match_the_on_disk_fields() {
        assert!(label_problem(ContentKind::Filesystem(FsKind::Ext), &"x".repeat(16)).is_none());
        assert!(label_problem(ContentKind::Filesystem(FsKind::Ext), &"x".repeat(17)).is_some());

        assert!(label_problem(ContentKind::Filesystem(FsKind::Xfs), &"x".repeat(12)).is_none());
        assert!(label_problem(ContentKind::Filesystem(FsKind::Xfs), &"x".repeat(13)).is_some());

        assert!(label_problem(ContentKind::Filesystem(FsKind::Btrfs), &"x".repeat(255)).is_none());
        assert!(label_problem(ContentKind::Filesystem(FsKind::Btrfs), &"x".repeat(256)).is_some());

        assert!(label_problem(ContentKind::Filesystem(FsKind::Ntfs), &"x".repeat(128)).is_none());
        assert!(label_problem(ContentKind::Filesystem(FsKind::Ntfs), &"x".repeat(129)).is_some());

        assert!(label_problem(ContentKind::Filesystem(FsKind::Vfat), "LABEL").is_none());
        assert!(label_problem(ContentKind::Filesystem(FsKind::Vfat), &"x".repeat(12)).is_some());
        assert!(
            label_problem(ContentKind::Filesystem(FsKind::Vfat), "卷标").is_some(),
            "本实现只接受 ASCII 卷标"
        );

        assert!(label_problem(ContentKind::Filesystem(FsKind::Exfat), &"x".repeat(11)).is_none());
        assert!(label_problem(ContentKind::Filesystem(FsKind::Exfat), &"x".repeat(12)).is_some());

        assert!(label_problem(ContentKind::Swap, &"x".repeat(15)).is_none());
        assert!(label_problem(ContentKind::Swap, &"x".repeat(16)).is_some());

        // 没有卷标字段的内容不在这里拒绝：能力闸会先把它们拦下。
        assert!(label_problem(ContentKind::Lvm2Pv, "anything").is_none());
    }

    #[test]
    fn label_programs_come_from_the_attrs_column() {
        assert_eq!(
            label_program(ContentKind::Filesystem(FsKind::Ext)),
            Some(Program::Tune2fs)
        );
        assert_eq!(
            label_program(ContentKind::Filesystem(FsKind::Vfat)),
            Some(Program::Fatlabel)
        );
        assert_eq!(label_program(ContentKind::Swap), Some(Program::Swaplabel));
        assert_eq!(label_program(ContentKind::Lvm2Pv), None);
        assert_eq!(label_program(ContentKind::Unknown), None);
    }
}
