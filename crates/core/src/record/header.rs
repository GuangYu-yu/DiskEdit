//! 记录头：16 字节固定布局，两段 CRC 分别覆盖头与载荷。
//!
//! 字节序为小端。头 CRC 覆盖前 12 字节；载荷 CRC 覆盖整个载荷。
//! 头只负责"这段字节是不是一份完整记录"，不解释载荷内容。

use super::MAX_PAYLOAD_BYTES;
use crate::table::crc32;

/// 记录文件的格式标识。
///
/// 帧布局在写入后字节（postimage）加入时改变，按"布局或语义变化就换标识"的约定
/// 从 `DRB1` 升为 `DRB2`：旧记录会按外来格式识别，只允许 `recover --release`。
pub const MAGIC: [u8; 4] = *b"DRB2";

/// 记录头长度，字节。
pub const HEADER_LEN: usize = 16;

/// 头 CRC 覆盖的字节范围。
const HEADER_CRC_RANGE: std::ops::Range<usize> = 0..12;

/// 记录头字段。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RecordHeader {
    pub payload_len: u32,
    pub payload_crc: u32,
    pub header_crc: u32,
}

impl RecordHeader {
    /// 由载荷字节生成自洽的头。
    pub fn of_payload(payload: &[u8]) -> Option<Self> {
        let payload_len = u32::try_from(payload.len()).ok()?;
        if payload_len > MAX_PAYLOAD_BYTES {
            return None;
        }
        let mut bytes = [0u8; HEADER_LEN];
        bytes[..MAGIC.len()].copy_from_slice(&MAGIC);
        bytes[4..8].copy_from_slice(&payload_len.to_le_bytes());
        bytes[8..12].copy_from_slice(&crc32(payload).to_le_bytes());
        let header_crc = crc32(&bytes[HEADER_CRC_RANGE]);
        bytes[12..16].copy_from_slice(&header_crc.to_le_bytes());
        Some(Self {
            payload_len,
            payload_crc: crc32(payload),
            header_crc,
        })
    }

    /// 序列化为 16 字节。
    pub fn to_bytes(self) -> [u8; HEADER_LEN] {
        let mut bytes = [0u8; HEADER_LEN];
        bytes[..MAGIC.len()].copy_from_slice(&MAGIC);
        bytes[4..8].copy_from_slice(&self.payload_len.to_le_bytes());
        bytes[8..12].copy_from_slice(&self.payload_crc.to_le_bytes());
        bytes[12..16].copy_from_slice(&self.header_crc.to_le_bytes());
        bytes
    }

    /// 解析头；返回的 `header_crc` 与 `payload_len` 已通过自洽校验。
    pub fn parse(bytes: &[u8]) -> Result<Self, HeaderError> {
        if bytes.len() < HEADER_LEN {
            return Err(HeaderError::TooShort {
                needed: HEADER_LEN,
                actual: bytes.len(),
            });
        }
        let magic = [bytes[0], bytes[1], bytes[2], bytes[3]];
        if magic != MAGIC {
            return Err(HeaderError::ForeignFormat { magic });
        }
        let stored = u32::from_le_bytes([bytes[12], bytes[13], bytes[14], bytes[15]]);
        if crc32(&bytes[HEADER_CRC_RANGE]) != stored {
            return Err(HeaderError::HeaderCrcMismatch);
        }
        let payload_len = u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
        if payload_len > MAX_PAYLOAD_BYTES {
            return Err(HeaderError::PayloadTooLarge(payload_len));
        }
        Ok(Self {
            payload_len,
            payload_crc: u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]),
            header_crc: stored,
        })
    }
}

/// 头部解析失败的原因。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HeaderError {
    /// 字节数不足一个头。
    TooShort { needed: usize, actual: usize },
    /// 标识不是本实现的格式。
    ForeignFormat { magic: [u8; 4] },
    /// 头 CRC 与计算值不符。
    HeaderCrcMismatch,
    /// 声明的载荷长度超过实现上限。
    PayloadTooLarge(u32),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_keeps_all_fields() {
        let payload = b"payload bytes";
        let header = RecordHeader::of_payload(payload).expect("载荷应可编码");
        let parsed = RecordHeader::parse(&header.to_bytes()).expect("头应可解析");
        assert_eq!(parsed, header);
        assert_eq!(parsed.payload_len as usize, payload.len());
        assert_eq!(parsed.payload_crc, crc32(payload));
    }

    #[test]
    fn foreign_magic_is_rejected() {
        let mut bytes = RecordHeader::of_payload(b"x")
            .expect("载荷应可编码")
            .to_bytes();
        bytes[0] = b'X';
        assert!(matches!(
            RecordHeader::parse(&bytes),
            Err(HeaderError::ForeignFormat { .. })
        ));
    }

    #[test]
    fn header_crc_detects_any_header_change() {
        let mut bytes = RecordHeader::of_payload(b"x")
            .expect("载荷应可编码")
            .to_bytes();
        bytes[4] = bytes[4].wrapping_add(1);
        assert_eq!(
            RecordHeader::parse(&bytes),
            Err(HeaderError::HeaderCrcMismatch)
        );
    }

    #[test]
    fn short_header_is_rejected() {
        assert_eq!(
            RecordHeader::parse(&[0u8; 4]),
            Err(HeaderError::TooShort {
                needed: HEADER_LEN,
                actual: 4
            })
        );
    }
}
