//! 记录帧：序号、类型、完成标记、不可逆标记、意图与区间原像。
//!
//! 每帧自带 CRC，因此即使载荷尾部被截断，也能确定哪些帧已经完整落盘。
//! 帧长度由字段自身推出：序号(4) | 类型(1) | 标记(1) | 区间数(2)
//! | 区间(偏移 8、长度 8、摘要 16、原像长度 4、原像 × n) | 意图长度(2) | 意图 | CRC(4)。
//!
//! 原像只在表区步骤出现，用于回滚；数据区步骤的原像长度为零、标记为不可逆，
//! 回滚时不动数据区、也不回收旧位置。

use crate::table::crc32;

/// 单条原像摘要的字节数。
pub const PREIMAGE_DIGEST_BYTES: usize = 16;
/// 单条区间记录的固定前缀：偏移、长度、原像摘要、原像长度。
pub const PREIMAGE_HEADER_BYTES: usize = 8 + 8 + PREIMAGE_DIGEST_BYTES + 4;
/// 帧固定前缀字节数（序号、类型、标记、区间数）。
const FRAME_PREFIX_BYTES: usize = 8;
/// 帧 CRC 字节数。
const FRAME_CRC_BYTES: usize = 4;
/// 标记位：该步的写入已全部落盘。
const FLAG_COMPLETED: u8 = 0b0000_0001;
/// 标记位：该步写入了不可回滚的数据区。
const FLAG_IRREVERSIBLE: u8 = 0b0000_0010;
/// 已定义的标记位集合。
const FLAG_MASK: u8 = FLAG_COMPLETED | FLAG_IRREVERSIBLE;

/// 计算一帧的标记字节。
fn flags_of(frame: &Frame) -> u8 {
    let mut flags = 0;
    if frame.completed {
        flags |= FLAG_COMPLETED;
    }
    if frame.irreversible {
        flags |= FLAG_IRREVERSIBLE;
    }
    flags
}

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

/// 一条将被覆盖区间的原像。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Preimage {
    pub offset: u64,
    pub len: u64,
    pub digest: [u8; PREIMAGE_DIGEST_BYTES],
    /// 表区的原始字节；数据区为空。
    pub data: Vec<u8>,
}

