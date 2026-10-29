//! GPT 条目数组几何：条目大小/数量与数组字节上限。

use super::*;

// ---------- 条目数组几何（canonical，codec 层自持） ----------
//
// "数组有多大"是编解码/布局事实（UEFI 2.10 §5.3.3 Table 5.6：NumberOfPartitionEntries ×
// SizeOfPartitionEntry 决定，不是固定 128 条目），住在本层；操作几何
// （geometry::ValidatedGeometry）只消费它，不让本层反向依赖几何层

/// 自定安全上限（**策略参数，非 UEFI 规范要求**）：条目数组字节数不得超过此值。
/// 刻意不进 `EntryArrayGeometry::new` 的规范判据——由构造点显式传入：解析分配防线与
/// 操作几何共用同一个常量，同一份策略不留第二个副本
pub const MAX_ARRAY_BYTES: u64 = 16 * 1024 * 1024;

/// `new` 造新表时写入的条目数与单条目字节数：建表策略，不是从盘上推导的事实。
/// 新建 GPT 的这两项由实现自选（各工具默认值不同），本工具取 128 × 128B；
/// 改它只影响新造的表，不影响对已有表的解读
pub const DEFAULT_ENTRY_COUNT: u32 = 128;

pub const DEFAULT_ENTRY_SIZE: u32 = 128;

/// 单条目字节数合规判据：UEFI 2.10 §5.3.3 Table 5.6 规定 SizeOfPartitionEntry = 128 × 2^n，
/// 前 128 字节为规范定义字段，其余为保留区（必须为零）
pub fn entry_size_ok(entry_size: u32) -> bool {
    entry_size >= 128 && entry_size.is_power_of_two()
}

pub(crate) fn invalid_geom(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.into())
}

/// 条目数组的几何：`sector_size` 是**该表自身**的扇区大小（条目 LBA 的单位），
/// 与容器扇区大小可以不同（4Kn 表放在 512e 容器里），两者不可混用
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct EntryArrayGeometry {
    pub sector_size: u64,
    pub entry_size: u32,
    pub entry_count: u32,
}

impl EntryArrayGeometry {
    /// 唯一构造点：规范判据（扇区大小不低于 512、条目数非零且单条目合规）在此表达；
    /// `max_array_bytes` 是调用方的策略输入（见 [`MAX_ARRAY_BYTES`]），不是本类型的
    /// 规范约束。解析层与写入层共用它，于是"表允许多大"只有一个答案
    pub fn new(sector_size: u64, entry_size: u32, entry_count: u32, max_array_bytes: u64) -> io::Result<Self> {
        // 512 是保护 MBR 记录（446..512）能被索引的下界，也是 UEFI 逻辑块的下界：
        // 更小的几何会让 ensure_protective_mbr 在写缓冲时越界
        if sector_size < 512 {
            return Err(invalid_geom(format!(
                "GPT entry geometry: sector size {sector_size} is below the 512-byte minimum"
            )));
        }
        if entry_count == 0 || !entry_size_ok(entry_size) {
            return Err(invalid_geom("implausible GPT entry geometry"));
        }
        let byte_len = entry_count as u64 * entry_size as u64;
        if byte_len > max_array_bytes {
            return Err(invalid_geom(format!(
                "implausible GPT entry geometry: {entry_count} × {entry_size} B = {byte_len} bytes exceeds the {max_array_bytes}-byte safety limit"
            )));
        }
        Ok(Self { sector_size, entry_size, entry_count })
    }

    /// 数组占用的字节数（UEFI 2.10 §5.3.3 Table 5.6：由两个自述字段相乘决定）
    pub fn byte_len(&self) -> u64 {
        self.entry_count as u64 * self.entry_size as u64
    }

    /// 数组占用的扇区数（向上取整到整扇区）
    pub fn lba_span(&self) -> u64 {
        self.byte_len().div_ceil(self.sector_size)
    }

    /// 1-based 分区号 → 数组下标。越界返回 None：**禁止**用字面量上限直接索引
    pub fn slot(&self, part: u32) -> Option<usize> {
        let idx = part.checked_sub(1)? as usize;
        (idx < self.entry_count as usize).then_some(idx)
    }
}