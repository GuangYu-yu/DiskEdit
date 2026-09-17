//! 文件系统魔数识别（偏移与特征值的来源见各行注释）。

use crate::dev::FileSource;
use std::io;

const SQUASHFS_MAGIC: &[u8; 4] = b"hsqs";
const EROFS_MAGIC: [u8; 4] = [0xE2, 0xE1, 0xF5, 0xE0];

pub fn identify(src: &FileSource, part_first_lba: u64, part_size_lba: u64) -> io::Result<&'static str> {
    let ss = src.sector_size;
    let base = part_first_lba * ss;
    let part_len = part_size_lba * ss;

    let rd = |off: u64, len: usize| -> io::Result<Option<Vec<u8>>> {
        if off + len as u64 > part_len {
            return Ok(None);
        }
        let mut buf = vec![0u8; len];
        if src.read_at(base + off, &mut buf).is_err() {
            return Ok(None);
        }
        Ok(Some(buf))
    };

    // squashfs 4.0: "hsqs" @0（内核 squashfs_fs.h SQUASHFS_MAGIC，LE 落盘）
    if let Some(b) = rd(0, 4)?
        && b == SQUASHFS_MAGIC
    {
        return Ok("squashfs");
    }
    // EROFS: 0xE0F5E1E2 (LE) @1024（内核 erofs_fs.h EROFS_SUPER_MAGIC_V1，超级块 @1024）
    if let Some(b) = rd(1024, 4)?
        && b == EROFS_MAGIC
    {
        return Ok("erofs");
    }
    // ext2/3/4: 0xEF53 (LE) @0x438（sb @1024，s_magic @sb+0x38）。magic 区分不出 2/3/4，
    // 而 e2fsck/resize2fs/tune2fs 对三者通用，故统一返回 "ext"（各消费点接受 ext2/3/4 写法）
    if let Some(b) = rd(0x438, 2)?
        && u16::from_le_bytes([b[0], b[1]]) == 0xEF53
    {
        return Ok("ext");
    }
    // XFS: "XFSB" @0（BE 序 magic 0xC03B3998；libxfs xfs_format.h XFS_SB_MAGIC）
    if let Some(b) = rd(0, 4)?
        && &b == b"XFSB"
    {
        return Ok("xfs");
    }
    // btrfs: "_BHRfS_M" @0x10040（内核 fs/btrfs ctree.h BTRFS_MAGIC；超级块 @64 KiB，magic 在 +64）
    if let Some(b) = rd(0x10040, 8)?
        && &b == b"_BHRfS_M"
    {
        return Ok("btrfs");
    }
    // F2FS: 0xF2F52010 (LE) @0x400，第二副本 @0x1400（内核 __get_raw_super 依次读两份，
    // 主 SB 损坏时回退第二份，故两处都查）
    for off in [0x400u64, 0x1400] {
        if let Some(b) = rd(off, 4)?
            && u32::from_le_bytes(b[..4].try_into().unwrap()) == 0xF2F5_2010
        {
            return Ok("f2fs");
        }
    }
    // exFAT: "EXFAT   "（8 字节含 3 尾随空格）@3 —— 微软规范 §3.1 FileSystemName
    if let Some(b) = rd(3, 8)?
        && &b == b"EXFAT   "
    {
        return Ok("exfat");
    }
    // FAT32: "FAT32   " @0x52；FAT12/16 在 @0x36 —— 微软 EFI FAT32 规范 §5 的 BS_FilSysType
    if let Some(b) = rd(0x52, 8)?
        && &b == b"FAT32   "
    {
        return Ok("vfat");
    }
    if let Some(b) = rd(0x36, 8)?
        && (&b == b"FAT12   " || &b == b"FAT16   ")
    {
        return Ok("vfat");
    }
    // NTFS: "NTFS    " @3（$Boot OEM ID；ntfs-3g libntfs-3g/bootsect.c
    // ntfs_boot_sector_is_ntfs：oem_id == "NTFS    "）
    if let Some(b) = rd(3, 8)?
        && &b == b"NTFS    "
    {
        return Ok("ntfs");
    }
    // HFS+/HFSX: 卷头 @1024，签名 "H+"/"HX"（Apple TN1150 HFS Plus Volume Format：
    // "The volume signature is the value 'H+' … 'HX' for HFSX"）
    for sig in [b"H+", b"HX"] {
        if let Some(b) = rd(0x400, 2)?
            && b == sig
        {
            return Ok("hfsplus");
        }
    }
    // APFS: 容器超块 nx_superblock_t 在块 0，前 32 字节为 obj_phys_t 对象头
    // （o_cksum 8 + o_oid 8 + o_xid 8 + o_type 4），magic "NXSB" @0x20
    // （Apple File System Reference nx_superblock_t； carving 文献同：bytes 32..36 == 'NXSB'）
    if let Some(b) = rd(0x20, 4)?
        && &b == b"NXSB"
    {
        return Ok("apfs");
    }
    // LVM2 PV：标签头 "LABELONE" 位于前 4 个 512B 扇区之一（pvcreate 默认第 2 扇区），
    // 标签头内偏移 24 处为类型串 "LVM2 001"（LVM2 lib/format_text/layout.h label_header）
    for off in [0u64, 512, 1024, 1536] {
        if let Some(b) = rd(off, 32)?
            && &b[0..8] == b"LABELONE"
            && &b[24..32] == b"LVM2 001"
        {
            return Ok("lvm2_pv");
        }
    }
    // swap: "SWAPSPACE2" 位于第一页末尾 10 字节（内核 include/linux/swap.h
    // union swap_header：reserved[PAGE_SIZE-10] + magic[10]）。盘上不记录创建时的页大小，
    // 故按候选 [本机页,4K,8K,16K,64K] 逐一探测——本工具的探测策略，非规范要求。
    // 32K 不在候选内：util-linux swapon 的 swap_get_header 明确跳过 0x8000（注释称该页大小
    // 似不受支持），本工具跟随该口径；libblkid 仍探测 0x7ff6，两者不同
    // 运行系统页大小（man sysconf(3) 的 _SC_PAGESIZE）
    #[cfg(target_os = "linux")]
    let host_page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as u64;
    #[cfg(not(target_os = "linux"))]
    let host_page = 4096u64;
    // host_page 与固定候选重合时（如 4K 页宿主）去重，避免同偏移重复探测
    let mut pages: Vec<u64> = Vec::new();
    for ps in [host_page, 4096, 8192, 16384, 65536] {
        if !pages.contains(&ps) {
            pages.push(ps);
        }
    }
    for ps in pages {
        if part_len >= ps
            && let Some(b) = rd(ps - 10, 10)?
            && &b == b"SWAPSPACE2"
        {
            return Ok("swap");
        }
    }
    Ok("unknown")
}

