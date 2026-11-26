//! 外部程序身份。
//!
//! 这里只记录"要做这件事需要哪个程序"，不记录可执行名、路径、参数拼装或调用
//! 方式。程序表按执行顺序摆放，是档案表的一部分，不是第二份能力清单；平台实现
//! 负责把身份映射到具体可执行文件。

/// 程序对目标的访问方式。
///
/// 由程序身份唯一决定，调用点不能自行声明：写类工具无论带什么旗标都按写类处理，
/// 只读探针不会因为被放进写链就获得写权限。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Access {
    /// 只读：不改变目标字节，也不改变内核视图。
    ReadOnly,
    /// 写目标字节。
    WritesTarget,
    /// 改变目标之外的状态（内核分区视图、loop 绑定、挂载）。
    WritesExternalState,
}

/// 同一程序以相同参数重跑是否必然得到相同字节。
///
/// 建卷与重建 swap 会写入当前时间、随机 UUID 等，重跑得到不同字节；检查、扩缩与
/// 打标签只由输入决定。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Determinism {
    /// 输出只由输入决定。
    Deterministic,
    /// 输出含时间、随机源等不可复现成分。
    Nondeterministic,
}

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

    /// 该程序对目标的访问方式。写类工具因此无法在不持写令牌的调用点被执行。
    pub const fn access(self) -> Access {
        match self {
            Self::Dumpe2fs | Self::Blkid | Self::Pvs | Self::Lvs | Self::Vgs => Access::ReadOnly,
            Self::Losetup | Self::Sfdisk | Self::Partx | Self::Mount | Self::Umount => {
                Access::WritesExternalState
            }
            Self::E2fsck
            | Self::Resize2fs
            | Self::Mke2fs
            | Self::Tune2fs
            | Self::Ntfsresize
            | Self::Ntfsfix
            | Self::Ntfslabel
            | Self::FsckF2fs
            | Self::ResizeF2fs
            | Self::XfsGrowfs
            | Self::XfsRepair
            | Self::XfsAdmin
            | Self::Btrfs
            | Self::Btrfstune
            | Self::Fatresize
            | Self::Fatlabel
            | Self::FsckVfat
            | Self::Exfatlabel
            | Self::FsckExfat
            | Self::Mkswap
            | Self::Swaplabel
            | Self::Pvresize
            | Self::MkfsVfat
            | Self::MkfsExfat
            | Self::MkfsNtfs
            | Self::MkfsXfs
            | Self::MkfsBtrfs
            | Self::MkfsF2fs => Access::WritesTarget,
        }
    }

    /// 同参重跑是否必然得到相同字节。
    pub const fn determinism(self) -> Determinism {
        match self {
            Self::Mke2fs
            | Self::MkfsVfat
            | Self::MkfsExfat
            | Self::MkfsNtfs
            | Self::MkfsXfs
            | Self::MkfsBtrfs
            | Self::MkfsF2fs
            | Self::Mkswap => Determinism::Nondeterministic,
            Self::E2fsck
            | Self::Resize2fs
            | Self::Dumpe2fs
            | Self::Tune2fs
            | Self::Ntfsresize
            | Self::Ntfsfix
            | Self::Ntfslabel
            | Self::FsckF2fs
            | Self::ResizeF2fs
            | Self::XfsGrowfs
            | Self::XfsRepair
            | Self::XfsAdmin
            | Self::Btrfs
            | Self::Btrfstune
            | Self::Fatresize
            | Self::Fatlabel
            | Self::FsckVfat
            | Self::Exfatlabel
            | Self::FsckExfat
            | Self::Swaplabel
            | Self::Pvs
            | Self::Lvs
            | Self::Vgs
            | Self::Pvresize
            | Self::Mount
            | Self::Umount
            | Self::Losetup
            | Self::Sfdisk
            | Self::Partx
            | Self::Blkid => Determinism::Deterministic,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_program_identity_is_registered() {
        assert_eq!(Program::ALL.len(), Program::COUNT);
    }

    // covers: I9
    #[test]
    fn every_program_declares_an_access_and_a_determinism() {
        // 注册表的每一行都必须落在两类里：访问方式决定是否需要写令牌，
        // 确定性决定重跑能否被当作同一结果的来源。
        for program in Program::ALL {
            let access = program.access();
            let determinism = program.determinism();
            assert!(
                matches!(
                    access,
                    Access::ReadOnly | Access::WritesTarget | Access::WritesExternalState
                ),
                "{program:?} 没有访问方式"
            );
            assert!(
                matches!(
                    determinism,
                    Determinism::Deterministic | Determinism::Nondeterministic
                ),
                "{program:?} 没有确定性分类"
            );
        }
        // 只读探针不写目标；建卷与重建 swap 含不可复现成分。
        assert_eq!(Program::Dumpe2fs.access(), Access::ReadOnly);
        assert_eq!(Program::Resize2fs.access(), Access::WritesTarget);
        assert_eq!(Program::Mount.access(), Access::WritesExternalState);
        assert_eq!(Program::Mke2fs.determinism(), Determinism::Nondeterministic);
        assert_eq!(Program::Resize2fs.determinism(), Determinism::Deterministic);
    }
}
