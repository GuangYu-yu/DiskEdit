//! 分区几何里与搬移共用的判定：缩容相位、FS 动作与 NTFS 隐藏扇区。
//!
//! 执行不属于这里：`resize-part` 的表与 FS 步骤都经 `assembly` 装配、`execution` 执行，
//! 本模块只保留四面共用的判定与那两个纯函数。

use super::*;

/// 缩容状态。三态而非 bool：`Unverified` 表示缩容命令已返回、落点却
/// 回读不出来——搬数据按它继续就可能切进未缩的元数据，故不能与 `Verified` 等同
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub(crate) enum FsShrink {
    NotShrunk = 0,
    Verified = 1,
    Unverified = 2,
}

impl FsShrink {
    fn from_code(code: u8) -> io::Result<Self> {
        match code {
            0 => Ok(Self::NotShrunk),
            1 => Ok(Self::Verified),
            2 => Ok(Self::Unverified),
            _ => Err(io::Error::new(io::ErrorKind::InvalidData, "implausible resize record shrink state")),
        }
    }
}

/// `progress.fs_phase` 与 [`FsShrink`] 同序：记录里只放这一个字节。
/// 认不出的码按"未缩"处理——重做一次缩容是幂等的，误当作已缩才会切进未缩的元数据
pub(crate) fn fs_shrunk_from_phase(code: u8) -> FsShrink {
    FsShrink::from_code(code).unwrap_or(FsShrink::NotShrunk)
}

/// 本次对 FS 的动作。prepare 算一次，execute 只 match——若两处各自从尺寸重新推导，
/// "要不要缩/扩"就有了两个来源（尺寸没变、缩容已完成、`--no-fs` 都落进 `Leave`）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FsAction {
    /// 不碰 FS
    Leave,
    Shrink,
    Grow,
}

pub(crate) fn ntfs_hidden_value(start_lba: u64) -> (u32, bool) {
    if start_lba >> 32 == 0 { (start_lba as u32, false) } else { (0u32, true) }
}

/// NTFS boot sector BPB HiddenSectors（偏移 0x1C，u32 LE，值 = 分区起始 LBA；
/// NTFS 引导扇区布局，ntfs-3g bootsect.c 与微软 NTFS 规范同此定义）。
/// 单位是设备逻辑扇区（4Kn 原生盘上即 4Kn LBA，不折算 512）：bytes_per_sector 记录
/// 各自的扇区大小，hidden_sectors 语义随它计。字段只有 32 位：start ≥ 2^32 时写 0 并
/// 警告（Windows 将无法从该分区引导，数据不受影响）。仅在分区起始 LBA 变化后调用。
pub(crate) fn fix_ntfs_hidden_sectors(src: &mut WriteSource, new_first_lba: u64, ss: u64, log: &mut dyn FnMut(&str)) -> io::Result<()> {
    let (val, warn) = ntfs_hidden_value(new_first_lba);
    // 与执行器的 LBA→字节换算同口径：起点来自调用方传入的几何，裸乘回绕会把
    // HiddenSectors 写到盘上另一个位置
    let sector_off = new_first_lba
        .checked_mul(ss)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("NTFS HiddenSectors offset overflows ({new_first_lba} × {ss})"),
            )
        })?;
    let off = sector_off.checked_add(0x1C).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("NTFS HiddenSectors offset overflows ({new_first_lba} × {ss} + 0x1C)"),
        )
    })?;
    // 写 4 字节前复核目标扇区仍是 NTFS 引导扇区：调用点的 identify 与这里的写入
    // 之间盘面可能变化。签名不符即拒绝——把 HiddenSectors 写进
    // 非引导扇区比不写更糟
    let mut probe = vec![0u8; ss as usize];
    src.read_at(sector_off, &mut probe)?;
    if probe.len() < 11 || probe[3..11] != *b"NTFS    " {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("the first sector at LBA {new_first_lba} is not an NTFS boot sector; refusing to write HiddenSectors"),
        ));
    }
    src.write_at(off, &val.to_le_bytes())?;
    src.sync_data()?;
    if warn {
        log("warning: partition starts beyond 2^32; HiddenSectors zeroed — Windows cannot boot from it");
    }
    Ok(())
}
