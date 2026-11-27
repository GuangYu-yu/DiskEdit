//! 文件系统种类与磁盘内容类别。
//!
//! 识别域只到"族"：`ext` 家族不区分 2/3/4；建卷请求另有点名的目标类型。
//! 名称表只在本模块维护，`as_str` 与 `from_name` 共用同一份枚举顺序。

/// 可识别的文件系统族。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum FsKind {
    Squashfs,
    Erofs,
    Ext,
    Xfs,
    Btrfs,
    F2fs,
    Exfat,
    Vfat,
    Ntfs,
    HfsPlus,
    Apfs,
    Bcachefs,
    Jfs,
    Reiserfs,
    Udf,
}

impl FsKind {
    /// 全部识别族，顺序与名称表一致。
    pub const ALL: [FsKind; 15] = [
        FsKind::Squashfs,
        FsKind::Erofs,
        FsKind::Ext,
        FsKind::Xfs,
        FsKind::Btrfs,
        FsKind::F2fs,
        FsKind::Exfat,
        FsKind::Vfat,
        FsKind::Ntfs,
        FsKind::HfsPlus,
        FsKind::Apfs,
        FsKind::Bcachefs,
        FsKind::Jfs,
        FsKind::Reiserfs,
        FsKind::Udf,
    ];

    /// 按规范名或常见别名解析；未知名字返回 `None`。
    pub fn parse(name: &str) -> Option<Self> {
        let lowered = name.to_ascii_lowercase();
        if lowered == "ext2" || lowered == "ext3" || lowered == "ext4" {
            return Some(Self::Ext);
        }
        Self::ALL.into_iter().find(|kind| kind.as_str() == lowered)
    }

    /// 对外规范名。
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Squashfs => "squashfs",
            Self::Erofs => "erofs",
            Self::Ext => "ext",
            Self::Xfs => "xfs",
            Self::Btrfs => "btrfs",
            Self::F2fs => "f2fs",
            Self::Exfat => "exfat",
            Self::Vfat => "vfat",
            Self::Ntfs => "ntfs",
            Self::HfsPlus => "hfsplus",
            Self::Apfs => "apfs",
            Self::Bcachefs => "bcachefs",
            Self::Jfs => "jfs",
            Self::Reiserfs => "reiserfs",
            Self::Udf => "udf",
        }
    }

    /// 由规范名还原；只认规范拼写。
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.as_str() == name)
    }
}

/// `mkfs` 请求点名的目标类型。
///
/// 识别域只到族（[`FsKind`]），建卷需要保留 ext2/3/4 的区别与 swap、
/// LVM2 PV 这两种非文件系统目标，故单列一层。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MkfsKind {
    Ext2,
    Ext3,
    Ext4,
    Xfs,
    Btrfs,
    F2fs,
    Vfat,
    Exfat,
    Ntfs,
    Swap,
    Lvm2Pv,
}

impl MkfsKind {
    pub const ALL: [MkfsKind; 11] = [
        MkfsKind::Ext2,
        MkfsKind::Ext3,
        MkfsKind::Ext4,
        MkfsKind::Xfs,
        MkfsKind::Btrfs,
        MkfsKind::F2fs,
        MkfsKind::Vfat,
        MkfsKind::Exfat,
        MkfsKind::Ntfs,
        MkfsKind::Swap,
        MkfsKind::Lvm2Pv,
    ];

    pub fn parse(name: &str) -> Option<Self> {
        let lowered = name.to_ascii_lowercase();
        Self::ALL.into_iter().find(|kind| kind.as_str() == lowered)
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ext2 => "ext2",
            Self::Ext3 => "ext3",
            Self::Ext4 => "ext4",
            Self::Xfs => "xfs",
            Self::Btrfs => "btrfs",
            Self::F2fs => "f2fs",
            Self::Vfat => "vfat",
            Self::Exfat => "exfat",
            Self::Ntfs => "ntfs",
            Self::Swap => "swap",
            Self::Lvm2Pv => "lvm2_pv",
        }
    }

    /// 建卷请求对应的内容类别。
    pub const fn content(self) -> ContentKind {
        match self {
            Self::Ext2 | Self::Ext3 | Self::Ext4 => ContentKind::Filesystem(FsKind::Ext),
            Self::Xfs => ContentKind::Filesystem(FsKind::Xfs),
            Self::Btrfs => ContentKind::Filesystem(FsKind::Btrfs),
            Self::F2fs => ContentKind::Filesystem(FsKind::F2fs),
            Self::Vfat => ContentKind::Filesystem(FsKind::Vfat),
            Self::Exfat => ContentKind::Filesystem(FsKind::Exfat),
            Self::Ntfs => ContentKind::Filesystem(FsKind::Ntfs),
            Self::Swap => ContentKind::Swap,
            Self::Lvm2Pv => ContentKind::Lvm2Pv,
        }
    }
}

