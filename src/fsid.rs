//! 盘上内容识别。
//!
//! 结论类型 `ContentKind` 定义在 `domain`（平台无关）；本模块负责读盘与签名比对，
//! 对外的结论只有内容类别，不向上层泄漏具体偏移或字节布局。

use crate::dev::FileSource;
use std::io;

pub(crate) use crate::domain::{ContentKind, FsKind};

const SQUASHFS_MAGIC: &[u8; 4] = b"hsqs";
const EROFS_MAGIC: [u8; 4] = [0xE2, 0xE1, 0xF5, 0xE0];

/// LUKS1/LUKS2 头 magic，偏移 0（cryptsetup 头规范；util-linux
/// libblkid/src/superblocks/crypt.c 的 LUKS_MAGIC 同值）
const LUKS_MAGIC: &[u8; 6] = b"LUKS\xba\xbe";

/// BitLocker 引导扇区 OEM ID，偏移 3（util-linux libblkid/src/superblocks/bitlocker.c
/// 的 BDE_MAGIC_VISTA/WIN7 以 `\xeb\x52\x90`/`\xeb\x58\x90` 开头，其后即这段）
const BITLOCKER_MAGIC: &[u8; 8] = b"-FVE-FS-";

/// bcachefs 超级块内的 magic 偏移：bch_sb 起于字节 4096（BCH_SB_SECTOR = 8），
/// bch_csum 16 字节 + version/version_min/padding 8 字节后为 magic，即 sb+24
/// （kernel fs/bcachefs/bcachefs_format.h 的 struct bch_sb）
const BCACHEFS_MAGIC_OFFSET: u64 = 4096 + 24; // audit-allow: const-expr
/// BCHFS_MAGIC 的盘上字节序：UUID_INIT 把 128 位值按网络字节序写入 `__uuid_t`，
/// 故 c68573f6-66ce-90a9-d96a-60cf803df7ef 落盘为下列 16 字节
const BCACHEFS_MAGIC: [u8; 16] =
    [0xc6, 0x85, 0x73, 0xf6, 0x66, 0xce, 0x90, 0xa9, 0xd9, 0x6a, 0x60, 0xcf, 0x80, 0x3d, 0xf7, 0xef];

/// JFS 超级块偏移与 magic（util-linux libblkid/src/superblocks/jfs.c：kboff 32）
const JFS_SB_OFFSET: u64 = 32768;
const JFS_MAGIC: &[u8; 4] = b"JFS1";

/// UDF 卷识别序列首描述符偏移与签名（util-linux libblkid/src/superblocks/udf.c：@0x8000）
const UDF_VRS_OFFSET: u64 = 32768;
const UDF_VRS_MAGIC: &[u8; 5] = b"BEA01";

/// ReiserFS 超级块 @64KiB，s_magic（char[10]）在 sb+52；三个版本串见
/// kernel include/uapi/linux/magic.h
const REISERFS_MAGIC_OFFSET: u64 = 65536 + 52; // audit-allow: const-expr
const REISERFS_MAGICS: [&[u8]; 3] = [b"ReIsErFs", b"ReIsEr2Fs", b"ReIsEr3Fs"];

/// btrfs magic：磁盘上是 `__le64` 的 `BTRFS_MAGIC = 0x4D5F_5366_5248_425F`，小端落地即
/// `_BHRfS_M`（内核 `include/uapi/linux/btrfs_tree.h`、btrfs-progs
/// `kernel-shared/uapi/btrfs_tree.h`）。识别（[`probe_fs`]）与成员数判据
/// （[`crate::dev::occupancy::btrfs_members`]）共用这一份，按 8 字节原值比较
pub(crate) const BTRFS_MAGIC: [u8; 8] = 0x4D5F_5366_5248_425Fu64.to_le_bytes();

/// 把识别结论映射为对外名；`Unknown` → `"unknown"`。
///
/// `Unknown` 只表示"本工具没有认出受支持的内容"，不表示这段区域为空，也不表示
/// 可以安全跳过——"可供识别的字节不足"（区间太短）同样落在这里。写入路径是否跳过
/// FS 步骤由 [`crate::fsops::Growable`] 决定；"认得出却激活不了"的情形（页格式不匹配
/// 的 swap）在那里单独成变体，不并入 `Unknown`。所有回放"未识别"文案/JSON 的地方都走
/// 此处，不另写字面量
pub fn content_name(content: ContentKind) -> &'static str {
    content.as_str()
}

/// swap 探测命中：签名所在的候选页大小，以及该页在宿主 util-linux swapon 口径下
/// 是否能被激活（判定集合为 [`SWAPON_PAGES`]）
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SwapHit {
    pub page: u64,
    pub activatable: bool,
}

/// 一次字节区间探测的结果：命中的内容种类，以及未命中其他类型时探测到的 swap 候选
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FsProbe {
    /// 与 [`identify`] 的返回值同源：swap 只在本机可激活且头内自洽时计入
    pub content: ContentKind,
    /// 没有其他类型命中时才做 swap 探测；未命中为 `None`。`SwapHit` 只要求签名命中，
    /// 头内自洽与否由 `content` 表达
    pub swap: Option<SwapHit>,
}

