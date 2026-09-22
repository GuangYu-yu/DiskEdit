//! 文件系统魔数识别（偏移与特征值的来源见各行注释）。

use crate::dev::FileSource;
use std::io;

const SQUASHFS_MAGIC: &[u8; 4] = b"hsqs";
const EROFS_MAGIC: [u8; 4] = [0xE2, 0xE1, 0xF5, 0xE0];

/// 按**字节区间**识别文件系统：`base` 起、`len_bytes` 长的区域。
///
/// 刻意收字节而不收 LBA + 扇区大小：LBA 的单位取决于它来自哪张表——GPT 条目的 LBA 以
/// **表自身的** ss 计（4Kn 镜像未加 --sector-size 时 `g.ss != src.sector_size`），
/// MBR 条目的 LBA 以容器 ss 计。签名里带 ss 就等于要求每个调用点都为**别人的**单位负责，
/// 而它手上往往只有 LBA；改收字节后，换算发生在唯一知道单位的那一层（读到表的地方），
/// 本模块退化为"给一段字节，判它是什么"，与 probe_swap_header / overlay_offset_at 同形
///
/// Err 只表达设备故障：调用方必须把它与"区间内没有已知签名"（Ok("unknown")）区分开，
/// 否则 resize 会在 I/O 错误时把 FS 步骤整体跳过并报成功
pub fn identify(src: &FileSource, base: u64, len_bytes: u64) -> io::Result<&'static str> {
    let rd = |off: u64, len: usize| -> io::Result<Option<Vec<u8>>> {
        // 区间外：这里确实没有那些字节，不是读失败。off/len 虽全是常量调用点，
        // checked 失败同样按不命中处理——回绕地址不得混进 read_at
        let Some(end) = off.checked_add(len as u64) else { return Ok(None) };
        if end > len_bytes {
            return Ok(None);
        }
        let mut buf = vec![0u8; len];
        let Some(pos) = base.checked_add(off) else { return Ok(None) };
        src.read_at(pos, &mut buf)?;
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
    // APFS：容器超块 nx_superblock_t 在块 0；对象头 obj_phys_t 共 32 字节
    // （o_cksum8 + o_oid8 + o_xid8 + o_type4 + o_subtype4），magic "NXSB" @0x20
    // （Apple File System Reference nx_superblock_t；carving 文献同：bytes 32..36 == 'NXSB'）
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
    // swap: 签名位于"创建机页大小"末尾 10 字节（内核 include/linux/swap.h
    // union swap_header：reserved[PAGE_SIZE-10] + magic[10]），盘上不记录该页大小，
    // 故由 probe_swap_header 按候选集探测。此处取 swapon 口径的候选集——
    // 本函数回答的是"这台宿主能不能把它当 swap 处理"
    if probe_swap_header(src, base, len_bytes, &swapon_activatable_pages())?.is_some() {
        return Ok("swap");
    }
    Ok("unknown")
}

/// ext 家族判定（本次识别的名字 + 用户可显式书写的别名）。
/// identify 对 0xEF53 只回 "ext"，ext2/3/4 来自 mkfs 的目标 FS 参数，
/// 而所有 ext 消费点对这四个名字行为一致，故"家族"只在这里定义一次
pub fn is_ext(fstype: &str) -> bool {
    matches!(fstype, "ext" | "ext2" | "ext3" | "ext4")
}

/// swap v1 签名（内核 include/linux/swap.h union swap_header）
const SWAP_MAGIC: &[u8; 10] = b"SWAPSPACE2";

/// util-linux swapon 的页大小范围：sys-utils/swapon.c 循环 `0x1000..=64K` 步进翻倍，
/// 并显式 `if (page == 0x8000) continue;`（注释称 32K 页似不受支持）
const SWAPON_PAGES: [u64; 4] = [4096, 8192, 16384, 65536];

/// util-linux libblkid 的 swap 签名偏移表：libblkid/src/superblocks/swap.c 的
/// sboff = 0xff6 / 0x1ff6 / 0x3ff6 / 0x7ff6 / 0xfff6（含 32K）
const BLKID_PAGES: [u64; 5] = [4096, 8192, 16384, 32768, 65536];

