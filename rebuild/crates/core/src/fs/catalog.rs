//! 每类内容一行的能力档案。
//!
//! 档案是数据：五列能力各给出可用的程序序列、不适用或带原因的拒绝。
//! [`profile`] 对 [`ContentKind`] 的匹配是穷尽的——新增种类不写表就无法编译。
//! `Support::Tools` 里的程序按执行顺序排列，这就是该能力的步骤程序。

use super::profile::{ContentKind, FsKind};
use super::program::Program;

/// 一项能力。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Support {
    /// 需要这些程序，按执行顺序排列。
    Tools(&'static [Program]),
    /// 本操作对该内容没有可作用的对象，不是失败。
    NotApplicable,
    /// 认得出来但本实现不做，理由随变体携带。
    Unsupported(&'static str),
}

impl Support {
    /// 需要的程序序列；不适用与不支持都没有程序。
    pub const fn tools(self) -> Option<&'static [Program]> {
        match self {
            Self::Tools(tools) => Some(tools),
            Self::NotApplicable | Self::Unsupported(_) => None,
        }
    }

    /// 能否由本实现执行。
    pub const fn is_supported(self) -> bool {
        matches!(self, Self::Tools(_) | Self::NotApplicable)
    }

    pub const fn reason(self) -> Option<&'static str> {
        match self {
            Self::Tools(_) | Self::NotApplicable => None,
            Self::Unsupported(why) => Some(why),
        }
    }
}

/// 操作方向。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verb {
    Grow,
    Shrink,
    Attrs,
    Check,
    Mkfs,
}

/// 挂载要求。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MountNeed {
    /// 不需要挂载。
    Never,
    /// 扩容需要挂载，收缩另行拒绝。
    Grow,
    /// 扩容与收缩都需要挂载。
    GrowAndShrink,
}

/// 收缩的设备范围。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShrinkScope {
    Any,
    /// 只有单设备才可收缩。
    SingleDeviceOnly,
}

/// 在已有挂载上扩容文件系统的方式。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OnlineGrow {
    /// 程序以分区设备为参数。
    Device(Program),
    /// 程序以挂载点为参数。
    MountPoint(Program),
    /// 该内容没有在线扩容方式，理由随变体携带。
    Unsupported(&'static str),
}

impl OnlineGrow {
    /// 需要的程序；不支持时为 `None`。
    pub const fn program(self) -> Option<Program> {
        match self {
            Self::Device(program) | Self::MountPoint(program) => Some(program),
            Self::Unsupported(_) => None,
        }
    }

    /// 不支持的原因；支持时为 `None`。
    pub const fn reason(self) -> Option<&'static str> {
        match self {
            Self::Unsupported(why) => Some(why),
            Self::Device(_) | Self::MountPoint(_) => None,
        }
    }

    /// 程序是否需要挂载点而不是设备节点。
    pub const fn takes_mount_point(self) -> bool {
        matches!(self, Self::MountPoint(_))
    }
}

/// 一类内容的能力档案。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Profile {
    pub grow: Support,
    pub shrink: Support,
    pub attrs: Support,
    pub check: Support,
    pub mkfs: Support,
    pub mount: MountNeed,
    pub shrink_scope: ShrinkScope,
}

impl Profile {
    /// 按方向取对应能力。
    pub const fn support(&self, verb: Verb) -> Support {
        match verb {
            Verb::Grow => self.grow,
            Verb::Shrink => self.shrink,
            Verb::Attrs => self.attrs,
            Verb::Check => self.check,
            Verb::Mkfs => self.mkfs,
        }
    }
}

const EXT_RESIZE: &[Program] = &[Program::E2fsck, Program::Resize2fs, Program::Dumpe2fs];
const NTFS_RESIZE: &[Program] = &[Program::Ntfsresize];
const BTRFS: &[Program] = &[Program::Btrfs];
const F2FS_GROW: &[Program] = &[Program::FsckF2fs, Program::ResizeF2fs];
const XFS_GROW: &[Program] = &[Program::XfsGrowfs];
const VFAT_GROW: &[Program] = &[Program::Fatresize];
const LVM_SHRINK: &[Program] = &[Program::Pvs, Program::Vgs, Program::Pvresize];
const SWAP_REBUILD: &[Program] = &[Program::Mkswap];

const READ_ONLY: &str = "read-only identification; this build does not modify this filesystem";
const LUKS_GROW: &str = "LUKS container — open it, grow the inner filesystem on the mapped device, then grow the partition (cryptsetup)";
const LUKS_SHRINK: &str = "LUKS container — shrink the inner filesystem on the mapped device and resize the LUKS header first (cryptsetup)";
const LUKS_MKFS: &str = "LUKS containers are created with cryptsetup, not mkfs";
const BITLOCKER: &str = "BitLocker volume — resize it with Windows tools (manage-bde) or dislocker";
const BITLOCKER_MKFS: &str = "BitLocker volumes are created by Windows, not mkfs";
const UNRECOGNIZED: &str =
    "filesystem type unrecognized — acting on it without identifying it would corrupt data";
