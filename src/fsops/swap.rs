//! swap 重建与擦除：分区落位后的内容重建。

use super::*;

/// swap 重建：mkswap -U <uuid> [-L <label>]，保持 UUID/卷标以维持 fstab 兼容
/// （-U/-L 见 man mkswap：UUID 存取同序、无端转换，全零视为未设置走随机生成）。
/// 卷标按字节透传：sws_volume 是不做编码校验的固定宽度字段，转成 String 再写回必然失真。
/// 失败由调用方降级为日志（swap 内容可弃，但 fstab 指向的 UUID 需人工 mkswap 恢复）
pub fn recreate_swap(src: &FileSource, part: u32, identity: (Option<[u8; 16]>, Option<Vec<u8>>)) -> Result<(), FsError> {
    with_partition_device(src, part, |dev| {
        let mut args: Vec<OsString> = Vec::new();
        if let Some(u) = &identity.0 {
            // swap UUID 按原字节序列化为标准 8-4-4-4-12（mkswap 存取同序，无混合端转换）
            let hex: String = u.iter().map(|b| format!("{b:02x}")).collect();
            let s = format!("{}-{}-{}-{}-{}", &hex[0..8], &hex[8..12], &hex[12..16], &hex[16..20], &hex[20..32]);
            args.extend([OsString::from("-U"), OsString::from(s)]);
        }
        if let Some(l) = &identity.1 {
            args.extend([OsString::from("-L"), os_bytes(l)]);
        }
        args.push(OsString::from(dev));
        let out = run("mkswap", &args)?;
        if !out.status.success() {
            return Err(FsError::command("mkswap", &out));
        }
        Ok(())
    })
}

/// 下取整到 unit 的整数倍
///
/// 用 div_euclid 而非 `/`：Rust 的整数除法对负数是向零取整，而这里要的是 floor
/// （如 pos = part_len−1MiB 相对桶界的回退），两者在负值上不同
pub(crate) fn floor_to_unit(v: i64, unit: i64) -> i64 {
    v.div_euclid(unit) * unit
}

/// 上取整到 unit 的整数倍（取整方向说明见 floor_to_unit）
pub(crate) fn ceil_to_unit(v: i64, unit: i64) -> i64 {
    let f = floor_to_unit(v, unit);
    if f == v {
        f
    } else {
        f + unit
    }
}

/// mkfs 前残留签名擦除：分区内固定区间整段写零，一张表覆盖已知备份超级块，
/// 不依赖旧 FS 识别。区间表布局取自 GParted erase_filesystem_signatures()
/// （GParted_Core.cc:4108-4120），是该工具的自定策略而非 FS/磁盘规范。
/// offset 为负 = 距分区尾，rounding = 对齐粒度；起点下取整到扇区界、末端上取整到
/// 扇区界，两端裁剪到分区界内，空区间丢弃、与已入表末项相同的相邻区间跳过。
pub(crate) fn erase_ranges(part_len: u64, ss: u64) -> Vec<(u64, u64)> {
    const K: u64 = 1024;
    const M: u64 = 1024 * K;
    const G: u64 = 1024 * M;
    const P: u64 = 1024 * G;
    // (offset, rounding, length)
    let raw: &[(i64, u64, u64)] = &[
        (0, 1, 512 * K),           // 头部：ZFS L0/L1 及全部前部超级块
        (64 * M as i64, 1, 4 * K), // btrfs 镜像超级块
        (256 * G as i64, 1, 4 * K),
        (P as i64, 1, 4 * K),
        (-(3087 * 512), 1, 512), // Promise FastTrack RAID
        (-(M as i64), 128 * K, 4 * K), // bcachefs 备份超级块（4 种桶对齐）
        (-(M as i64), 256 * K, 4 * K),
        (-(M as i64), 512 * K, 4 * K),
        (-(M as i64), M, 4 * K),
        (-(512 * K as i64), 256 * K, 768 * K), // 尾部：ZFS L2/L3、md、ATARAID、nilfs2
    ];
    let ss_i = ss as i64;
    let part_end = part_len as i64; // 实际容量远小于 2^63，i64 足以容纳负的中间量
    let mut out: Vec<(u64, u64)> = Vec::new();
    for &(off, rounding, len) in raw {
        // 起点：负偏移从分区尾回退，先按该行 rounding 下取整（bcachefs 备份超级块在
        // -1MiB 的桶对齐处，须先取整才能命中），再落到扇区界；越界部分由下方裁剪兜住
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
            f.write_all(&zero[..n])?;
            left -= n as u64;
        }
        f.sync_data()?;
    }
    Ok(())
}