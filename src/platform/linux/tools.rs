//! 外部程序身份到可执行名与固定参数的唯一映射。
//!
//! 程序身份来自纯核的档案表；可执行名、固定前缀参数与只读检查参数只在这里出现。
//! 上层只说"要运行哪个程序、目标是什么"，不拼 argv。

use diskedit_core::fs::Program;

/// 可执行名。
pub fn executable(program: Program) -> &'static str {
    match program {
        Program::E2fsck => "e2fsck",
        Program::Resize2fs => "resize2fs",
        Program::Dumpe2fs => "dumpe2fs",
        Program::Mke2fs => "mke2fs",
        Program::Tune2fs => "tune2fs",
        Program::Ntfsresize => "ntfsresize",
        Program::Ntfsfix => "ntfsfix",
        Program::Ntfslabel => "ntfslabel",
        Program::FsckF2fs => "fsck.f2fs",
        Program::ResizeF2fs => "resize.f2fs",
        Program::XfsGrowfs => "xfs_growfs",
        Program::XfsRepair => "xfs_repair",
        Program::XfsAdmin => "xfs_admin",
        Program::Btrfs => "btrfs",
        Program::Btrfstune => "btrfstune",
        Program::Fatresize => "fatresize",
        Program::Fatlabel => "fatlabel",
        Program::FsckVfat => "fsck.vfat",
        Program::Exfatlabel => "exfatlabel",
        Program::FsckExfat => "fsck.exfat",
        Program::Mkswap => "mkswap",
        Program::Swaplabel => "swaplabel",
        Program::Pvs => "pvs",
        Program::Lvs => "lvs",
        Program::Vgs => "vgs",
        Program::Pvresize => "pvresize",
        Program::Lvextend => "lvextend",
        Program::MkfsVfat => "mkfs.vfat",
        Program::MkfsExfat => "mkfs.exfat",
        Program::MkfsNtfs => "mkfs.ntfs",
        Program::MkfsXfs => "mkfs.xfs",
        Program::MkfsBtrfs => "mkfs.btrfs",
        Program::MkfsF2fs => "mkfs.f2fs",
        Program::Mount => "mount",
        Program::Umount => "umount",
        Program::Losetup => "losetup",
        Program::Sfdisk => "sfdisk",
        Program::Partx => "partx",
        Program::Blkid => "blkid",
    }
}

/// 只读检查的固定前缀参数。
///
/// 只放"这项检查必须带上、与目标无关"的旗标；目标路径由调用方追加在末尾。
pub fn check_arguments(program: Program) -> &'static [&'static str] {
    match program {
        Program::E2fsck => &["-f", "-n"],
        Program::XfsRepair => &["-n"],
        Program::Btrfs => &["check", "--readonly"],
        Program::FsckVfat => &["-n"],
        Program::FsckExfat => &["-n"],
        Program::Ntfsfix => &["-n"],
        Program::Resize2fs
        | Program::FsckF2fs
        | Program::Dumpe2fs
        | Program::Mke2fs
        | Program::Tune2fs
        | Program::Ntfsresize
        | Program::Ntfslabel
        | Program::ResizeF2fs
        | Program::XfsGrowfs
        | Program::XfsAdmin
        | Program::Btrfstune
        | Program::Fatresize
        | Program::Fatlabel
        | Program::Exfatlabel
        | Program::Mkswap
        | Program::Swaplabel
        | Program::Pvs
        | Program::Lvs
        | Program::Vgs
        | Program::Pvresize
        | Program::Lvextend
        | Program::MkfsVfat
        | Program::MkfsExfat
        | Program::MkfsNtfs
        | Program::MkfsXfs
        | Program::MkfsBtrfs
        | Program::MkfsF2fs
        | Program::Mount
        | Program::Umount
        | Program::Losetup
        | Program::Sfdisk
        | Program::Partx
        | Program::Blkid => &[],
    }
}

/// 在 `PATH` 中查找可执行文件。
pub fn locate(program: Program) -> Option<std::path::PathBuf> {
    let name = executable(program);
    let paths = std::env::var_os("PATH")?;
    for directory in std::env::split_paths(&paths) {
        let candidate = directory.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}