/// 按**字节区间**识别文件系统：`base` 起、`len_bytes` 长的区域。
///
/// 刻意收字节而不收 LBA + 扇区大小：LBA 的单位取决于它来自哪张表——GPT 条目的 LBA 以
/// **表自身的** ss 计（4Kn 镜像未加 --sector-size 时 `g.ss != src.sector_size`），
/// MBR 条目的 LBA 以容器 ss 计。签名里带 ss 就等于要求每个调用点都为**别人的**单位负责，
/// 而它手上往往只有 LBA；收字节后，换算发生在唯一知道单位的那一层（读到表的地方），
/// 本模块退化为"给一段字节，判它是什么"，与 probe_swap_header / overlay_offset_at 同形
///
/// Err 只表达设备故障：调用方必须把它与"区间内没有已知签名"（`Ok(Unknown)`）区分开，
/// 否则 resize 会在 I/O 错误时把 FS 步骤整体跳过并报成功
pub fn identify(src: &FileSource, base: u64, len_bytes: u64) -> io::Result<ContentKind> {
    Ok(probe_fs(src, base, len_bytes)?.content)
}

/// 同一段区间的一次读盘结论：内容类别，以及未命中其他类型时的 swap 探测。写入路径要分辨
/// "空区域"与"识别不出、但元数据确实是 swap"时取 `swap`——[`identify`] 的 `Unknown` 两者
/// 都表达，无法支撑"swap 未重建"这条待办的判据
pub fn probe_fs(src: &FileSource, base: u64, len_bytes: u64) -> io::Result<FsProbe> {
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
        return Ok(FsProbe { content: ContentKind::Filesystem(FsKind::Squashfs), swap: None });
    }
    // EROFS: 0xE0F5E1E2 (LE) @1024（内核 erofs_fs.h EROFS_SUPER_MAGIC_V1，超级块 @1024）
    if let Some(b) = rd(1024, 4)?
        && b == EROFS_MAGIC
    {
        return Ok(FsProbe { content: ContentKind::Filesystem(FsKind::Erofs), swap: None });
    }
    // ext2/3/4: EXT_MAGIC (LE) @0x438（sb @1024，s_magic @sb+0x38）。magic 区分不出 2/3/4，
    // 而 e2fsck/resize2fs/tune2fs 对三者通用，故统一收敛到 Ext（具体 extN 只存在于 --fs 请求）
    if let Some(b) = rd(0x438, 2)?
        && u16::from_le_bytes([b[0], b[1]]) == EXT_MAGIC
    {
        return Ok(FsProbe { content: ContentKind::Filesystem(FsKind::Ext), swap: None });
    }
    // XFS: "XFSB" @0（BE 序 magic 0xC03B3998；libxfs xfs_format.h XFS_SB_MAGIC）
    if let Some(b) = rd(0, 4)?
        && &b == b"XFSB"
    {
        return Ok(FsProbe { content: ContentKind::Filesystem(FsKind::Xfs), swap: None });
    }
    // btrfs: magic @0x10040（超级块 @64 KiB，magic 在 +0x40，按 8 字节原值比较）
    if let Some(b) = rd(0x10040, 8)?
        && b == BTRFS_MAGIC
    {
        return Ok(FsProbe { content: ContentKind::Filesystem(FsKind::Btrfs), swap: None });
    }
    // F2FS: 0xF2F52010 (LE) @0x400，第二副本 @0x1400（内核 __get_raw_super 依次读两份，
    // 主 SB 损坏时回退第二份，故两处都查）
    for off in [0x400u64, 0x1400] {
        if let Some(b) = rd(off, 4)?
            && u32::from_le_bytes(b[..4].try_into().unwrap()) == 0xF2F5_2010
        {
            return Ok(FsProbe { content: ContentKind::Filesystem(FsKind::F2fs), swap: None });
        }
    }
    // exFAT: "EXFAT   "（8 字节含 3 尾随空格）@3 —— 微软规范 §3.1 FileSystemName
    if let Some(b) = rd(3, 8)?
        && &b == b"EXFAT   "
    {
        return Ok(FsProbe { content: ContentKind::Filesystem(FsKind::Exfat), swap: None });
    }
    // FAT32: "FAT32   " @0x52；FAT12/16 在 @0x36 —— 微软 EFI FAT32 规范 §5 的 BS_FilSysType
    if let Some(b) = rd(0x52, 8)?
        && &b == b"FAT32   "
    {
        return Ok(FsProbe { content: ContentKind::Filesystem(FsKind::Vfat), swap: None });
    }
    if let Some(b) = rd(0x36, 8)?
        && (&b == b"FAT12   " || &b == b"FAT16   ")
    {
        return Ok(FsProbe { content: ContentKind::Filesystem(FsKind::Vfat), swap: None });
    }
    // NTFS: "NTFS    " @3（$Boot OEM ID；ntfs-3g libntfs-3g/bootsect.c
    // ntfs_boot_sector_is_ntfs：oem_id == "NTFS    "）
    if let Some(b) = rd(3, 8)?
        && &b == b"NTFS    "
    {
        return Ok(FsProbe { content: ContentKind::Filesystem(FsKind::Ntfs), swap: None });
    }
    // HFS+/HFSX: 卷头 @1024，签名 "H+"/"HX"（Apple TN1150 HFS Plus Volume Format：
    // "The volume signature is the value 'H+' … 'HX' for HFSX"）
    for sig in [b"H+", b"HX"] {
        if let Some(b) = rd(0x400, 2)?
            && b == sig
        {
            return Ok(FsProbe { content: ContentKind::Filesystem(FsKind::HfsPlus), swap: None });
        }
    }
    // APFS：容器超块 nx_superblock_t 在块 0；对象头 obj_phys_t 共 32 字节
    // （o_cksum8 + o_oid8 + o_xid8 + o_type4 + o_subtype4），magic "NXSB" @0x20
    // （Apple File System Reference nx_superblock_t；carving 文献同：bytes 32..36 == 'NXSB'）
    if let Some(b) = rd(0x20, 4)?
        && &b == b"NXSB"
    {
        return Ok(FsProbe { content: ContentKind::Filesystem(FsKind::Apfs), swap: None });
    }
    // LVM2 PV：标签头 "LABELONE" 位于前 4 个 512B 扇区之一（pvcreate 默认第 2 扇区），
    // 标签头内偏移 24 处为类型串 "LVM2 001"（LVM2 主干 lib/label/label.h 的 label_header：
    // packed 的 id[8]+sector_xl[8]+crc_xl[4]+offset_xl[4]+type[8]；扫描范围量
    // LABEL_SCAN_SECTORS = 4）
    for off in [0u64, 512, 1024, 1536] {
        if let Some(b) = rd(off, 32)?
            && &b[0..8] == b"LABELONE"
            && &b[24..32] == b"LVM2 001"
        {
            return Ok(FsProbe { content: ContentKind::Lvm2Pv, swap: None });
        }
    }
    // LUKS1/LUKS2：magic @0（cryptsetup 头规范；util-linux
    // libblkid/src/superblocks/crypt.c 的 LUKS_MAGIC）
    if let Some(b) = rd(0, 6)?
        && b == LUKS_MAGIC
    {
        return Ok(FsProbe { content: ContentKind::Luks, swap: None });
    }
    // BitLocker：OEM ID @3（util-linux libblkid/src/superblocks/bitlocker.c 的
    // BDE_MAGIC_VISTA/WIN7：`\xeb\x52\x90-FVE-FS-` / `\xeb\x58\x90-FVE-FS-`）
    if let Some(b) = rd(3, 8)?
        && b == BITLOCKER_MAGIC
    {
        return Ok(FsProbe { content: ContentKind::BitLocker, swap: None });
    }
    // bcachefs：bch_sb @4096（BCH_SB_SECTOR 8），magic 为 __uuid_t @sb+24；
    // BCHFS_MAGIC c68573f6-66ce-90a9-d96a-60cf803df7ef（kernel
    // fs/bcachefs/bcachefs_format.h）
    if let Some(b) = rd(BCACHEFS_MAGIC_OFFSET, 16)?
        && b[..] == BCACHEFS_MAGIC[..]
    {
        return Ok(FsProbe { content: ContentKind::Filesystem(FsKind::Bcachefs), swap: None });
    }
    // JFS：超级块 @32768，magic "JFS1"（util-linux libblkid/src/superblocks/jfs.c，kboff 32）
    if let Some(b) = rd(JFS_SB_OFFSET, 4)?
        && b == JFS_MAGIC
    {
        return Ok(FsProbe { content: ContentKind::Filesystem(FsKind::Jfs), swap: None });
    }
    // UDF：卷识别序列首描述符 @32768，"BEA01"（util-linux
    // libblkid/src/superblocks/udf.c 的 VRS @0x8000）
    if let Some(b) = rd(UDF_VRS_OFFSET, 5)?
        && b == UDF_VRS_MAGIC
    {
        return Ok(FsProbe { content: ContentKind::Filesystem(FsKind::Udf), swap: None });
    }
    // ReiserFS：超级块 @64KiB，s_magic @sb+52；三个版本串见
    // kernel include/uapi/linux/magic.h，s_magic 为 char[10]，按 C 字符串前缀比较
    if let Some(b) = rd(REISERFS_MAGIC_OFFSET, 10)?
        && REISERFS_MAGICS.iter().any(|m| b.starts_with(m))
    {
        return Ok(FsProbe { content: ContentKind::Filesystem(FsKind::Reiserfs), swap: None });
    }
    // swap: 签名位于"创建机页大小"末尾 10 字节（内核 include/linux/swap.h
    // union swap_header：reserved[PAGE_SIZE-10] + magic[10]），盘上不记录该页大小，
    // 故由 probe_swap_header 按候选集探测。候选集是 swapon 口径在前、仅 libblkid 认得的
    // 页大小（32K）在后：第一次命中同时给出"宿主 swapon 能不能把它当 swap 处理"
    // （activatable 由 SWAPON_PAGES 判定）。`content` 仍要求命中页可激活且头内自洽
    // （版本与页数，见 swap_header_consistent）：签名可能只是某段普通数据末尾的巧合，
    // 而误判 swap 会让扩容路径据此 mkswap，用新头覆盖该分区首个页
    let swap = probe_swap_header(src, base, len_bytes, &swap_probe_pages())?
        .map(|page| SwapHit { page, activatable: SWAPON_PAGES.contains(&page) });
    let content = match swap {
        Some(h) if h.activatable => {
            if swap_header_consistent(src, base, len_bytes, h.page)? {
                ContentKind::Swap
            } else {
                ContentKind::Unknown
            }
        }
        _ => ContentKind::Unknown,
    };
    Ok(FsProbe { content, swap })
}

