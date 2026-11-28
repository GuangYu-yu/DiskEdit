//! 持久记录：16 字节头、帧编解码与纯前缀恢复。
//!
//! 记录只追加，帧序号连续。载荷固定前缀描述目标身份与计划摘要，随后是帧。
//! 恢复器不读目标、不看时钟：输入为记录字节与当前指纹，输出为确定的恢复结论。

pub mod codec;
pub mod frame;
pub mod header;
pub mod recover;

pub use codec::{DecodedRecord, RecordError, decode, encode};
pub use frame::{Frame, FrameKind, Preimage};
pub use header::{HEADER_LEN, MAGIC, RecordHeader};
pub use recover::{ManualReason, RecoveryExpectation, RecoveryPlan, recover};

/// 记录标识的字符数。
pub const ID_HEX_LEN: usize = 32;

/// 记录标识：计划摘要前 16 字节的小写十六进制。
///
/// 标识只用于把 `records list` 的输出与 `records verify` 的参数对上；它不参与
/// 恢复判定，也不承载目标的身份。同一目标上的同一份计划重放得到同一标识——
/// 重放不是新记录。
pub fn id_of(record: &DecodedRecord) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(ID_HEX_LEN);
    for byte in &record.body.plan_digest[..ID_HEX_LEN / 2] {
        out.push(DIGITS[usize::from(byte >> 4)] as char);
        out.push(DIGITS[usize::from(byte & 0x0f)] as char);
    }
    out
}

use crate::fingerprint::Fingerprint;

/// 记录载荷固定前缀的字节数：指纹、长度代次、计划摘要、前缀 CRC。
pub const BODY_LEN: usize = 32 + 8 + 32 + 4;

/// 载荷声明长度上限，字节。
pub const MAX_PAYLOAD_BYTES: u32 = 16 * 1024 * 1024;

/// 载荷固定前缀：这份记录属于哪个目标、对应哪一份计划。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RecordBody {
    pub target: Fingerprint,
    /// 长度代次：目标总字节数经 [`length_generation`] 展开的值。零表示记录不带
    /// 长度证据，读到即按需要人工处理。
    pub length_generation: u64,
    pub plan_digest: [u8; 32],
}

/// 目标总字节数的长度代次。
///
/// 固定的 FNV-1a 把总字节数展开成 `u64` 后置最低位：结果恒为奇数，零保留给不
/// 携带长度证据的记录。写入端存这个值，恢复端按当前目标长度重算并要求相等；
/// 长度变了代次就变。它只覆盖容量：字节偏移的可用范围由长度决定，扇区尺寸不
/// 参与比较。
pub fn length_generation(total_bytes: u64) -> u64 {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = OFFSET;
    for byte in total_bytes.to_le_bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(PRIME);
    }
    hash | 1
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(plan_digest: [u8; 32]) -> DecodedRecord {
        DecodedRecord {
            body: RecordBody {
                target: Fingerprint::strong([0x11; 32]),
                length_generation: 0,
                plan_digest,
            },
            frames: Vec::new(),
        }
    }

    #[test]
    fn id_is_the_lowercase_hex_of_the_first_sixteen_digest_bytes() {
        let mut digest = [0u8; 32];
        digest[..16].copy_from_slice(&[
            0x00, 0x0f, 0xa5, 0xff, 0x10, 0x20, 0x30, 0x40, 0x50, 0x60, 0x70, 0x80, 0x90, 0xa0,
            0xb0, 0xc0,
        ]);
        assert_eq!(
            id_of(&record(digest)),
            "000fa5ff102030405060708090a0b0c0".to_owned()
        );
    }

    #[test]
    fn id_ignores_the_tail_of_the_digest() {
        let mut left = [0u8; 32];
        let mut right = [0u8; 32];
        right[16..].fill(0xff);
        assert_eq!(id_of(&record(left)), id_of(&record(right)));
        left[0] = 1;
        assert_ne!(id_of(&record(left)), id_of(&record(right)));
    }
}