/// OpenWrt combined 布局的 RW overlay 起点（字节，相对分区头）。公式须与
/// fstools libfstools/rootdisk.c 一致（mount_root 建 loop 用的 lo_offset 即此值）：
/// - squashfs 4.0 → bytes_used __le64 @0x28（内核 squashfs_fs.h 结构序：
///   5×u32 + 4×u16 + u64 root_inode = 0x28）
/// - EROFS → blocks u32 @超级块+0x24 左移 blkszbits u8 @超级块+0x0C（超级块 @1024）
///
/// 统一 64K 上对齐（fstools ROOTDEV_OVERLAY_ALIGN 的惯例，非通用规范）；
/// 解析失败或 0 → None
pub fn overlay_offset_at(src: &FileSource, part_offset: u64) -> io::Result<Option<u64>> {
    const ALIGN: u64 = 64 * 1024;
    let mut sb = [0u8; 2048];
    if src.read_at(part_offset, &mut sb).is_err() {
        return Ok(None);
    }
    let raw = if &sb[0..4] == SQUASHFS_MAGIC {
        u64::from_le_bytes(sb[0x28..0x30].try_into().unwrap())
    } else if sb[1024..1028] == EROFS_MAGIC {
        let blkszbits = sb[1024 + 0x0C] as u32;
        // blkszbits 是盘上可控值：内核按自身 LOG_BLOCK_SIZE 匹配，故只查下界；
        // checked_shl 挡位数溢出
        if blkszbits < 9 {
            return Ok(None);
        }
        let blocks = u32::from_le_bytes(sb[1024 + 0x24..1024 + 0x28].try_into().unwrap()) as u64;
        match blocks.checked_shl(blkszbits) {
            Some(v) => v,
            None => return Ok(None),
        }
    } else {
        return Ok(None);
    };
    if raw == 0 {
        return Ok(None);
    }
    Ok(raw.checked_add(ALIGN - 1).map(|v| v & !(ALIGN - 1)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn src_from(tag: &str, data: Vec<u8>) -> FileSource {
        let mut tmp = std::env::temp_dir();
        tmp.push(format!("diskedit_fsid_{tag}_{}.img", std::process::id()));
        std::fs::write(&tmp, &data).unwrap();
        let f = std::fs::OpenOptions::new().read(true).write(true).open(&tmp).unwrap();
        FileSource { file: f, path: tmp, sector_size: 512, size: data.len() as u64, is_block: false, journal: None }
    }

    #[test]
    fn ext_magic() {
        let mut data = vec![0u8; 4096];
        data[0x438..0x43A].copy_from_slice(&0xEF53u16.to_le_bytes());
        let s = src_from("ext", data);
        assert_eq!(identify(&s, 0, 8).unwrap(), "ext");
    }

    #[test]
    fn swap_magic_at_page_tail() {
        let mut data = vec![0u8; 8192];
        let ps = 4096u64;
        data[(ps - 10) as usize..ps as usize].copy_from_slice(b"SWAPSPACE2");
        let s = src_from("swap", data);
        assert_eq!(identify(&s, 0, 16).unwrap(), "swap");
    }

    #[test]
    fn swap_detected_across_page_sizes() {
        // 创建机页 8K、宿主页 4K：候选探测命中 8192 处 magic
        let mut data = vec![0u8; 16384];
        let ps = 8192u64;
        data[(ps - 10) as usize..ps as usize].copy_from_slice(b"SWAPSPACE2");
        let s = src_from("swap8k", data);
        assert_eq!(identify(&s, 0, 16).unwrap(), "swap");
    }

    #[test]
    fn swap_signature_at_32k_offset_not_detected() {
        // 候选集不含 32K（swapon 的 swap_get_header 跳过 0x8000），64K 处也没有 magic
        let mut data = vec![0u8; 65536];
        let ps = 32768u64;
        data[(ps - 10) as usize..ps as usize].copy_from_slice(b"SWAPSPACE2");
        let s = src_from("swap32k", data);
        assert_eq!(identify(&s, 0, 128).unwrap(), "unknown");
    }

    #[test]
    fn hfsplus_magic_at_volume_header() {
        let mut data = vec![0u8; 4096];
        data[0x400..0x402].copy_from_slice(b"H+");
        let s = src_from("hfsplus", data);
        assert_eq!(identify(&s, 0, 8).unwrap(), "hfsplus");
    }

    #[test]
    fn apfs_magic_at_object_header_end() {
        let mut data = vec![0u8; 4096];
        data[0x20..0x24].copy_from_slice(b"NXSB");
        let s = src_from("apfs", data);
        assert_eq!(identify(&s, 0, 8).unwrap(), "apfs");
    }

    #[test]
    fn lvm2_pv_label_in_second_sector() {
        let mut data = vec![0u8; 4096];
        data[512..520].copy_from_slice(b"LABELONE");
        data[536..544].copy_from_slice(b"LVM2 001");
        let s = src_from("lvm", data);
        assert_eq!(identify(&s, 0, 8).unwrap(), "lvm2_pv");
    }

    #[test]
    fn unknown_not_guessed() {
        let data = vec![0u8; 4096];
        let s = src_from("unk", data);
        assert_eq!(identify(&s, 0, 8).unwrap(), "unknown");
    }

    #[test]
    fn squashfs_and_erofs_magic() {
        let mut data = vec![0u8; 8192];
        data[0..4].copy_from_slice(b"hsqs");
        let s = src_from("sq", data);
        assert_eq!(identify(&s, 0, 16).unwrap(), "squashfs");

        let mut data = vec![0u8; 8192];
        data[1024..1028].copy_from_slice(&[0xE2, 0xE1, 0xF5, 0xE0]);
        let s = src_from("ero", data);
        assert_eq!(identify(&s, 0, 16).unwrap(), "erofs");
    }

    /// overlay 起点公式：squashfs bytes_used 上取 64K 对齐；EROFS blocks<<blkszbits 同；
    /// 无根 fs 魔数 → None；blkszbits 溢出 → None
    #[test]
    fn overlay_offset_fstools_alignment() {
        // squashfs：bytes_used = 0x2_9C00 → 上对齐 64K = 0x2_0000... 验证非对齐值上取
        let mut data = vec![0u8; 65536];
        data[0..4].copy_from_slice(b"hsqs");
        data[0x28..0x30].copy_from_slice(&0x29C00u64.to_le_bytes());
        let s = src_from("ovl", data.clone());
        assert_eq!(overlay_offset_at(&s, 0).unwrap(), Some(0x30000));
        // 已对齐值不变
        data[0x28..0x30].copy_from_slice(&0x30000u64.to_le_bytes());
        let s = src_from("ovl2", data);
        assert_eq!(overlay_offset_at(&s, 0).unwrap(), Some(0x30000));
        // EROFS：blocks=0x30 << blkszbits=12 = 0x30000
        let mut data = vec![0u8; 65536];
        data[1024..1028].copy_from_slice(&[0xE2, 0xE1, 0xF5, 0xE0]);
        data[1024 + 0x0C] = 12;
        data[1024 + 0x24..1024 + 0x28].copy_from_slice(&0x30u32.to_le_bytes());
        let s = src_from("ovl3", data.clone());
        assert_eq!(overlay_offset_at(&s, 0).unwrap(), Some(0x30000));
        // blkszbits 溢出 → None
        data[1024 + 0x0C] = 99;
        let s = src_from("ovl4", data);
        assert_eq!(overlay_offset_at(&s, 0).unwrap(), None);
        // blkszbits < 9（无下界语义）→ None
        let mut data = vec![0u8; 65536];
        data[1024..1028].copy_from_slice(&[0xE2, 0xE1, 0xF5, 0xE0]);
        data[1024 + 0x0C] = 8;
        let s = src_from("ovl6", data);
        assert_eq!(overlay_offset_at(&s, 0).unwrap(), None);
        // 无魔数 → None
        let data = vec![0u8; 65536];
        let s = src_from("ovl5", data);
        assert_eq!(overlay_offset_at(&s, 0).unwrap(), None);
    }
}