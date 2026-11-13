//! swap 重建与擦除：分区落位后的内容重建。

use super::*;

/// 旧 swap 首部的卷标。"读不出来"与"确实没有"分开：只有后者才允许省略 -L
#[derive(Debug)]
pub enum SwapLabel {
    /// 16 字节字段全零：这块 swap 本来就没有卷标
    Absent,
    /// 卷标字段读不出来
    Unreadable(String),
    Present(Vec<u8>),
}

/// 旧 swap 首部给出的身份。三态而不是两个 `Option`：读不出来时重建
/// 无法保住 fstab/resume 依赖的内容，必须拒绝，不能当成"本来就没有"
#[derive(Debug)]
pub enum SwapIdentity {
    /// 该位置没有有效 swap 签名（未格式化或非 swap）
    NotSwap,
    /// 有签名，但首部读不出来（探测或 UUID 读取失败、范围换算溢出）
    Unreadable(String),
    Known { uuid: Option<[u8; 16]>, label: SwapLabel },
}

impl SwapIdentity {
    pub(crate) fn has_uuid(&self) -> bool {
        matches!(self, Self::Known { uuid: Some(_), .. })
    }
}

/// swap 重建：mkswap -U <uuid> [-L <label>]，保持 UUID/卷标以维持 fstab 兼容
/// （-U/-L 见 man mkswap：UUID 存取同序、无端转换，全零视为未设置走随机生成）。
/// 卷标按字节透传：sws_volume 是不做编码校验的固定宽度字段，转成 String 再写回必然失真。
/// 返回的说明与实际写入同源，调用方不得另写一套文案。
/// 失败由调用方转成一条 Swap 待办（swap 内容可弃，但 fstab 指向的 UUID 需人工 mkswap
/// 恢复），而不是把"重建没做成"洗成成功
pub fn recreate_swap(
    src: &FileSource,
    part: u32,
    identity: &SwapIdentity,
) -> Result<String, FsError> {
    let (uuid, label, note) = match identity {
        SwapIdentity::Unreadable(why) => {
            return Err(FsError::Io(io::Error::other(format!(
                "cannot rebuild swap: the old signature could not be read ({why})"
            ))))
        }
        SwapIdentity::NotSwap => (None, None, "old signature had no UUID — mkswap wrote a fresh one".to_string()),
        SwapIdentity::Known { uuid, label } => {
            let note = match (uuid.is_some(), label) {
                (true, SwapLabel::Present(_)) => "UUID and volume label preserved".to_string(),
                (true, SwapLabel::Absent) => "UUID preserved (the old signature had no volume label)".to_string(),
                (true, SwapLabel::Unreadable(why)) => format!(
                    "UUID preserved; the old volume label could not be read ({why}), so none was written"
                ),
                (false, SwapLabel::Present(_)) => {
                    "old signature had no UUID — mkswap wrote a fresh one (volume label preserved)".to_string()
                }
                (false, SwapLabel::Absent) => {
                    "old signature had no UUID or volume label — mkswap wrote a fresh UUID and no label".to_string()
                }
                (false, SwapLabel::Unreadable(why)) => format!(
                    "old signature had no UUID — mkswap wrote a fresh one; the old volume label \
                     could not be read ({why}), so none was written"
                ),
            };
            (uuid.as_ref(), Some(label), note)
        }
    };
    with_partition_device(src, part, |dev| {
        let mut args: Vec<OsString> = Vec::new();
        if let Some(u) = uuid {
            // swap UUID 按原字节序列化为标准 8-4-4-4-12（mkswap 存取同序，无混合端转换）
            let hex: String = u.iter().map(|b| format!("{b:02x}")).collect();
            let s = format!("{}-{}-{}-{}-{}", &hex[0..8], &hex[8..12], &hex[12..16], &hex[16..20], &hex[20..32]);
            args.extend([OsString::from("-U"), OsString::from(s)]);
        }
        if let Some(SwapLabel::Present(l)) = label {
            args.extend([OsString::from("-L"), os_bytes(l)]);
        }
        args.push(OsString::from(dev));
        let out = run_write("mkswap", &args)?;
        if !out.status.success() {
            return Err(FsError::command("mkswap", &out));
        }
        Ok(note)
    })
}

/// 用 div_euclid 而非 `/`：Rust 的整数除法对负数是向零取整，而这里要的是 floor
/// （如末尾行相对 256 KiB 对齐界的回退），两者在负值上不同
pub(crate) fn floor_to_unit(v: i64, unit: i64) -> i64 {
    v.div_euclid(unit) * unit
}

pub(crate) fn ceil_to_unit(v: i64, unit: i64) -> i64 {
    let f = floor_to_unit(v, unit);
    if f == v {
        f
    } else {
        f + unit
    }
}

