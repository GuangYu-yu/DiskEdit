//! 最小尺寸探针声明。
//!
//! 这里只声明"用哪个程序、带什么参数、读哪条规则"，不执行程序也不解析真实输出。
//! 缩容判定需要的是"该族能不能给出可信的最小尺寸"，因此没有探针的族显式记为
//! 不支持，而不是留一个会取默认值的空实现。

use diskedit_core::fs::{ContentKind, FsKind, Program, profile};

/// 最小尺寸结论的读取规则。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MinSizeRule {
    /// 运行程序，按给定单位读取输出中的数字。
    Tool {
        program: Program,
        args: &'static [&'static str],
        unit: Unit,
    },
    /// 该族没有可信的最小尺寸探针。
    Unavailable { why: &'static str },
}

/// 输出数字的单位。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unit {
    Bytes,
    KiB,
}

const RESIZE2FS: &[&str] = &["-P"];
const NTFSRESIZE: &[&str] = &["--info", "--force", "--no-action"];
const BTRFS_MIN: &[&str] = &["inspect-internal", "min-dev-size"];

/// 某一内容类别的最小尺寸规则。
pub fn min_size_rule(kind: ContentKind) -> MinSizeRule {
    match kind {
        ContentKind::Filesystem(FsKind::Ext) => MinSizeRule::Tool {
            program: Program::Resize2fs,
            args: RESIZE2FS,
            unit: Unit::KiB,
        },
        ContentKind::Filesystem(FsKind::Ntfs) => MinSizeRule::Tool {
            program: Program::Ntfsresize,
            args: NTFSRESIZE,
            unit: Unit::Bytes,
        },
        ContentKind::Filesystem(FsKind::Btrfs) => MinSizeRule::Tool {
            program: Program::Btrfs,
            args: BTRFS_MIN,
            unit: Unit::Bytes,
        },
        ContentKind::Filesystem(FsKind::Xfs)
        | ContentKind::Filesystem(FsKind::F2fs)
        | ContentKind::Filesystem(FsKind::Vfat)
        | ContentKind::Filesystem(FsKind::Exfat)
        | ContentKind::Filesystem(FsKind::Squashfs)
        | ContentKind::Filesystem(FsKind::Erofs)
        | ContentKind::Filesystem(FsKind::HfsPlus)
        | ContentKind::Filesystem(FsKind::Apfs)
        | ContentKind::Filesystem(FsKind::Bcachefs)
        | ContentKind::Filesystem(FsKind::Jfs)
        | ContentKind::Filesystem(FsKind::Reiserfs)
        | ContentKind::Filesystem(FsKind::Udf) => MinSizeRule::Unavailable {
            why: "this family has no trustworthy minimum size probe; shrink is treated as rebuild",
        },
        ContentKind::Swap | ContentKind::Lvm2Pv | ContentKind::Luks | ContentKind::BitLocker => {
            MinSizeRule::Unavailable {
                why: "container or swap area has no filesystem-level minimum size",
            }
        }
        ContentKind::Unknown => MinSizeRule::Unavailable {
            why: "unknown content kind; cannot give a minimum size",
        },
    }
}

/// 该族是否声明了缩容探针。
pub fn has_probe(kind: ContentKind) -> bool {
    matches!(min_size_rule(kind), MinSizeRule::Tool { .. })
}

/// 档案表对该族是否声明了缩容能力；与探针声明必须同时成立才可走缩容。
pub fn shrink_declared(kind: ContentKind) -> bool {
    profile(kind).shrink.is_supported()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_content_kind_has_a_declaration() {
        for kind in ContentKind::ALL {
            let rule = min_size_rule(kind);
            if let MinSizeRule::Unavailable { why } = rule {
                assert!(!why.is_empty(), "{kind:?} 的不支持理由不得为空");
            }
        }
    }

    #[test]
    fn probes_match_declared_shrink_capability() {
        assert!(has_probe(ContentKind::Filesystem(FsKind::Ext)));
        assert!(shrink_declared(ContentKind::Filesystem(FsKind::Ext)));
        assert!(!has_probe(ContentKind::Filesystem(FsKind::Xfs)));
        assert!(!has_probe(ContentKind::Swap));
    }
}