/// 本机页大小（man sysconf(3) 的 _SC_PAGESIZE）
fn host_page() -> u64 {
    #[cfg(target_os = "linux")]
    {
        // SAFETY: sysconf 只读进程/系统常量，_SC_PAGESIZE 无失败写回，返回值为长整型页大小
        unsafe { libc::sysconf(libc::_SC_PAGESIZE) as u64 }
    }
    #[cfg(not(target_os = "linux"))]
    {
        4096
    }
}

/// 候选顺序：本机页优先（本机格式化的 swap 区就是本机页大小），其余按集合升序。
/// host 不在集合内时不追加——集合定义"哪些页大小属于该口径支持的范围内"
fn ordered_pages(set: &[u64]) -> Vec<u64> {
    let host = host_page();
    let mut v: Vec<u64> = Vec::with_capacity(set.len());
    if set.contains(&host) {
        v.push(host);
    }
    v.extend(set.iter().copied().filter(|p| *p != host));
    v
}

/// swapon 口径的候选页大小（"能否激活"，不含 32K）
pub fn swapon_activatable_pages() -> Vec<u64> {
    ordered_pages(&SWAPON_PAGES)
}

/// libblkid 口径的候选页大小（"能否识别出元数据"；含 32K，宽于激活能力）
pub fn blkid_known_pages() -> Vec<u64> {
    ordered_pages(&BLKID_PAGES)
}

/// swap 签名探测（唯一实现）：在候选页大小各自的末尾 10 字节找 SWAPSPACE2，
/// 返回命中的页大小（magic 偏移 = page − 10）。候选集由调用方按语义选择（见上两个
/// 具名集合），本函数不做取舍。只认 SWAPSPACE2：v0 的 "SWAP-SPACE" 内核早已不再写入，
/// swsuspend 系签名（S1SUSPEND/S2SUSPEND/ULSUSPEND/TOI/LINHIB0001）是休眠镜像
/// 而非 swap 区，二者都不认。
/// 读取失败回 Err 而非 None：签名位置读不出来 ≠ 证明签名不在——把设备故障压成
/// "未命中"会让 identify 把故障盘报成 unknown，resize 随之跳过 FS 步骤并报成功
pub fn probe_swap_header(src: &FileSource, base: u64, len_bytes: u64, page_sizes: &[u64]) -> io::Result<Option<u64>> {
    for &page in page_sizes {
        if len_bytes < page {
            continue;
        }
        let mut magic = [0u8; 10];
        src.read_at(base + page - 10, &mut magic)?;
        if &magic == SWAP_MAGIC {
            return Ok(Some(page));
        }
    }
    Ok(None)
}

/// 元数据是 swap、但**本机（swapon 口径）激活不了**的区间：libblkid 口径命中而 swapon 口径落空。
/// 两个候选集的差集只有 32K 一项（见 SWAPON_PAGES / BLKID_PAGES 的依据），故命中即
/// "创建机用了 32K 页"。identify 对此类区间回 "unknown"，写入路径会据此跳过 FS 步骤，
/// 而分区扩容后 swap 头里的页数还是旧值——那是一条未完成的后置条件，不能报成功
pub fn unactivatable_swap(src: &FileSource, base: u64, len_bytes: u64) -> io::Result<bool> {
    Ok(matches!(
        (
            probe_swap_header(src, base, len_bytes, &blkid_known_pages())?,
            probe_swap_header(src, base, len_bytes, &swapon_activatable_pages())?,
        ),
        (Some(_), None)
    ))
}