/// 一帧记录。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Frame {
    pub seq: u32,
    pub kind: FrameKind,
    /// 该步的写入已全部落盘。
    pub completed: bool,
    /// 该步写入了不可回滚的数据区；回滚只覆盖表区。
    pub irreversible: bool,
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
        out.push(flags_of(self));
        out.extend_from_slice(&region_count.to_le_bytes());
        for region in &self.regions {
            if !region.data.is_empty() && region.data.len() as u64 != region.len {
                return Err(FrameError::PreimageLengthMismatch);
            }
            let data_len =
                u32::try_from(region.data.len()).map_err(|_| FrameError::PreimageTooLarge)?;
            out.extend_from_slice(&region.offset.to_le_bytes());
            out.extend_from_slice(&region.len.to_le_bytes());
            out.extend_from_slice(&region.digest);
            out.extend_from_slice(&data_len.to_le_bytes());
            out.extend_from_slice(&region.data);
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
        if flags & !FLAG_MASK != 0 {
            return Err(FrameError::ReservedFlags(flags));
        }
        let region_count = u16::from_le_bytes([bytes[6], bytes[7]]) as usize;
        let mut regions = Vec::with_capacity(region_count);
        let mut cursor = FRAME_PREFIX_BYTES;
        for _ in 0..region_count {
            let header_end = cursor
                .checked_add(PREIMAGE_HEADER_BYTES)
                .ok_or(FrameError::Incomplete)?;
            if bytes.len() < header_end {
                return Err(FrameError::Incomplete);
            }
            let offset = read_u64(bytes, cursor);
            let len = read_u64(bytes, cursor + 8);
            let mut digest = [0u8; PREIMAGE_DIGEST_BYTES];
            digest.copy_from_slice(&bytes[cursor + 16..cursor + 16 + PREIMAGE_DIGEST_BYTES]);
            let data_len = u32::from_le_bytes([
                bytes[cursor + 32],
                bytes[cursor + 33],
                bytes[cursor + 34],
                bytes[cursor + 35],
            ]) as usize;
            let data_end = header_end
                .checked_add(data_len)
                .ok_or(FrameError::Incomplete)?;
            if data_end > bytes.len() || bytes.len() - data_end < 2 + FRAME_CRC_BYTES {
                return Err(FrameError::Incomplete);
            }
            regions.push(Preimage {
                offset,
                len,
                digest,
                data: bytes[header_end..data_end].to_vec(),
            });
            cursor = data_end;
        }
        let intent_at = cursor;
        if bytes.len() - intent_at < 2 + FRAME_CRC_BYTES {
            return Err(FrameError::Incomplete);
        }
        let intent_len = u16::from_le_bytes([bytes[intent_at], bytes[intent_at + 1]]) as usize;
        let total = intent_at + 2 + intent_len + FRAME_CRC_BYTES;
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
        let intent = bytes[intent_at + 2..intent_at + 2 + intent_len].to_vec();
        Ok((
            Self {
                seq,
                kind,
                completed: flags & FLAG_COMPLETED != 0,
                irreversible: flags & FLAG_IRREVERSIBLE != 0,
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
    /// 原像长度与区间长度不一致。
    PreimageLengthMismatch,
    /// 原像字节数超出 `u32`。
    PreimageTooLarge,
}

impl std::fmt::Display for FrameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Incomplete => f.write_str("frame is incomplete"),
            Self::CrcMismatch { seq } => write!(f, "frame {seq} CRC does not match"),
            Self::UnknownKind(kind) => write!(f, "frame kind {kind} is not known"),
            Self::ReservedFlags(flags) => {
                write!(f, "frame flags {flags:#04x} carry undefined bits")
            }
            Self::TooManyRegions => f.write_str("frame declares too many regions"),
            Self::IntentTooLong => f.write_str("frame intent text is too long"),
            Self::PreimageLengthMismatch => {
                f.write_str("frame preimage length does not match its region")
            }
            Self::PreimageTooLarge => f.write_str("frame preimage exceeds the size limit"),
        }
    }
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
            irreversible: false,
            intent: b"resize ext4".to_vec(),
            regions: vec![
                Preimage {
                    offset: 512,
                    len: 4096,
                    digest: [0x11; PREIMAGE_DIGEST_BYTES],
                    data: Vec::new(),
                },
                Preimage {
                    offset: 8192,
                    len: 512,
                    digest: [0x22; PREIMAGE_DIGEST_BYTES],
                    data: Vec::new(),
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
    fn table_preimage_bytes_survive_the_round_trip() {
        let mut original = frame(0, FrameKind::Preimage, false);
        original.regions[0].data = vec![0xab; 4096];
        let mut bytes = Vec::new();
        original.encode_into(&mut bytes).expect("编码应成立");
        let (parsed, _) = Frame::parse(&bytes).expect("解析应成立");
        assert_eq!(parsed.regions[0].data, vec![0xab; 4096]);
        assert!(parsed.regions[1].data.is_empty());
    }

    #[test]
    fn preimage_bytes_longer_than_the_region_are_rejected() {
        let mut original = frame(0, FrameKind::Preimage, false);
        original.regions[0].data = vec![0u8; 4097];
        let mut bytes = Vec::new();
        assert_eq!(
            original.encode_into(&mut bytes),
            Err(FrameError::PreimageLengthMismatch)
        );
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
        flags[5] = 0b100;
        assert_eq!(Frame::parse(&flags), Err(FrameError::ReservedFlags(4)));
    }

    #[test]
    fn irreversible_flag_survives_the_round_trip() {
        let mut original = frame(0, FrameKind::Preimage, false);
        original.irreversible = true;
        let mut bytes = Vec::new();
        original.encode_into(&mut bytes).expect("编码应成立");
        let (parsed, _) = Frame::parse(&bytes).expect("解析应成立");
        assert!(parsed.irreversible);
        assert!(!parsed.completed);
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