/// ext2/3/4 超级块 magic（s_magic，盘上偏移见 [`probe_fs`]；
/// 内核 include/uapi/linux/magic.h 的 EXT2_SUPER_MAGIC = 0xEF53）
const EXT_MAGIC: u16 = 0xEF53;

/// swap v1 签名（内核 include/linux/swap.h union swap_header）
const SWAP_MAGIC: &[u8; 10] = b"SWAPSPACE2";

/// 宿主 swapon 能处理的页大小：util-linux v2.42.4 `sys-utils/swapon.c` 的探测循环
/// `for (page = 0x1000; page <= MAX_PAGESIZE; page <<= 1)`（`MAX_PAGESIZE` = 64K），
/// 其中 `if (page == 0x8000) continue;`。跳过 32K 是 util-linux 用户态探测策略
/// （源码注释称该页大小似不受支持），不代表内核不接受 32K 页的 swap 头
const SWAPON_PAGES: [u64; 4] = [4096, 8192, 16384, 65536];

/// 能认出 swap 元数据的页大小：util-linux v2.42.4 `libblkid/src/superblocks/swap.c`
/// 的 `swap_idinfo.magics`，sboff = 0xff6 / 0x1ff6 / 0x3ff6 / 0x7ff6 / 0xfff6（含 32K），
/// `minsz` = 10 × 4096。
///
/// 两表均以上述版本为基准；上游调整 swapon 的跳过项或 blkid 的偏移表时，
/// 这两张表与 swap 相关测试须一并复查
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