/// OpenWrt combined 布局的 RW overlay 起点（字节，相对分区头）。公式须与
/// fstools libfstools/rootdisk.c 一致（mount_root 建 loop 用的 lo_offset 即此值）：
/// - squashfs 4.0 → bytes_used __le64 @0x28（内核 squashfs_fs.h：超级块为 5×u32 + 6×u16，
///   其后 u64 root_inode@0x20、u64 bytes_used@0x28）
/// - EROFS → blocks u32 @超级块+0x24 左移 blkszbits u8 @超级块+0x0C（超级块 @1024）
///
/// 统一 64K 上对齐（fstools ROOTDEV_OVERLAY_ALIGN 的惯例，非通用规范）；
/// 解析失败或 0 → None。读错误如实上抛：读不出来与"签名不命中"是两回事，
/// 把前者折叠成 None 会让调用方把盘上事实当成"没有 overlay"。分区不足以容纳
/// 超级块读区同样按未命中——那不是读故障，是区间不存在的事实
pub fn overlay_offset_at(src: &FileSource, part_offset: u64) -> io::Result<Option<u64>> {
    const ALIGN: u64 = 64 * 1024;
    let mut sb = [0u8; 2048];
    // 不设此闸，read_exact 对越界区间报 UnexpectedEof，会被调用方当成 I/O 故障（退 30）
    if src.size < part_offset.saturating_add(sb.len() as u64) {
        return Ok(None);
    }
    src.read_at(part_offset, &mut sb)?;
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
        FileSource {
            identity: crate::dev::TargetIdentity::resolve_image(&tmp),
            file: f,
            path: tmp,
            sector_size: 512,
            size: data.len() as u64,
            is_block: false,
            journal: None,
            ownership: None,
        }
    }

    #[test]
    fn ext_magic() {
        let mut data = vec![0u8; 4096];
        data[0x438..0x43A].copy_from_slice(&0xEF53u16.to_le_bytes());
        let s = src_from("ext", data);
        assert_eq!(identify(&s, 0, 4096).unwrap(), "ext");
    }

    #[test]
    fn swap_magic_at_page_tail() {
        let mut data = vec![0u8; 8192];
        let ps = 4096u64;
        data[(ps - 10) as usize..ps as usize].copy_from_slice(b"SWAPSPACE2");
        let s = src_from("swap", data);
        assert_eq!(identify(&s, 0, 8192).unwrap(), "swap");
    }

    /// 读失败必须与"没有已知签名"区分开：identify 对设备故障回 Err——
    /// 压成 Ok("unknown") 的后果是 resize 跳过 FS 步骤并报成功
    #[test]
    fn read_fault_is_error_not_unknown() {
        let s = src_from("fault", vec![0u8; 4096]);
        let _g = crate::dev::ReadFaultGuard::at(0);
        assert!(identify(&s, 0, 4096).is_err(), "device fault must surface as Err");
    }

    #[test]
    fn swap_detected_across_page_sizes() {
        // 创建机页 8K、宿主页 4K：候选探测命中 8192 处 magic
        let mut data = vec![0u8; 16384];
        let ps = 8192u64;
        data[(ps - 10) as usize..ps as usize].copy_from_slice(b"SWAPSPACE2");
        let s = src_from("swap8k", data);
        assert_eq!(identify(&s, 0, 8192).unwrap(), "swap");
    }

    #[test]
    fn swap_signature_at_32k_offset_not_detected() {
        // 候选集不含 32K（swapon 的 swap_get_header 跳过 0x8000），64K 处也没有 magic
        let mut data = vec![0u8; 65536];
        let ps = 32768u64;
        data[(ps - 10) as usize..ps as usize].copy_from_slice(b"SWAPSPACE2");
        let s = src_from("swap32k", data);
        assert_eq!(identify(&s, 0, 65536).unwrap(), "unknown");
    }

    /// 两个具名候选集：swapon 口径不含 32K，libblkid 口径含 32K（各自的依据见常量注释）
    #[test]
    fn swap_page_size_policies() {
        let swapon = swapon_activatable_pages();
        assert!(swapon.contains(&4096) && swapon.contains(&65536));
        assert!(!swapon.contains(&32768), "swapon 循环跳过 0x8000");
        let blkid = blkid_known_pages();
        assert!(blkid.contains(&32768), "libblkid 的 magic 表含 0x7ff6");
        // 顺序：本机页优先，其余按集合升序（本机页与固定集重合时去重）
        assert_eq!(swapon[0], host_page());
        assert_eq!(blkid[0], host_page());
        let mut sorted = swapon.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(swapon, sorted);
    }

    /// 探测结果由候选集决定：同一镜像在 libblkid 口径命中 32K，在 swapon 口径不命中；
    /// 分区长度不足以容纳候选页时不读越界
    #[test]
    fn swap_probe_is_driven_by_candidate_set() {
        let mut data = vec![0u8; 65536];
        data[32768 - 10..32768].copy_from_slice(b"SWAPSPACE2");
        let s = src_from("probe32k", data);
        assert_eq!(probe_swap_header(&s, 0, 65536, &blkid_known_pages()).unwrap(), Some(32768));
        assert_eq!(probe_swap_header(&s, 0, 65536, &swapon_activatable_pages()).unwrap(), None);
        // 长度小于候选页 → 跳过该候选，不判越界为命中
        assert_eq!(probe_swap_header(&s, 0, 4096, &[65536]).unwrap(), None);
        // 非 SWAPSPACE2 的 v0 签名不认
        let mut old = vec![0u8; 8192];
        old[4096 - 10..4096].copy_from_slice(b"SWAP-SPACE");
        let s2 = src_from("probev0", old);
        assert_eq!(probe_swap_header(&s2, 0, 8192, &blkid_known_pages()).unwrap(), None);
    }

    /// "本机激活不了的 swap"判据：两口径的差集只有 32K 一项，故它等价于
    /// "libblkid 认得出、swapon 认不出"
    #[test]
    fn unactivatable_swap_is_the_32k_case() {
        // 4K swap：两个口径都命中 ⇒ 不是"激活不了"
        let mut data = vec![0u8; 65536];
        data[4096 - 10..4096].copy_from_slice(b"SWAPSPACE2");
        assert!(!unactivatable_swap(&src_from("un_act4k", data), 0, 65536).unwrap());
        // 32K swap：只有 libblkid 口径命中
        let mut data = vec![0u8; 65536];
        data[32768 - 10..32768].copy_from_slice(b"SWAPSPACE2");
        assert!(unactivatable_swap(&src_from("un_act32k", data), 0, 65536).unwrap());
        // 非 swap 区：两个口径都落空
        assert!(!unactivatable_swap(&src_from("un_actnone", vec![0u8; 65536]), 0, 65536).unwrap());
        // 签名相对**区间起点**解释：同一段字节换个起点就不再命中
        let mut data = vec![0u8; 131072];
        data[65536 + 32768 - 10..65536 + 32768].copy_from_slice(b"SWAPSPACE2");
        let s = src_from("un_actoff", data);
        assert!(unactivatable_swap(&s, 65536, 65536).unwrap());
        assert!(!unactivatable_swap(&s, 0, 65536).unwrap());
    }

    #[test]
    fn hfsplus_magic_at_volume_header() {
        let mut data = vec![0u8; 4096];
        data[0x400..0x402].copy_from_slice(b"H+");
        let s = src_from("hfsplus", data);
        assert_eq!(identify(&s, 0, 4096).unwrap(), "hfsplus");
    }

    #[test]
    fn apfs_magic_at_object_header_end() {
        let mut data = vec![0u8; 4096];
        data[0x20..0x24].copy_from_slice(b"NXSB");
        let s = src_from("apfs", data);
        assert_eq!(identify(&s, 0, 4096).unwrap(), "apfs");
    }

    #[test]
    fn lvm2_pv_label_in_second_sector() {
        let mut data = vec![0u8; 4096];
        data[512..520].copy_from_slice(b"LABELONE");
        data[536..544].copy_from_slice(b"LVM2 001");
        let s = src_from("lvm", data);
        assert_eq!(identify(&s, 0, 4096).unwrap(), "lvm2_pv");
    }

    #[test]
    fn unknown_not_guessed() {
        let data = vec![0u8; 4096];
        let s = src_from("unk", data);
        assert_eq!(identify(&s, 0, 4096).unwrap(), "unknown");
    }

    #[test]
    fn squashfs_and_erofs_magic() {
        let mut data = vec![0u8; 8192];
        data[0..4].copy_from_slice(b"hsqs");
        let s = src_from("sq", data);
        assert_eq!(identify(&s, 0, 8192).unwrap(), "squashfs");

        let mut data = vec![0u8; 8192];
        data[1024..1028].copy_from_slice(&[0xE2, 0xE1, 0xF5, 0xE0]);
        let s = src_from("ero", data);
        assert_eq!(identify(&s, 0, 8192).unwrap(), "erofs");
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