/// 磁盘分区承载的内容类别。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ContentKind {
    Filesystem(FsKind),
    /// swap 区。
    Swap,
    /// LVM2 物理卷标签。
    Lvm2Pv,
    /// LUKS 容器头。
    Luks,
    /// BitLocker 卷头。
    BitLocker,
    /// 可读内容无法归入任何已知格式。
    Unknown,
}

impl ContentKind {
    /// 全部内容类别，顺序与名称表一致。
    pub const ALL: [ContentKind; 20] = [
        ContentKind::Filesystem(FsKind::Squashfs),
        ContentKind::Filesystem(FsKind::Erofs),
        ContentKind::Filesystem(FsKind::Ext),
        ContentKind::Filesystem(FsKind::Xfs),
        ContentKind::Filesystem(FsKind::Btrfs),
        ContentKind::Filesystem(FsKind::F2fs),
        ContentKind::Filesystem(FsKind::Exfat),
        ContentKind::Filesystem(FsKind::Vfat),
        ContentKind::Filesystem(FsKind::Ntfs),
        ContentKind::Filesystem(FsKind::HfsPlus),
        ContentKind::Filesystem(FsKind::Apfs),
        ContentKind::Filesystem(FsKind::Bcachefs),
        ContentKind::Filesystem(FsKind::Jfs),
        ContentKind::Filesystem(FsKind::Reiserfs),
        ContentKind::Filesystem(FsKind::Udf),
        ContentKind::Swap,
        ContentKind::Lvm2Pv,
        ContentKind::Luks,
        ContentKind::BitLocker,
        ContentKind::Unknown,
    ];

    /// 对外规范名；非文件系统内容有自己的名字。
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Filesystem(kind) => kind.as_str(),
            Self::Swap => "swap",
            Self::Lvm2Pv => "lvm2_pv",
            Self::Luks => "luks",
            Self::BitLocker => "bitlocker",
            Self::Unknown => "unknown",
        }
    }

    /// 由规范名还原：先匹配非文件系统名称，再转文件系统族。
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "swap" => Some(Self::Swap),
            "lvm2_pv" => Some(Self::Lvm2Pv),
            "luks" => Some(Self::Luks),
            "bitlocker" => Some(Self::BitLocker),
            "unknown" => Some(Self::Unknown),
            _ => FsKind::from_name(name).map(Self::Filesystem),
        }
    }

    /// 文件系统族；非文件系统内容为 `None`。
    pub const fn filesystem(self) -> Option<FsKind> {
        match self {
            Self::Filesystem(kind) => Some(kind),
            Self::Swap | Self::Lvm2Pv | Self::Luks | Self::BitLocker | Self::Unknown => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_unique_and_round_trip() {
        for kind in FsKind::ALL {
            assert_eq!(FsKind::from_name(kind.as_str()), Some(kind));
        }
        for kind in ContentKind::ALL {
            assert_eq!(ContentKind::from_name(kind.as_str()), Some(kind));
        }
        let mut names: Vec<&str> = ContentKind::ALL.iter().map(|k| k.as_str()).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), ContentKind::ALL.len());
    }

    #[test]
    fn non_filesystem_contents_have_no_family() {
        assert_eq!(
            ContentKind::Filesystem(FsKind::Ext).filesystem(),
            Some(FsKind::Ext)
        );
        assert_eq!(ContentKind::Swap.filesystem(), None);
        assert_eq!(ContentKind::Lvm2Pv.filesystem(), None);
        assert_eq!(ContentKind::Luks.filesystem(), None);
        assert_eq!(ContentKind::BitLocker.filesystem(), None);
        assert_eq!(ContentKind::Unknown.filesystem(), None);
    }
}
