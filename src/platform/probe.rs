//! 盘上内容识别：按字节区间比对签名。
//!
//! 输入是已打开的句柄、区间起点与长度；输出只有内容类别。具体偏移与魔术字只
//! 在这里出现，上层拿到的是 `ContentKind`。读失败与"没有已知签名"严格区分：
//! 后者是 [`ContentKind::Unknown`]，前者是 `Err`。

use diskedit_core::fs::{ContentKind, FsKind};

use super::{PlatformError, PlatformResult, TargetHandle};

const SQUASHFS_MAGIC: &[u8; 4] = b"hsqs";
const EROFS_MAGIC: [u8; 4] = [0xE2, 0xE1, 0xF5, 0xE0];
const LUKS_MAGIC: &[u8; 6] = b"LUKS\xba\xbe";
const BITLOCKER_MAGIC: &[u8; 8] = b"-FVE-FS-";
const BCACHEFS_MAGIC_OFFSET: u64 = 4096 + 24;
const BCACHEFS_MAGIC: [u8; 16] = [
    0xc6, 0x85, 0x73, 0xf6, 0x66, 0xce, 0x90, 0xa9, 0xd9, 0x6a, 0x60, 0xcf, 0x80, 0x3d, 0xf7, 0xef,
];
const JFS_SB_OFFSET: u64 = 32768;
const JFS_MAGIC: &[u8; 4] = b"JFS1";
const UDF_VRS_OFFSET: u64 = 32768;
const UDF_VRS_MAGIC: &[u8; 5] = b"BEA01";
const REISERFS_MAGIC_OFFSET: u64 = 65536 + 52;
const REISERFS_MAGICS: [&[u8]; 3] = [b"ReIsErFs", b"ReIsEr2Fs", b"ReIsEr3Fs"];
const EXT_MAGIC: u16 = 0xEF53;
const SWAP_MAGIC: &[u8; 10] = b"SWAPSPACE2";
/// 本机 swapon 能处理的页大小。
const SWAPON_PAGES: [u64; 4] = [4096, 8192, 16384, 65536];
/// 能认出 swap 元数据的页大小（libblkid 口径，含 32K）。
const BLKID_PAGES: [u64; 5] = [4096, 8192, 16384, 32768, 65536];

/// 识别一段区间的内容类别。
pub fn identify(
    handle: &dyn TargetHandle,
    base: u64,
    len_bytes: u64,
) -> PlatformResult<ContentKind> {
    let reader = Reader {
        handle,
        base,
        len: len_bytes,
    };
    let hit = |offset: u64, len: usize| reader.read(offset, len);

    if hit(0, 4)?.as_deref() == Some(SQUASHFS_MAGIC) {
        return Ok(ContentKind::Filesystem(FsKind::Squashfs));
    }
    if hit(1024, 4)?.as_deref() == Some(&EROFS_MAGIC) {
        return Ok(ContentKind::Filesystem(FsKind::Erofs));
    }
    if let Some(bytes) = hit(0x438, 2)?
        && u16::from_le_bytes([bytes[0], bytes[1]]) == EXT_MAGIC
    {
        return Ok(ContentKind::Filesystem(FsKind::Ext));
    }
    if hit(0, 4)?.as_deref() == Some(b"XFSB") {
        return Ok(ContentKind::Filesystem(FsKind::Xfs));
    }
    if let Some(bytes) = hit(0x10040, 8)?
        && bytes == 0x4D5F_5366_5248_425Fu64.to_le_bytes()
    {
        return Ok(ContentKind::Filesystem(FsKind::Btrfs));
    }
    for offset in [0x400u64, 0x1400] {
        if let Some(bytes) = hit(offset, 4)?
            && u32::from_le_bytes(bytes.as_slice().try_into().expect("四字节")) == 0xF2F5_2010
        {
            return Ok(ContentKind::Filesystem(FsKind::F2fs));
        }
    }
    if hit(3, 8)?.as_deref() == Some(b"EXFAT   ") {
        return Ok(ContentKind::Filesystem(FsKind::Exfat));
    }
    if hit(0x52, 8)?.as_deref() == Some(b"FAT32   ") {
        return Ok(ContentKind::Filesystem(FsKind::Vfat));
    }
    if let Some(bytes) = hit(0x36, 8)?
        && (bytes.as_slice() == b"FAT12   " || bytes.as_slice() == b"FAT16   ")
    {
        return Ok(ContentKind::Filesystem(FsKind::Vfat));
    }
    if hit(3, 8)?.as_deref() == Some(b"NTFS    ") {
        return Ok(ContentKind::Filesystem(FsKind::Ntfs));
    }
    for signature in [b"H+", b"HX"] {
        if hit(0x400, 2)?.as_deref() == Some(signature) {
            return Ok(ContentKind::Filesystem(FsKind::HfsPlus));
        }
    }
    if hit(0x20, 4)?.as_deref() == Some(b"NXSB") {
        return Ok(ContentKind::Filesystem(FsKind::Apfs));
    }
    for offset in [0u64, 512, 1024, 1536] {
        if let Some(bytes) = hit(offset, 32)?
            && &bytes[0..8] == b"LABELONE"
            && &bytes[24..32] == b"LVM2 001"
        {
            return Ok(ContentKind::Lvm2Pv);
        }
    }
    if hit(0, 6)?.as_deref() == Some(LUKS_MAGIC) {
        return Ok(ContentKind::Luks);
    }
    if hit(3, 8)?.as_deref() == Some(BITLOCKER_MAGIC) {
        return Ok(ContentKind::BitLocker);
    }
    if hit(BCACHEFS_MAGIC_OFFSET, 16)?.as_deref() == Some(&BCACHEFS_MAGIC) {
        return Ok(ContentKind::Filesystem(FsKind::Bcachefs));
    }
    if hit(JFS_SB_OFFSET, 4)?.as_deref() == Some(JFS_MAGIC) {
        return Ok(ContentKind::Filesystem(FsKind::Jfs));
    }
    if hit(UDF_VRS_OFFSET, 5)?.as_deref() == Some(UDF_VRS_MAGIC) {
        return Ok(ContentKind::Filesystem(FsKind::Udf));
    }
    if let Some(bytes) = hit(REISERFS_MAGIC_OFFSET, 10)?
        && REISERFS_MAGICS.iter().any(|magic| bytes.starts_with(magic))
    {
        return Ok(ContentKind::Filesystem(FsKind::Reiserfs));
    }
    if swap_hit(&reader)? {
        return Ok(ContentKind::Swap);
    }
    Ok(ContentKind::Unknown)
}

