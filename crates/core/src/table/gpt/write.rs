//! GPT 序列化辅助：条目数组 CRC。

use super::super::crc32;

/// 条目数组的 CRC，只覆盖逻辑载荷。
pub fn array_crc(raw: &[u8]) -> u32 {
    crc32(raw)
}