const NO_ATTRS: &str = "this content has no filesystem attributes";
const NO_CHECK: &str = "no consistency check is wired for this content";
const LVM_MKFS: &str = "LVM2 PVs are created with pvcreate, not mkfs";

/// 一类内容的档案。匹配穷尽，新增 [`ContentKind`] 分量必须在此给出结论。
pub const fn profile(kind: ContentKind) -> Profile {
    match kind {
        ContentKind::Filesystem(FsKind::Ext) => Profile {
            grow: Support::Tools(EXT_RESIZE),
            shrink: Support::Tools(EXT_RESIZE),
            attrs: Support::Tools(&[Program::Tune2fs]),
            check: Support::Tools(&[Program::E2fsck]),
            mkfs: Support::Tools(&[Program::Mke2fs]),
            mount: MountNeed::Never,
            shrink_scope: ShrinkScope::Any,
        },
        ContentKind::Filesystem(FsKind::Xfs) => Profile {
            grow: Support::Tools(XFS_GROW),
            shrink: Support::Unsupported(
                "xfs cannot shrink; rebuild the filesystem at the smaller size instead",
            ),
            attrs: Support::Tools(&[Program::XfsAdmin]),
            check: Support::Tools(&[Program::XfsRepair]),
            mkfs: Support::Tools(&[Program::MkfsXfs]),
            mount: MountNeed::Grow,
            shrink_scope: ShrinkScope::Any,
        },
        ContentKind::Filesystem(FsKind::Btrfs) => Profile {
            grow: Support::Tools(BTRFS),
            shrink: Support::Tools(BTRFS),
            attrs: Support::Tools(BTRFS),
            check: Support::Tools(BTRFS),
            mkfs: Support::Tools(&[Program::MkfsBtrfs]),
            mount: MountNeed::GrowAndShrink,
            shrink_scope: ShrinkScope::SingleDeviceOnly,
        },
        ContentKind::Filesystem(FsKind::F2fs) => Profile {
            grow: Support::Tools(F2FS_GROW),
            shrink: Support::Unsupported("f2fs shrink is not wired to a tool"),
            attrs: Support::Unsupported("f2fs attributes are not wired to a tool"),
            check: Support::Unsupported("the f2fs checker has no read-only mode"),
            mkfs: Support::Tools(&[Program::MkfsF2fs]),
            mount: MountNeed::Never,
            shrink_scope: ShrinkScope::Any,
        },
        ContentKind::Filesystem(FsKind::Ntfs) => Profile {
            grow: Support::Tools(NTFS_RESIZE),
            shrink: Support::Tools(NTFS_RESIZE),
            attrs: Support::Tools(&[Program::Ntfslabel]),
            check: Support::Tools(&[Program::Ntfsfix]),
            mkfs: Support::Tools(&[Program::MkfsNtfs]),
            mount: MountNeed::Never,
            shrink_scope: ShrinkScope::Any,
        },
        ContentKind::Filesystem(FsKind::Vfat) => Profile {
            grow: Support::Tools(VFAT_GROW),
            shrink: Support::Unsupported("vfat shrink is not wired to a tool"),
            attrs: Support::Tools(&[Program::Fatlabel]),
            check: Support::Tools(&[Program::FsckVfat]),
            mkfs: Support::Tools(&[Program::MkfsVfat]),
            mount: MountNeed::Never,
            shrink_scope: ShrinkScope::Any,
        },
        ContentKind::Filesystem(FsKind::Exfat) => Profile {
            grow: Support::Unsupported("exfat grow is not wired to a tool"),
            shrink: Support::Unsupported("exfat shrink is not wired to a tool"),
            attrs: Support::Tools(&[Program::Exfatlabel]),
            check: Support::Tools(&[Program::FsckExfat]),
            mkfs: Support::Tools(&[Program::MkfsExfat]),
            mount: MountNeed::Never,
            shrink_scope: ShrinkScope::Any,
        },
        ContentKind::Filesystem(
            FsKind::Squashfs
            | FsKind::Erofs
            | FsKind::HfsPlus
            | FsKind::Apfs
            | FsKind::Bcachefs
            | FsKind::Jfs
            | FsKind::Reiserfs
            | FsKind::Udf,
        ) => Profile {
            grow: Support::Unsupported(READ_ONLY),
            shrink: Support::Unsupported(READ_ONLY),
            attrs: Support::Unsupported(READ_ONLY),
            check: Support::Unsupported(READ_ONLY),
            mkfs: Support::Unsupported(READ_ONLY),
            mount: MountNeed::Never,
            shrink_scope: ShrinkScope::Any,
        },
        ContentKind::Swap => Profile {
            grow: Support::Tools(SWAP_REBUILD),
            shrink: Support::Unsupported("swap 区域没有原地收缩；按目标尺寸重建"),
            attrs: Support::Tools(&[Program::Swaplabel]),
            check: Support::Unsupported("swap has no filesystem check"),
            mkfs: Support::Tools(SWAP_REBUILD),
            mount: MountNeed::Never,
            shrink_scope: ShrinkScope::Any,
        },
        ContentKind::Lvm2Pv => Profile {
            grow: Support::NotApplicable,
            shrink: Support::Tools(LVM_SHRINK),
            attrs: Support::Unsupported(NO_ATTRS),
            check: Support::Unsupported("LVM2 PV metadata is checked with pvck, not fsck"),
            mkfs: Support::Unsupported(LVM_MKFS),
            mount: MountNeed::Never,
            shrink_scope: ShrinkScope::Any,
        },
        ContentKind::Luks => Profile {
            grow: Support::Unsupported(LUKS_GROW),
            shrink: Support::Unsupported(LUKS_SHRINK),
            attrs: Support::Unsupported(NO_ATTRS),
            check: Support::Unsupported(NO_CHECK),
            mkfs: Support::Unsupported(LUKS_MKFS),
            mount: MountNeed::Never,
            shrink_scope: ShrinkScope::Any,
        },
        ContentKind::BitLocker => Profile {
            grow: Support::Unsupported(BITLOCKER),
            shrink: Support::Unsupported(BITLOCKER),
            attrs: Support::Unsupported(NO_ATTRS),
            check: Support::Unsupported(NO_CHECK),
            mkfs: Support::Unsupported(BITLOCKER_MKFS),
            mount: MountNeed::Never,
            shrink_scope: ShrinkScope::Any,
        },
        ContentKind::Unknown => Profile {
            grow: Support::Unsupported(UNRECOGNIZED),
            shrink: Support::Unsupported(UNRECOGNIZED),
            attrs: Support::Unsupported(UNRECOGNIZED),
            check: Support::Unsupported(UNRECOGNIZED),
            mkfs: Support::Unsupported(UNRECOGNIZED),
            mount: MountNeed::Never,
            shrink_scope: ShrinkScope::Any,
        },
    }
}

