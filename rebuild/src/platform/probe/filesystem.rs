//! 文件系统签名识别：只读入字节，不读盘。
//!
//! 调用方负责把足够长的前缀读进来；这里按架构固定的偏移逐族比对。字节不足时
//! 回答"读不到"，凑够字节但无一命中回答"格式未知"，两者都不会被当作可修复对象。

use diskedit_core::evidence::Evidence;
use diskedit_core::fs::{ContentKind, FsKind};

/// 识别所需的最短前缀：至少要覆盖 VFAT 的 FAT32 标签。
pub const MIN_BYTES: usize = 512;

/// 识别内容类别。
pub fn identify(bytes: &[u8]) -> Evidence<ContentKind> {
    if bytes.len() < MIN_BYTES {
        return Evidence::Unreadable {
            why: format!("签名前缀只有 {} 字节，至少需要 {MIN_BYTES}", bytes.len()),
        };
    }
    if let Some(kind) = match_kind(bytes) {
        Evidence::Present(kind)
    } else {
        Evidence::UnknownFormat {
            why: "前缀中没有任何已登记的签名".to_string(),
        }
    }
}

fn match_kind(bytes: &[u8]) -> Option<ContentKind> {
    let fs = |kind| Some(ContentKind::Filesystem(kind));

    if at(bytes, 0, b"XFSB") {
        return fs(FsKind::Xfs);
    }
    if at(bytes, 0, b"hsqs") || at(bytes, 0, b"sqsh") {
        return fs(FsKind::Squashfs);
    }
    if at(bytes, 1024, &0xE0F5_E1E2u32.to_le_bytes()) {
        return fs(FsKind::Erofs);
    }
    if at(bytes, 0x438, &0x53EFu16.to_le_bytes()) {
        return fs(FsKind::Ext);
    }
    if at(bytes, 0x10040, b"_BHRfS_M") {
        return fs(FsKind::Btrfs);
    }
    if at(bytes, 1024, &0xF2F5_2010u32.to_le_bytes()) {
        return fs(FsKind::F2fs);
    }
    if at(bytes, 3, b"EXFAT   ") {
        return fs(FsKind::Exfat);
    }
    if at(bytes, 3, b"NTFS    ") {
        return fs(FsKind::Ntfs);
    }
    if at(bytes, 0x36, b"FAT12   ") || at(bytes, 0x36, b"FAT16   ") || at(bytes, 0x52, b"FAT32   ")
    {
        return fs(FsKind::Vfat);
    }
    if at(bytes, 1024, b"H+") || at(bytes, 1024, b"HX") {
        return fs(FsKind::HfsPlus);
    }
    if at(bytes, 32, b"NXSB") {
        return fs(FsKind::Apfs);
    }
    if at(bytes, 0x1018, &0xC685_73F6u32.to_le_bytes()) {
        return fs(FsKind::Bcachefs);
    }
    if at(bytes, 0x8000, b"JFS1") {
        return fs(FsKind::Jfs);
    }
    if at(bytes, 0x10000, b"ReIsEr") {
        return fs(FsKind::Reiserfs);
    }
    if at(bytes, 0x8000, b"BEA01") || at(bytes, 0x8800, b"NSR02") {
        return fs(FsKind::Udf);
    }
    if at(bytes, 0, b"LUKS\xBA\xBE") {
        return Some(ContentKind::Luks);
    }
    if at(bytes, 3, b"-FVE-FS-") {
        return Some(ContentKind::BitLocker);
    }
    if at(bytes, 0x200, b"LABELONE") && contains(bytes, b"LVM2 001") {
        return Some(ContentKind::Lvm2Pv);
    }
    if swap_signature(bytes) {
        return Some(ContentKind::Swap);
    }
    None
}

fn at(bytes: &[u8], offset: usize, needle: &[u8]) -> bool {
    bytes
        .get(offset..offset + needle.len())
        .is_some_and(|window| window == needle)
}

fn contains(bytes: &[u8], needle: &[u8]) -> bool {
    bytes.windows(needle.len()).any(|window| window == needle)
}

fn swap_signature(bytes: &[u8]) -> bool {
    at(bytes, 0, b"SWAP-SPACE")
        || at(bytes, 4086, b"SWAPSPACE2")
        || at(bytes, bytes.len().saturating_sub(10), b"SWAPSPACE2")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with(offset: usize, needle: &[u8]) -> Vec<u8> {
        let mut bytes = vec![0u8; MIN_BYTES.max(offset + needle.len())];
        bytes[offset..offset + needle.len()].copy_from_slice(needle);
        bytes
    }

    #[test]
    fn recognizes_each_family_at_its_offset() {
        let cases: &[(Vec<u8>, FsKind)] = &[
            (with(0, b"XFSB"), FsKind::Xfs),
            (with(0, b"hsqs"), FsKind::Squashfs),
            (with(0x438, &0x53EFu16.to_le_bytes()), FsKind::Ext),
            (with(1024, &0xF2F5_2010u32.to_le_bytes()), FsKind::F2fs),
            (with(3, b"NTFS    "), FsKind::Ntfs),
            (with(3, b"EXFAT   "), FsKind::Exfat),
            (with(0x52, b"FAT32   "), FsKind::Vfat),
            (with(1024, b"H+"), FsKind::HfsPlus),
            (with(32, b"NXSB"), FsKind::Apfs),
            (with(0x8000, b"JFS1"), FsKind::Jfs),
        ];
        for (bytes, expected) in cases {
            match identify(bytes) {
                Evidence::Present(ContentKind::Filesystem(kind)) => assert_eq!(kind, *expected),
                other @ (Evidence::Present(_)
                | Evidence::Absent
                | Evidence::UnknownFormat { .. }
                | Evidence::Unreadable { .. }) => {
                    panic!("期望 {expected:?}，得到 {other:?}")
                }
            }
        }
    }

    #[test]
    fn recognizes_containers() {
        assert_eq!(
            identify(&with(0, b"LUKS\xBA\xBE")),
            Evidence::Present(ContentKind::Luks)
        );
        assert_eq!(
            identify(&with(3, b"-FVE-FS-")),
            Evidence::Present(ContentKind::BitLocker)
        );
        let mut lvm = with(0x200, b"LABELONE");
        lvm.extend_from_slice(b"LVM2 001");
        assert_eq!(identify(&lvm), Evidence::Present(ContentKind::Lvm2Pv));
        let mut swap = vec![0u8; 4096];
        swap[4096 - 10..].copy_from_slice(b"SWAPSPACE2");
        assert_eq!(identify(&swap), Evidence::Present(ContentKind::Swap));
    }

    #[test]
    fn short_or_unmatched_input_stays_unresolved() {
        assert!(matches!(identify(&[0u8; 100]), Evidence::Unreadable { .. }));
        assert!(matches!(
            identify(&[0u8; MIN_BYTES]),
            Evidence::UnknownFormat { .. }
        ));
    }
}
