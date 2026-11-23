//! 记录帧：序号、类型、完成标记、意图与区间原像摘要。
//!
//! 每帧自带 CRC，因此即使载荷尾部被截断，也能确定哪些帧已经完整落盘。
//! 帧长度由字段自身推出，不额外存长度：序号(4) | 类型(1) | 标记(1) | 区间数(2)
//! | 区间(32×n) | 意图长度(2) | 意图 | CRC(4)。

use crate::table::crc32;

/// 单条原像摘要的字节数。
pub const PREIMAGE_DIGEST_BYTES: usize = 16;
/// 单条区间记录的字节数：偏移、长度、原像摘要。
pub const PREIMAGE_BYTES: usize = 8 + 8 + PREIMAGE_DIGEST_BYTES;
/// 帧固定前缀字节数（序号、类型、标记、区间数）。
const FRAME_PREFIX_BYTES: usize = 8;
/// 帧 CRC 字节数。
const FRAME_CRC_BYTES: usize = 4;

/// 帧类型。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameKind {
    /// 声明本步意图。
    Intent,
    /// 登记本步将覆盖区间的原像摘要。
    Preimage,
    /// 推进进度。
    Progress,
    /// 事务已提交。
    Committed,
    /// 事务已放弃。
    Aborted,
}

impl FrameKind {
    pub const fn as_byte(self) -> u8 {
        match self {
            Self::Intent => 0,
            Self::Preimage => 1,
            Self::Progress => 2,
            Self::Committed => 3,
            Self::Aborted => 4,
        }
    }

    pub const fn from_byte(byte: u8) -> Option<Self> {
        match byte {
            0 => Some(Self::Intent),
            1 => Some(Self::Preimage),
            2 => Some(Self::Progress),
            3 => Some(Self::Committed),
            4 => Some(Self::Aborted),
            _ => None,
        }
    }

    /// 事务终态帧。
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Committed | Self::Aborted)
    }
}

/// 一条将被覆盖区间的原像摘要。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Preimage {
    pub offset: u64,
    pub len: u64,
    pub digest: [u8; PREIMAGE_DIGEST_BYTES],
}

/// 一帧记录。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Frame {
    pub seq: u32,
    pub kind: FrameKind,
    /// 该步的写入已全部落盘。
    pub completed: bool,
    pub intent: Vec<u8>,
    pub regions: Vec<Preimage>,
}

impl Frame {
    /// 把本帧追加到 `out`。
    pub fn encode_into(&self, out: &mut Vec<u8>) -> Result<(), FrameError> {
        let region_count =
            u16::try_from(self.regions.len()).map_err(|_| FrameError::TooManyRegions)?;
        let intent_len = u16::try_from(self.intent.len()).map_err(|_| FrameError::IntentTooLong)?;
        let start = out.len();
        out.extend_from_slice(&self.seq.to_le_bytes());
        out.push(self.kind.as_byte());
        out.push(u8::from(self.completed));
        out.extend_from_slice(&region_count.to_le_bytes());
        for region in &self.regions {
            out.extend_from_slice(&region.offset.to_le_bytes());
            out.extend_from_slice(&region.len.to_le_bytes());
            out.extend_from_slice(&region.digest);
        }
        out.extend_from_slice(&intent_len.to_le_bytes());
        out.extend_from_slice(&self.intent);
        let crc = crc32(&out[start..]);
        out.extend_from_slice(&crc.to_le_bytes());
        Ok(())
    }

