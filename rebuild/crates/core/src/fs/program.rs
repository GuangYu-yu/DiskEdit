//! 外部程序标识。
//!
//! 这里只记录"要做这件事需要哪个程序"，不决定路径、参数拼装或调用方式；
//! 程序表按执行顺序摆放，是档案表的一部分，不是第二份能力清单。

/// 档案表引用的外部程序。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Program {
    E2fsck,
    Resize2fs,
    Dumpe2fs,
    Mke2fs,
    Tune2fs,
    Ntfsresize,
    Ntfsfix,
    Ntfslabel,
    FsckF2fs,
    ResizeF2fs,
    XfsGrowfs,
    XfsRepair,
    XfsAdmin,
    Btrfs,
    Fatresize,
    Fatlabel,
    FsckVfat,
    Exfatlabel,
    FsckExfat,
    Mkswap,
    Swaplabel,
    Pvs,
    Lvs,
    Vgs,
    Pvresize,
    MkfsVfat,
    MkfsExfat,
    MkfsNtfs,
    MkfsXfs,
    MkfsBtrfs,
    MkfsF2fs,
    Mount,
    Umount,
    Losetup,
    Sfdisk,
    Partx,
}

impl Program {
    /// 全部程序，用于名称唯一性与可执行名对账。
    pub const ALL: [Program; 36] = [
        Program::E2fsck,
        Program::Resize2fs,
        Program::Dumpe2fs,
        Program::Mke2fs,
        Program::Tune2fs,
        Program::Ntfsresize,
        Program::Ntfsfix,
        Program::Ntfslabel,
        Program::FsckF2fs,
        Program::ResizeF2fs,
        Program::XfsGrowfs,
        Program::XfsRepair,
        Program::XfsAdmin,
        Program::Btrfs,
        Program::Fatresize,
        Program::Fatlabel,
        Program::FsckVfat,
        Program::Exfatlabel,
        Program::FsckExfat,
        Program::Mkswap,
        Program::Swaplabel,
        Program::Pvs,
        Program::Lvs,
        Program::Vgs,
        Program::Pvresize,
        Program::MkfsVfat,
        Program::MkfsExfat,
        Program::MkfsNtfs,
        Program::MkfsXfs,
        Program::MkfsBtrfs,
        Program::MkfsF2fs,
        Program::Mount,
        Program::Umount,
        Program::Losetup,
        Program::Sfdisk,
        Program::Partx,
    ];

    /// 可执行文件名。
    pub const fn executable(self) -> &'static str {
        match self {
            Self::E2fsck => "e2fsck",
            Self::Resize2fs => "resize2fs",
            Self::Dumpe2fs => "dumpe2fs",
            Self::Mke2fs => "mke2fs",
            Self::Tune2fs => "tune2fs",
            Self::Ntfsresize => "ntfsresize",
            Self::Ntfsfix => "ntfsfix",
            Self::Ntfslabel => "ntfslabel",
            Self::FsckF2fs => "fsck.f2fs",
            Self::ResizeF2fs => "resize.f2fs",
            Self::XfsGrowfs => "xfs_growfs",
            Self::XfsRepair => "xfs_repair",
            Self::XfsAdmin => "xfs_admin",
            Self::Btrfs => "btrfs",
            Self::Fatresize => "fatresize",
            Self::Fatlabel => "fatlabel",
            Self::FsckVfat => "fsck.fat",
            Self::Exfatlabel => "exfatlabel",
            Self::FsckExfat => "fsck.exfat",
            Self::Mkswap => "mkswap",
            Self::Swaplabel => "swaplabel",
            Self::Pvs => "pvs",
            Self::Lvs => "lvs",
            Self::Vgs => "vgs",
            Self::Pvresize => "pvresize",
            Self::MkfsVfat => "mkfs.vfat",
            Self::MkfsExfat => "mkfs.exfat",
            Self::MkfsNtfs => "mkntfs",
            Self::MkfsXfs => "mkfs.xfs",
            Self::MkfsBtrfs => "mkfs.btrfs",
            Self::MkfsF2fs => "mkfs.f2fs",
            Self::Mount => "mount",
            Self::Umount => "umount",
            Self::Losetup => "losetup",
            Self::Sfdisk => "sfdisk",
            Self::Partx => "partx",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn executable_names_are_unique_and_non_empty() {
        let mut names: Vec<&str> = Program::ALL.iter().map(|p| p.executable()).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), Program::ALL.len());
        assert!(Program::ALL.iter().all(|p| !p.executable().is_empty()));
    }
}