/// 一类内容的在线扩容方式。匹配穷尽，新增 [`ContentKind`] 分量必须在此给出结论。
pub const fn online_grow(kind: ContentKind) -> OnlineGrow {
    match kind {
        ContentKind::Filesystem(FsKind::Ext) => OnlineGrow::Device(Program::Resize2fs),
        ContentKind::Filesystem(FsKind::Xfs) => OnlineGrow::MountPoint(Program::XfsGrowfs),
        ContentKind::Filesystem(FsKind::Btrfs) => OnlineGrow::MountPoint(Program::Btrfs),
        ContentKind::Filesystem(
            FsKind::Squashfs
            | FsKind::Erofs
            | FsKind::F2fs
            | FsKind::Exfat
            | FsKind::Vfat
            | FsKind::Ntfs
            | FsKind::HfsPlus
            | FsKind::Apfs
            | FsKind::Bcachefs
            | FsKind::Jfs
            | FsKind::Reiserfs
            | FsKind::Udf,
        ) => OnlineGrow::Unsupported(
            "this filesystem has no online grow tool; unmount and grow it through a target form",
        ),
        ContentKind::Swap => {
            OnlineGrow::Unsupported("swap has no online grow; rebuild it at the target size")
        }
        ContentKind::Lvm2Pv => OnlineGrow::Unsupported(
            "LVM2 PVs are grown by the physical-volume path, not by a mounted-filesystem resize",
        ),
        ContentKind::Luks => OnlineGrow::Unsupported(LUKS_GROW),
        ContentKind::BitLocker => OnlineGrow::Unsupported(BITLOCKER),
        ContentKind::Unknown => OnlineGrow::Unsupported(UNRECOGNIZED),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tools(kind: ContentKind, verb: Verb) -> Option<&'static [Program]> {
        profile(kind).support(verb).tools()
    }

    // covers: I6
    #[test]
    fn every_content_kind_has_a_complete_row() {
        assert_eq!(ContentKind::ALL.len(), 20);
        for kind in ContentKind::ALL {
            let row = profile(kind);
            // 五列能力都是合法形态；穷尽匹配保证没有缺席格。
            let all = [row.grow, row.shrink, row.attrs, row.check, row.mkfs];
            assert_eq!(all.len(), 5);
        }
    }

    #[test]
    fn ext_resize_program_runs_check_resize_then_verify() {
        assert_eq!(
            tools(ContentKind::Filesystem(FsKind::Ext), Verb::Grow),
            Some(EXT_RESIZE)
        );
        assert_eq!(
            tools(ContentKind::Filesystem(FsKind::Ext), Verb::Shrink),
            Some(EXT_RESIZE)
        );
        assert_eq!(EXT_RESIZE[0], Program::E2fsck);
        assert_eq!(EXT_RESIZE[1], Program::Resize2fs);
        assert_eq!(EXT_RESIZE[2], Program::Dumpe2fs);
    }

    #[test]
    fn xfs_grows_mounted_and_never_shrinks() {
        let row = profile(ContentKind::Filesystem(FsKind::Xfs));
        assert_eq!(row.mount, MountNeed::Grow);
        assert_eq!(row.grow, Support::Tools(XFS_GROW));
        assert!(!row.shrink.is_supported());
    }

    #[test]
    fn btrfs_shrinks_only_single_device_and_mounts_both_ways() {
        let row = profile(ContentKind::Filesystem(FsKind::Btrfs));
        assert_eq!(row.mount, MountNeed::GrowAndShrink);
        assert_eq!(row.shrink_scope, ShrinkScope::SingleDeviceOnly);
        assert_eq!(row.shrink, Support::Tools(BTRFS));
    }

    #[test]
    fn lvm_shrink_proves_extents_before_resizing() {
        assert_eq!(tools(ContentKind::Lvm2Pv, Verb::Shrink), Some(LVM_SHRINK));
        assert_eq!(LVM_SHRINK[0], Program::Pvs);
        assert_eq!(LVM_SHRINK[1], Program::Vgs);
        assert_eq!(LVM_SHRINK[2], Program::Pvresize);
        assert_eq!(profile(ContentKind::Lvm2Pv).grow, Support::NotApplicable);
    }

    #[test]
    fn swap_is_rebuilt_rather_than_resized_in_place() {
        let row = profile(ContentKind::Swap);
        assert_eq!(row.grow, Support::Tools(SWAP_REBUILD));
        assert_eq!(row.mkfs, Support::Tools(SWAP_REBUILD));
        assert!(!row.shrink.is_supported());
    }

    #[test]
    fn read_only_families_are_rejected_everywhere() {
        for kind in [
            FsKind::Squashfs,
            FsKind::Erofs,
            FsKind::HfsPlus,
            FsKind::Apfs,
            FsKind::Bcachefs,
            FsKind::Jfs,
            FsKind::Reiserfs,
            FsKind::Udf,
        ] {
            let row = profile(ContentKind::Filesystem(kind));
            for verb in [
                Verb::Grow,
                Verb::Shrink,
                Verb::Attrs,
                Verb::Check,
                Verb::Mkfs,
            ] {
                assert!(
                    !row.support(verb).is_supported(),
                    "{} {verb:?}",
                    kind.as_str()
                );
            }
        }
    }

    #[test]
    fn unknown_and_encryption_layers_are_rejected() {
        for kind in [
            ContentKind::Luks,
            ContentKind::BitLocker,
            ContentKind::Unknown,
        ] {
            let row = profile(kind);
            for verb in [
                Verb::Grow,
                Verb::Shrink,
                Verb::Attrs,
                Verb::Check,
                Verb::Mkfs,
            ] {
                assert!(!row.support(verb).is_supported(), "{kind:?} {verb:?}");
            }
        }
    }

    #[test]
    fn only_the_mountable_families_grow_online() {
        assert_eq!(
            online_grow(ContentKind::Filesystem(FsKind::Ext)),
            OnlineGrow::Device(Program::Resize2fs)
        );
        assert_eq!(
            online_grow(ContentKind::Filesystem(FsKind::Xfs)),
            OnlineGrow::MountPoint(Program::XfsGrowfs)
        );
        assert_eq!(
            online_grow(ContentKind::Filesystem(FsKind::Btrfs)),
            OnlineGrow::MountPoint(Program::Btrfs)
        );
        for kind in [
            ContentKind::Filesystem(FsKind::Ntfs),
            ContentKind::Swap,
            ContentKind::Unknown,
        ] {
            assert!(online_grow(kind).program().is_none(), "{kind:?}");
        }
    }

    #[test]
    fn every_referenced_program_is_registered() {
        for kind in ContentKind::ALL {
            let row = profile(kind);
            for verb in [
                Verb::Grow,
                Verb::Shrink,
                Verb::Attrs,
                Verb::Check,
                Verb::Mkfs,
            ] {
                if let Some(tools) = row.support(verb).tools() {
                    for program in tools {
                        assert!(
                            Program::ALL.contains(program),
                            "{program:?} referenced by {kind:?} {verb:?} is not registered"
                        );
                    }
                }
            }
        }
    }
}