/// libblkid 口径的候选页大小（"能否识别出元数据"；含 32K，宽于激活能力）
pub fn blkid_known_pages() -> Vec<u64> {
    ordered_pages(&BLKID_PAGES)
}

/// [`probe_fs`] 的 swap 候选顺序：swapon 口径（本机页优先、其余升序）在前，其后追加
/// 仅 libblkid 认得的页大小（32K）。前者在前保证"可激活的命中"优先出现；后者只用于
/// 认出元数据，激活与否另由 [`SWAPON_PAGES`] 判定
fn swap_probe_pages() -> Vec<u64> {
    let mut pages = ordered_pages(&SWAPON_PAGES);
    pages.extend(BLKID_PAGES.iter().copied().filter(|p| !SWAPON_PAGES.contains(p)));
    pages
}

/// swap 签名探测（唯一实现）：在候选页大小各自的末尾 10 字节找 SWAPSPACE2，
/// 返回命中的页大小（magic 偏移 = page − 10）。候选集由调用方按语义选择
/// （`SWAPON_PAGES` / `BLKID_PAGES` 两个具名集合），本函数不做取舍。只认 SWAPSPACE2：v0 的 "SWAP-SPACE" 内核早已不再写入，
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
        let at = base
            .checked_add(page)
            .and_then(|a| a.checked_sub(10))
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "swap header offset overflows"))?;
        src.read_at(at, &mut magic)?;
        if &magic == SWAP_MAGIC {
            return Ok(Some(page));
        }
    }
    Ok(None)
}

/// swap v1 头内自洽核对：`info` 区紧跟 `bootbits[1024]`，`version` @1024、`last_page` @1028
/// （内核 include/linux/swap.h union swap_header；mkswap 写 version = 1，last_page 为
/// 最高页号）。页大小不记录在头里，由 magic 命中的候选页给出。
///
/// `last_page` 与分区页数只比"不超过"（`last_page` < 页数）：扩容前记下的旧页数小于
/// 当前分区页数是合法状态（swap 内容随扩容重建前不会被读取），
/// 故不要求 `last_page` 等于 页数 − 1。
/// 读失败照旧回 Err——设备故障不能压成"未识别"
fn swap_header_consistent(src: &FileSource, base: u64, len_bytes: u64, page: u64) -> io::Result<bool> {
    if !(4096..=65536).contains(&page) || !page.is_power_of_two() {
        return Ok(false);
    }
    let at = base
        .checked_add(1024)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "swap header offset overflows"))?;
    let mut info = [0u8; 8];
    src.read_at(at, &mut info)?;
    let version = u32::from_le_bytes(info[0..4].try_into().unwrap());
    let last_page = u32::from_le_bytes(info[4..8].try_into().unwrap());
    Ok(version == 1 && u64::from(last_page) < len_bytes / page)
}

