//! 外部程序身份。
//!
//! 这里只记录"要做这件事需要哪个程序"，不记录可执行名、路径、参数拼装或调用
//! 方式。程序表按执行顺序摆放，是档案表的一部分，不是第二份能力清单；平台实现
//! 负责把身份映射到具体可执行文件。

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
    Btrfstune,
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
    Blkid,
}

impl Program {
    /// 全部程序，用于名称唯一性与可执行名对账。
    pub const ALL: [Program; 38] = [
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
        Program::Btrfstune,
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
        Program::Blkid,
    ];

    /// 程序身份已登记齐全。
    pub const COUNT: usize = 38;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_program_identity_is_registered() {
        assert_eq!(Program::ALL.len(), Program::COUNT);
    }
}
