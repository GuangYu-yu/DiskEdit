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
pub use recover::{RecoveryPlan, recover};

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

/// 记录载荷固定前缀的字节数：指纹、几何代次、计划摘要、前缀 CRC。
pub const BODY_LEN: usize = 32 + 8 + 32 + 4;

/// 载荷声明长度上限，字节。
pub const MAX_PAYLOAD_BYTES: u32 = 16 * 1024 * 1024;

/// 载荷固定前缀：这份记录属于谁、对应哪一版几何与哪一份计划。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RecordBody {
    pub target: Fingerprint,
    pub geometry_generation: u64,
    pub plan_digest: [u8; 32],
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(plan_digest: [u8; 32]) -> DecodedRecord {
        DecodedRecord {
            body: RecordBody {
                target: Fingerprint::strong([0x11; 32]),
                geometry_generation: 1,
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