    /// 从 `bytes` 开头解析一帧，返回帧与消耗的字节数。
    pub fn parse(bytes: &[u8]) -> Result<(Self, usize), FrameError> {
        if bytes.len() < FRAME_PREFIX_BYTES {
            return Err(FrameError::Incomplete);
        }
        let seq = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        let kind = FrameKind::from_byte(bytes[4]).ok_or(FrameError::UnknownKind(bytes[4]))?;
        let flags = bytes[5];
        if flags & !1 != 0 {
            return Err(FrameError::ReservedFlags(flags));
        }
        let region_count = u16::from_le_bytes([bytes[6], bytes[7]]) as usize;
        let regions_end = FRAME_PREFIX_BYTES
            .checked_add(
                region_count
                    .checked_mul(PREIMAGE_BYTES)
                    .ok_or(FrameError::Incomplete)?,
            )
            .ok_or(FrameError::Incomplete)?;
        if bytes.len() < regions_end + 2 + FRAME_CRC_BYTES {
            return Err(FrameError::Incomplete);
        }
        let intent_len = u16::from_le_bytes([bytes[regions_end], bytes[regions_end + 1]]) as usize;
        let total = regions_end + 2 + intent_len + FRAME_CRC_BYTES;
        if bytes.len() < total {
            return Err(FrameError::Incomplete);
        }
        let stored = u32::from_le_bytes([
            bytes[total - 4],
            bytes[total - 3],
            bytes[total - 2],
            bytes[total - 1],
        ]);
        if crc32(&bytes[..total - FRAME_CRC_BYTES]) != stored {
            return Err(FrameError::CrcMismatch { seq });
        }
        let mut regions = Vec::with_capacity(region_count);
        for index in 0..region_count {
            let base = FRAME_PREFIX_BYTES + index * PREIMAGE_BYTES;
            regions.push(Preimage {
                offset: read_u64(bytes, base),
                len: read_u64(bytes, base + 8),
                digest: {
                    let mut digest = [0u8; PREIMAGE_DIGEST_BYTES];
                    digest.copy_from_slice(&bytes[base + 16..base + PREIMAGE_BYTES]);
                    digest
                },
            });
        }
        let intent = bytes[regions_end + 2..regions_end + 2 + intent_len].to_vec();
        Ok((
            Self {
                seq,
                kind,
                completed: flags & 1 == 1,
                intent,
                regions,
            },
            total,
        ))
    }
}

/// 单帧编解码失败的原因。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameError {
    /// 字节数不足一帧。
    Incomplete,
    /// 序号与 CRC 不符。
    CrcMismatch { seq: u32 },
    /// 类型字节不属于本实现。
    UnknownKind(u8),
    /// 标记位含有未定义位。
    ReservedFlags(u8),
    /// 区间数超出 `u16`。
    TooManyRegions,
    /// 意图长度超出 `u16`。
    IntentTooLong,
}

fn read_u64(bytes: &[u8], offset: usize) -> u64 {
    let mut raw = [0u8; 8];
    raw.copy_from_slice(&bytes[offset..offset + 8]);
    u64::from_le_bytes(raw)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(seq: u32, kind: FrameKind, completed: bool) -> Frame {
        Frame {
            seq,
            kind,
            completed,
            intent: b"resize ext4".to_vec(),
            regions: vec![
                Preimage {
                    offset: 512,
                    len: 4096,
                    digest: [0x11; PREIMAGE_DIGEST_BYTES],
                },
                Preimage {
                    offset: 8192,
                    len: 512,
                    digest: [0x22; PREIMAGE_DIGEST_BYTES],
                },
            ],
        }
    }

    #[test]
    fn round_trip_preserves_every_field() {
        let original = frame(3, FrameKind::Preimage, true);
        let mut bytes = Vec::new();
        original.encode_into(&mut bytes).expect("编码应成立");
        let (parsed, consumed) = Frame::parse(&bytes).expect("解析应成立");
        assert_eq!(parsed, original);
        assert_eq!(consumed, bytes.len());
    }

    #[test]
    fn truncated_prefix_is_incomplete_but_never_misparsed() {
        let original = frame(0, FrameKind::Intent, false);
        let mut bytes = Vec::new();
        original.encode_into(&mut bytes).expect("编码应成立");
        for cut in 0..bytes.len() {
            assert_eq!(Frame::parse(&bytes[..cut]), Err(FrameError::Incomplete));
        }
    }

    #[test]
    fn crc_detects_any_field_change() {
        let original = frame(0, FrameKind::Progress, false);
        let mut bytes = Vec::new();
        original.encode_into(&mut bytes).expect("编码应成立");
        let last = bytes.len() - 6;
        bytes[last] ^= 0xff;
        assert!(matches!(
            Frame::parse(&bytes),
            Err(FrameError::CrcMismatch { .. })
        ));
    }

    #[test]
    fn unknown_kind_and_reserved_flags_are_rejected() {
        let mut bytes = Vec::new();
        frame(0, FrameKind::Intent, false)
            .encode_into(&mut bytes)
            .expect("编码应成立");
        let mut kind = bytes.clone();
        kind[4] = 9;
        assert_eq!(Frame::parse(&kind), Err(FrameError::UnknownKind(9)));

        let mut flags = bytes;
        flags[5] = 0b10;
        assert_eq!(Frame::parse(&flags), Err(FrameError::ReservedFlags(2)));
    }

    #[test]
    fn terminal_kinds_are_identified() {
        assert!(FrameKind::Committed.is_terminal());
        assert!(FrameKind::Aborted.is_terminal());
        assert!(!FrameKind::Intent.is_terminal());
        assert!(!FrameKind::Preimage.is_terminal());
        assert!(!FrameKind::Progress.is_terminal());
    }
}