/// OpenWrt combined 布局的 RW overlay 起点（字节，相对分区头）。公式须与
/// fstools libfstools/rootdisk.c 一致（mount_root 建 loop 用的 lo_offset 即此值）：
/// - squashfs 4.0 → bytes_used __le64 @0x28（内核 squashfs_fs.h：超级块为 5×u32 + 6×u16，
///   其后 u64 root_inode@0x20、u64 bytes_used@0x28）。版本字段 `s_major`/`s_minor` 是
///   __le16 @0x1c/@0x1e；本布局只对 4.0 成立，其它版本的超级块按未命中处理
/// - EROFS → blocks u32 @超级块+0x24 左移 blkszbits u8 @超级块+0x0C（超级块 @1024）
///
/// 统一 64K 上对齐：fstools libfstools/rootdisk.c 的 ROOTDEV_OVERLAY_ALIGN = 64 * 1024，
/// 对齐式 `(off + ALIGN - 1) & ~(ALIGN - 1)` 与 `rootdisk_volume_find("rootfs_data")` 同形。
///
/// 信任边界：以上是 OpenWrt 的镜像布局约定，不是 squashfs / EROFS 格式规范对"镜像尾
/// 必有 RW 层"的保证。EROFS 规范（kernel.org erofs ondisk/core_ondisk：`blocks`、`inos`
/// 供 statvfs 报告，动态生成的镜像可置 0，且明确不应以该字段校验地址合法性）意味着此处
/// 算出的偏移只是**布局提示**，不能当作"该位置确实有一个 RW 层"的证明。真实存在与否由
/// 调用方在该偏移处重新识别内层来确认；识别不出时不写那段区域（见 `Growable::OverlayPending`）。
/// `blkszbits` 的格式上界没有权威取值可用，故只查下界；超大值由 `checked_shl`/`checked_mul`
/// 与调用方的区间边界一并挡成未命中或拒绝。
///
/// 解析失败或 0 → None。读错误如实上抛：读不出来与"签名不命中"是两回事，
/// 把前者折叠成 None 会让调用方把盘上事实当成"没有 overlay"。分区不足以容纳
/// 超级块读区同样按未命中——那不是读故障，是区间不存在的事实
pub fn overlay_offset_at(src: &FileSource, part_offset: u64, part_len_bytes: u64) -> io::Result<Option<u64>> {
    const ALIGN: u64 = 64 * 1024; // audit-allow: const-expr
    let mut sb = [0u8; 2048];
    // 读区越出分区尾按未命中：那不是读故障，是区间不存在的事实；且闸若缺位，
    // 越界读会吃到分区之后的邻分区字节（或 EOF 上的 UnexpectedEof，被调用方
    // 当成 I/O 故障退 4）
    if sb.len() as u64 > part_len_bytes {
        return Ok(None);
    }
    src.read_at(part_offset, &mut sb)?;
    let raw = if &sb[0..4] == SQUASHFS_MAGIC {
        let major = u16::from_le_bytes(sb[0x1c..0x1e].try_into().unwrap());
        let minor = u16::from_le_bytes(sb[0x1e..0x20].try_into().unwrap());
        // 3.x 的超级块字段布局不同，不能按 4.x 的偏移取 bytes_used
        if (major, minor) != (4, 0) {
            return Ok(None);
        }
        u64::from_le_bytes(sb[0x28..0x30].try_into().unwrap())
    } else if sb[1024..1028] == EROFS_MAGIC {
        let blkszbits = sb[1024 + 0x0C] as u32; // audit-allow: index-in-bounds
        // blkszbits 是盘上可控值：内核按自身 LOG_BLOCK_SIZE 匹配，故只查下界
        if blkszbits < 9 {
            return Ok(None);
        }
        let blocks = u32::from_le_bytes(sb[1024 + 0x24..1024 + 0x28].try_into().unwrap()) as u64; // audit-allow: index-in-bounds
        // 乘 2^blkszbits 走 checked_mul：checked_shl 只挡位移量越界，不挡高位丢弃——
        // 盘上可控的 blocks/blkszbits 组合会把溢出静默折成一个小值，算出伪 overlay 偏移
        let unit = match 1u64.checked_shl(blkszbits) {
            Some(v) => v,
            None => return Ok(None),
        };
        match blocks.checked_mul(unit) {
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

    fn src_from(tag: &str, data: Vec<u8>) -> crate::testdir::TestSource<FileSource> {
        let dir = crate::testdir::test_dir(tag);
        let path = dir.join("disk.img");
        std::fs::write(&path, &data).unwrap();
        let f = std::fs::OpenOptions::new().read(true).write(true).open(&path).unwrap();
        let src = FileSource {
            identity: crate::dev::TargetIdentity::resolve_image(&path),
            block_identity: None,
            file: f,
            path,
            sector_size: 512,
            size: data.len() as u64,
            is_block: false,
            record_journal: None,
            fingerprint: Default::default(),
            loop_mapping: None,
            owner: None,
        };
        crate::testdir::TestSource::new(dir, src)
    }

    #[test]
    fn ext_magic() {
        let mut data = vec![0u8; 4096];
        data[0x438..0x43A].copy_from_slice(&0xEF53u16.to_le_bytes());
        let s = src_from("ext", data);
        assert_eq!(identify(&s, 0, 4096).unwrap(), ContentKind::Filesystem(FsKind::Ext));
    }

    #[test]
    fn ntfs_oem_id() {
        let mut data = vec![0u8; 4096];
        data[3..11].copy_from_slice(b"NTFS    ");
        let s = src_from("ntfs", data);
        assert_eq!(identify(&s, 0, 4096).unwrap(), ContentKind::Filesystem(FsKind::Ntfs));
    }

    #[test]
    fn swap_magic_at_page_tail() {
        let mut data = vec![0u8; 8192];
        let ps = 4096u64;
        data[(ps - 10) as usize..ps as usize].copy_from_slice(b"SWAPSPACE2");
        // 2 页的分区：header version = 1、last_page = 1（最高页号）
        data[1024..1028].copy_from_slice(&1u32.to_le_bytes());
        data[1028..1032].copy_from_slice(&1u32.to_le_bytes());
        let s = src_from("swap", data);
        assert_eq!(identify(&s, 0, 8192).unwrap(), ContentKind::Swap);
    }

    /// 读失败必须与"没有已知签名"区分开：identify 对设备故障回 Err——
    /// 压成 Ok(None) 的后果是 resize 跳过 FS 步骤并报成功
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
        // 区间只有 8 KiB = 1 页：version = 1、last_page = 0
        data[1024..1028].copy_from_slice(&1u32.to_le_bytes());
        let s = src_from("swap8k", data.clone());
        assert_eq!(identify(&s, 0, 8192).unwrap(), ContentKind::Swap);
        // 签名在、头内不自洽：version 为 0
        let mut no_version = data.clone();
        no_version[1024..1028].fill(0);
        assert_eq!(identify(&src_from("swap8k_v0", no_version), 0, 8192).unwrap(), ContentKind::Unknown);
        // last_page 指向分区之外（这里 1 页的分区只有第 0 页）
        let mut beyond = data.clone();
        beyond[1028..1032].copy_from_slice(&1u32.to_le_bytes());
        assert_eq!(identify(&src_from("swap8k_beyond", beyond), 0, 8192).unwrap(), ContentKind::Unknown);
        // 记的是扩容前的旧页数（小于当前页数）仍是合法 swap
        let mut grown = data;
        grown.resize(32768, 0);
        grown[1028..1032].copy_from_slice(&1u32.to_le_bytes());
        assert_eq!(identify(&src_from("swap8k_grown", grown), 0, 32768).unwrap(), ContentKind::Swap);
    }

    #[test]
    fn swap_signature_at_32k_offset_not_detected() {
        // 32K 页只在 libblkid 口径内，不在可激活集合；identify 要求可激活，
        // 64K 处也没有 magic，故判未识别
        let mut data = vec![0u8; 65536];
        let ps = 32768u64;
        data[(ps - 10) as usize..ps as usize].copy_from_slice(b"SWAPSPACE2");
        let s = src_from("swap32k", data);
        assert_eq!(identify(&s, 0, 65536).unwrap(), ContentKind::Unknown);
    }

    /// 候选集的两个口径：swapon 口径不含 32K（各自的依据见常量注释），仅 libblkid
    /// 认得的页大小追加在后
    #[test]
    fn swap_page_size_policies() {
        let pages = swap_probe_pages();
        assert!(pages.contains(&4096) && pages.contains(&65536));
        assert!(pages.contains(&32768), "libblkid 的 magic 表含 0x7ff6");
        // 可激活的候选在前：本机页优先，其余按集合升序（本机页与固定集重合时去重）
        let activatable = &pages[..SWAPON_PAGES.len()];
        assert_eq!(activatable[0], host_page());
        let mut sorted = activatable.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(activatable, sorted.as_slice());
        assert!(!activatable.contains(&32768), "swapon 循环跳过 0x8000");
        // 仅 libblkid 认得的页大小排在可激活候选之后
        assert_eq!(&pages[SWAPON_PAGES.len()..], &[32768]);
    }

    /// 探测结果由候选集决定：同一镜像在 libblkid 口径命中 32K，在 swapon 口径不命中；
    /// 分区长度不足以容纳候选页时不读越界
    #[test]
    fn swap_probe_is_driven_by_candidate_set() {
        let mut data = vec![0u8; 65536];
        data[32768 - 10..32768].copy_from_slice(b"SWAPSPACE2");
        let s = src_from("probe32k", data);
        assert_eq!(probe_swap_header(&s, 0, 65536, &blkid_known_pages()).unwrap(), Some(32768));
        assert_eq!(probe_swap_header(&s, 0, 65536, &SWAPON_PAGES).unwrap(), None);
        // 长度小于候选页 → 跳过该候选，不判越界为命中
        assert_eq!(probe_swap_header(&s, 0, 4096, &[65536]).unwrap(), None);
        // 非 SWAPSPACE2 的 v0 签名不认
        let mut old = vec![0u8; 8192];
        old[4096 - 10..4096].copy_from_slice(b"SWAP-SPACE");
        let s2 = src_from("probev0", old);
        assert_eq!(probe_swap_header(&s2, 0, 8192, &blkid_known_pages()).unwrap(), None);
    }

    /// 一次探测给出两类事实：`kind` 只在命中页可激活且头内自洽时成立，`swap` 只要求
    /// 签名命中。差集只有 32K 一项（见 SWAPON_PAGES / BLKID_PAGES 的依据），故 32K
    /// 命中即"本机激活不了"——写入路径据此报"swap 未重建"而不是跳过 FS 步骤
    #[test]
    fn probe_fs_reports_swap_hit_and_activation() {
        // 4K swap：两个口径都命中 ⇒ 可激活，identify 认作 swap
        let mut data = vec![0u8; 65536];
        data[4096 - 10..4096].copy_from_slice(b"SWAPSPACE2");
        data[1024..1028].copy_from_slice(&1u32.to_le_bytes());
        data[1028..1032].copy_from_slice(&1u32.to_le_bytes());
        let p = probe_fs(&src_from("pf4k", data), 0, 65536).unwrap();
        assert_eq!(p.content, ContentKind::Swap);
        assert_eq!(p.swap, Some(SwapHit { page: 4096, activatable: true }));
        // 32K swap（12 页完整 v1 头）：只有 libblkid 口径命中 ⇒ 不可激活，identify 仍是 None
        let mut data = vec![0u8; 12 * 32768];
        data[32768 - 10..32768].copy_from_slice(b"SWAPSPACE2");
        data[1024..1028].copy_from_slice(&1u32.to_le_bytes());
        data[1028..1032].copy_from_slice(&1u32.to_le_bytes());
        let p = probe_fs(&src_from("pf32k", data.clone()), 0, 12 * 32768).unwrap();
        assert_eq!(p.content, ContentKind::Unknown);
        assert_eq!(p.swap, Some(SwapHit { page: 32768, activatable: false }));
        // 同一镜像抹掉头内字段：签名仍被认出，内容判定不变（不可激活这条独立于头内自洽）
        let mut magic_only = data;
        magic_only[1024..1032].fill(0);
        let p = probe_fs(&src_from("pf32k_nohdr", magic_only), 0, 12 * 32768).unwrap();
        assert_eq!(p.content, ContentKind::Unknown);
        assert_eq!(p.swap, Some(SwapHit { page: 32768, activatable: false }));
        // 空区域：两个事实都落空
        let p = probe_fs(&src_from("pfnone", vec![0u8; 65536]), 0, 65536).unwrap();
        assert_eq!(p.content, ContentKind::Unknown);
        assert_eq!(p.swap, None);
        // 签名相对**区间起点**解释：同一段字节换个起点就不再命中
        let mut data = vec![0u8; 131072];
        data[65536 + 32768 - 10..65536 + 32768].copy_from_slice(b"SWAPSPACE2");
        data[65536 + 1024..65536 + 1028].copy_from_slice(&1u32.to_le_bytes());
        let s = src_from("pfoff", data);
        assert_eq!(probe_fs(&s, 65536, 65536).unwrap().swap, Some(SwapHit { page: 32768, activatable: false }));
        assert_eq!(probe_fs(&s, 0, 65536).unwrap().swap, None);
    }

    #[test]
    fn hfsplus_magic_at_volume_header() {
        let mut data = vec![0u8; 4096];
        data[0x400..0x402].copy_from_slice(b"H+");
        let s = src_from("hfsplus", data);
        assert_eq!(identify(&s, 0, 4096).unwrap(), ContentKind::Filesystem(FsKind::HfsPlus));
    }

    #[test]
    fn apfs_magic_at_object_header_end() {
        let mut data = vec![0u8; 4096];
        data[0x20..0x24].copy_from_slice(b"NXSB");
        let s = src_from("apfs", data);
        assert_eq!(identify(&s, 0, 4096).unwrap(), ContentKind::Filesystem(FsKind::Apfs));
    }

    #[test]
    fn luks_and_bitlocker_magic() {
        let mut data = vec![0u8; 4096];
        data[0..6].copy_from_slice(LUKS_MAGIC);
        assert_eq!(identify(&src_from("luks", data), 0, 4096).unwrap(), ContentKind::Luks);

        let mut data = vec![0u8; 4096];
        data[3..11].copy_from_slice(BITLOCKER_MAGIC);
        assert_eq!(identify(&src_from("bitlocker", data), 0, 4096).unwrap(), ContentKind::BitLocker);
    }

    #[test]
    fn bcachefs_magic_at_superblock_plus_24() {
        let mut data = vec![0u8; 8192];
        data[BCACHEFS_MAGIC_OFFSET as usize..BCACHEFS_MAGIC_OFFSET as usize + 16]
            .copy_from_slice(&BCACHEFS_MAGIC);
        assert_eq!(identify(&src_from("bcachefs", data), 0, 8192).unwrap(), ContentKind::Filesystem(FsKind::Bcachefs));
        // 区间不到 magic 处：读不出那些字节 → 不命中，不越界
        let mut short = vec![0u8; 4096];
        short[0..4].copy_from_slice(b"XFSB");
        assert_eq!(identify(&src_from("bcachefs_short", short), 0, 4096).unwrap(), ContentKind::Filesystem(FsKind::Xfs));
    }

    #[test]
    fn jfs_udf_and_reiserfs_magic() {
        let mut data = vec![0u8; 32768 + 4];
        data[JFS_SB_OFFSET as usize..JFS_SB_OFFSET as usize + 4].copy_from_slice(JFS_MAGIC);
        assert_eq!(identify(&src_from("jfs", data), 0, 32772).unwrap(), ContentKind::Filesystem(FsKind::Jfs));

        let mut data = vec![0u8; 32768 + 5];
        data[UDF_VRS_OFFSET as usize..UDF_VRS_OFFSET as usize + 5].copy_from_slice(UDF_VRS_MAGIC);
        assert_eq!(identify(&src_from("udf", data), 0, 32773).unwrap(), ContentKind::Filesystem(FsKind::Udf));

        for (tag, magic) in [("rfs1", &b"ReIsErFs"[..]), ("rfs2", &b"ReIsEr2Fs"[..]), ("rfs3", &b"ReIsEr3Fs"[..])] {
            let mut data = vec![0u8; REISERFS_MAGIC_OFFSET as usize + 10];
            data[REISERFS_MAGIC_OFFSET as usize..REISERFS_MAGIC_OFFSET as usize + magic.len()]
                .copy_from_slice(magic);
            assert_eq!(
                identify(&src_from(tag, data), 0, REISERFS_MAGIC_OFFSET + 10).unwrap(),
                ContentKind::Filesystem(FsKind::Reiserfs)
            );
        }
    }

    #[test]
    fn lvm2_pv_label_in_second_sector() {
        let mut data = vec![0u8; 4096];
        data[512..520].copy_from_slice(b"LABELONE");
        data[536..544].copy_from_slice(b"LVM2 001");
        let s = src_from("lvm", data);
        assert_eq!(identify(&s, 0, 4096).unwrap(), ContentKind::Lvm2Pv);
    }

    #[test]
    fn unknown_not_guessed() {
        let data = vec![0u8; 4096];
        let s = src_from("unk", data);
        assert_eq!(identify(&s, 0, 4096).unwrap(), ContentKind::Unknown);
    }

    #[test]
    fn squashfs_and_erofs_magic() {
        let mut data = vec![0u8; 8192];
        data[0..4].copy_from_slice(b"hsqs");
        let s = src_from("sq", data);
        assert_eq!(identify(&s, 0, 8192).unwrap(), ContentKind::Filesystem(FsKind::Squashfs));

        let mut data = vec![0u8; 8192];
        data[1024..1028].copy_from_slice(&[0xE2, 0xE1, 0xF5, 0xE0]);
        let s = src_from("ero", data);
        assert_eq!(identify(&s, 0, 8192).unwrap(), ContentKind::Filesystem(FsKind::Erofs));
    }

    /// overlay 起点公式：squashfs bytes_used 上取 64K 对齐；EROFS blocks<<blkszbits 同；
    /// 无根 fs 魔数 → None；blkszbits 溢出 → None；squashfs 非 4.0 → None
    #[test]
    fn overlay_offset_fstools_alignment() {
        // squashfs：bytes_used = 0x2_9C00 → 上对齐 64K = 0x2_0000... 验证非对齐值上取
        let mut data = vec![0u8; 65536];
        data[0..4].copy_from_slice(b"hsqs");
        data[0x1c..0x1e].copy_from_slice(&4u16.to_le_bytes());
        data[0x1e..0x20].copy_from_slice(&0u16.to_le_bytes());
        data[0x28..0x30].copy_from_slice(&0x29C00u64.to_le_bytes());
        let s = src_from("ovl", data.clone());
        assert_eq!(overlay_offset_at(&s, 0, 65536).unwrap(), Some(0x30000));
        // 已对齐值不变
        data[0x28..0x30].copy_from_slice(&0x30000u64.to_le_bytes());
        let s = src_from("ovl2", data.clone());
        assert_eq!(overlay_offset_at(&s, 0, 65536).unwrap(), Some(0x30000));
        // 3.x：超级块布局不同，0x28 不是 bytes_used → None
        data[0x1c..0x1e].copy_from_slice(&3u16.to_le_bytes());
        let s = src_from("ovl7", data.clone());
        assert_eq!(overlay_offset_at(&s, 0, 65536).unwrap(), None);
        data[0x1c..0x1e].copy_from_slice(&4u16.to_le_bytes());
        data[0x1e..0x20].copy_from_slice(&1u16.to_le_bytes());
        let s = src_from("ovl8", data);
        assert_eq!(overlay_offset_at(&s, 0, 65536).unwrap(), None);
        // EROFS：blocks=0x30 << blkszbits=12 = 0x30000
        let mut data = vec![0u8; 65536];
        data[1024..1028].copy_from_slice(&[0xE2, 0xE1, 0xF5, 0xE0]);
        data[1024 + 0x0C] = 12;
        data[1024 + 0x24..1024 + 0x28].copy_from_slice(&0x30u32.to_le_bytes());
        let s = src_from("ovl3", data.clone());
        assert_eq!(overlay_offset_at(&s, 0, 65536).unwrap(), Some(0x30000));
        // blkszbits 溢出 → None
        data[1024 + 0x0C] = 99;
        let s = src_from("ovl4", data);
        assert_eq!(overlay_offset_at(&s, 0, 65536).unwrap(), None);
        // blkszbits < 9（无下界语义）→ None
        let mut data = vec![0u8; 65536];
        data[1024..1028].copy_from_slice(&[0xE2, 0xE1, 0xF5, 0xE0]);
        data[1024 + 0x0C] = 8;
        let s = src_from("ovl6", data);
        assert_eq!(overlay_offset_at(&s, 0, 65536).unwrap(), None);
        // 无魔数 → None
        let data = vec![0u8; 65536];
        let s = src_from("ovl5", data);
        assert_eq!(overlay_offset_at(&s, 0, 65536).unwrap(), None);
    }
}