/// 一段区间上的按偏移读取器；越界不是读故障。
struct Reader<'a> {
    handle: &'a dyn TargetHandle,
    base: u64,
    len: u64,
}

impl Reader<'_> {
    fn read(&self, offset: u64, len: usize) -> PlatformResult<Option<Vec<u8>>> {
        let Some(end) = offset.checked_add(len as u64) else {
            return Ok(None);
        };
        if end > self.len {
            return Ok(None);
        }
        let Some(position) = self.base.checked_add(offset) else {
            return Ok(None);
        };
        let mut buffer = vec![0u8; len];
        read_exact_at(self.handle, position, &mut buffer)?;
        Ok(Some(buffer))
    }
}

fn read_exact_at(handle: &dyn TargetHandle, offset: u64, buffer: &mut [u8]) -> PlatformResult<()> {
    let mut filled = 0;
    while filled < buffer.len() {
        let at = offset
            .checked_add(filled as u64)
            .ok_or_else(|| PlatformError::parse("probe offset overflow"))?;
        let read = handle.read_at(at, &mut buffer[filled..])?;
        if read == 0 {
            return Err(PlatformError::io(
                handle.path(),
                std::io::Error::from(std::io::ErrorKind::UnexpectedEof),
            ));
        }
        filled += read;
    }
    Ok(())
}

/// swap 签名命中且头内自洽。
fn swap_hit(reader: &Reader<'_>) -> PlatformResult<bool> {
    let mut pages: Vec<u64> = Vec::with_capacity(SWAPON_PAGES.len() + 1);
    let host = host_page();
    if SWAPON_PAGES.contains(&host) {
        pages.push(host);
    }
    pages.extend(SWAPON_PAGES.iter().copied().filter(|page| *page != host));
    pages.extend(
        BLKID_PAGES
            .iter()
            .copied()
            .filter(|page| !SWAPON_PAGES.contains(page)),
    );
    for page in pages {
        if reader.len < page {
            continue;
        }
        let Some(magic) = reader.read(page - 10, 10)? else {
            continue;
        };
        if magic.as_slice() != SWAP_MAGIC {
            continue;
        }
        if !SWAPON_PAGES.contains(&page) {
            // 只有 libblkid 认得的页大小不能被本机 swapon 激活，不计入 swap。
            continue;
        }
        let Some(info) = reader.read(1024, 8)? else {
            continue;
        };
        let version = u32::from_le_bytes(info[0..4].try_into().expect("四字节"));
        let last_page = u32::from_le_bytes(info[4..8].try_into().expect("四字节"));
        return Ok(version == 1 && u64::from(last_page) < reader.len / page);
    }
    Ok(false)
}

fn host_page() -> u64 {
    #[cfg(unix)]
    {
        // SAFETY: sysconf 只读进程/系统常量，无失败写回。
        let value = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        if value > 0 { value as u64 } else { 4096 }
    }
    #[cfg(not(unix))]
    {
        4096
    }
}