/// mkfs 前残留签名擦除：分区内固定区间整段写零，不依赖旧 FS 识别。
///
/// 位置写死，不按任何文件系统的内部布局推算。bcachefs 两组区间的口径是
/// libblkid 的探测候选，不是 `bch_sb_layout`：`wipefs`/`blkid` 只读 4 KiB、
/// 2 MiB、末尾−1 MiB 三处（util-linux v2.42.4 `libblkid/src/superblocks/
/// bcache.c` 的 `bcachefs_idinfo`，查询日期 2026-11-10），不会顺着布局找副本。
///
/// 负偏移以分区**字节长度**（末字节 + 1）为基准；起点先按该行 rounding 下取整、
/// 再落到扇区界，末端上取整到扇区界，两端裁剪到分区界内，空区间丢弃、
/// 与已入表末项相同的相邻区间跳过。
pub(crate) fn erase_ranges(part_len: u64, ss: u64) -> Vec<(u64, u64)> {
    const K: u64 = 1024;
    const M: u64 = 1024 * K;
    const G: u64 = 1024 * M;
    // (offset, rounding, length)
    let raw: &[(i64, u64, u64)] = &[
        // 头部 512 KiB：覆盖 0 与 0x1000。bcachefs 主超级块在扇区 8（4 KiB），
        // libblkid 候选 kboff=4（util-linux v2.42.4 libblkid/src/superblocks/
        // bcache.c，查询 2026-11-10）；ZFS L0/L1 标签在 0 与 256 KiB
        // （ZFS On-Disk Specification, DRAFT 2005, Illustration 2）。
        (0, 1, 512 * K),
        // 2 MiB：只按 libblkid 的固定候选覆盖（kboff = 1 << 11）。该候选在
        // bcachefs 各版本布局里推不出来：默认 sb_size=2048 扇区时 sb[1] 在
        // 2056 扇区（1.003 MiB），sb_size=2 MiB 时在 4104 扇区（2.004 MiB）
        // （bcachefs-tools v1.39.7 src/wrappers/super_io.rs、v1.6.4
        // c_src/libbcachefs.c 的 init_layout，查询 2026-11-10）。
        (2 * M as i64, 1, 4 * K),
        // 64 MiB：btrfs 超级块镜像 0x4000000（btrfs On-disk Format，
        // btrfs.readthedocs.io，查询 2026-11-10）。
        (64 * M as i64, 1, 4 * K),
        // 256 GiB：btrfs 超级块镜像 0x4000000000（同上）。
        (256 * G as i64, 1, 4 * K),
        // 末尾−3087 扇区：Promise FastTrack RAID 元数据（util-linux v2.42.4
        // libblkid/src/superblocks/promise_raid.c 的 sectors[] 含 3087，
        // 查询 2026-11-10）；厂商规范未查到。
        (-(3087 * 512), 1, 512),
        // 末尾−1 MiB：libblkid 固定候选，探针读 L−1 MiB+24 的 16 字节 magic
        // （util-linux v2.42.4 bcache.c 的 kboff = -(1 << 10)、libblkid/src/
        // probe.c 的负 kboff 换算，查询 2026-11-10）。本行擦不到的副本有两处：
        // 末尾副本在 floor((L − sb_max_size)/桶)×桶，桶值或对齐不落到 L−1 MiB
        // 时擦不到；第二副本默认在 2056 扇区（约 1.003 MiB，init_layout 的
        // sb_pos = round_up(8 + sb_size, block_size >> 9)），本表没有任何一行
        // 覆盖它（头部行到 512 KiB 为止，2 MiB 行从 2 MiB 起）。
        // 两处仍可接受：头部行同时清掉 0..4 KiB 的布局扇区
        // （BCH_SB_LAYOUT_SECTOR=7），内核无从枚举副本；libblkid 只读
        // 4 KiB、2 MiB、L−1 MiB 三个候选，都不在这两处。
        (-(M as i64), 1, 4 * K),
        // 尾部：最后 512 KiB（起点按 256 KiB 下取整，最多向前 768 KiB）。
        // ZFS L2/L3 在 L−512 KiB / L−256 KiB（ZFS On-Disk Specification,
        // DRAFT 2005）；md 0.90 超级块落在末尾 128 KiB 内、1.0 在末尾
        // 8–12 KiB（man 4 md、kernel v6.12 drivers/md/md.c，查询 2026-11-10）；
        // nilfs2 第二超级块在 L−4 KiB（NILFS_SB2_OFFSET_BYTES）。
        (-(512 * K as i64), 256 * K, 768 * K),
    ];
    let ss_i = ss as i64;
    let part_end = part_len as i64; // 实际容量远小于 2^63，i64 足以容纳负的中间量
    let mut out: Vec<(u64, u64)> = Vec::new();
    for &(off, rounding, len) in raw {
        // 起点：负偏移从分区尾回退，先按该行 rounding 下取整，再落到扇区界；
        // 越界部分由下方裁剪兜住。末尾−1 MiB 行取 rounding=1，位置不随桶大小移动。
        let start = if off >= 0 {
            floor_to_unit(off, ss_i)
        } else {
            floor_to_unit(floor_to_unit(part_end + off, rounding as i64), ss_i)
        };
        // 末端上取整到整扇区（写入以扇区为单位；非正的末端经下方裁剪必然落空）
        let end = ceil_to_unit(start + len as i64, ss_i);
        let s = start.clamp(0, part_end) as u64;
        let e = end.clamp(0, part_end) as u64;
        if s < e && out.last() != Some(&(s, e - s)) {
            out.push((s, e - s));
        }
    }
    out
}

/// 对分区设备执行擦除：逐区间写零（4KiB 块），每步 sync
pub(crate) fn wipe_zero(dev: &str, ranges: &[(u64, u64)]) -> io::Result<()> {
    use std::io::{Seek, Write};
    let mut f = std::fs::OpenOptions::new().write(true).open(dev)?;
    let zero = [0u8; 4096];
    for &(off, len) in ranges {
        f.seek(std::io::SeekFrom::Start(off))?;
        let mut left = len;
        while left > 0 {
            let n = (left as usize).min(zero.len());
            crate::outcome::note_target_write();
            f.write_all(&zero[..n])?;
            left -= n as u64;
        }
        f.sync_data()?;
    }
    Ok(())
}