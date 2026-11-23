//! 记录的整体编解码：头、载荷前缀与帧序列。
//!
//! 严格解码要求载荷长度与 CRC 全部成立、帧序列完整且序号连续；前缀恢复见
//! [`super::recover`]。编码顺序固定，任一字段变化都会改变长度或校验值。

use super::frame::{Frame, FrameError};
use super::header::{HEADER_LEN, HeaderError, RecordHeader};
use super::{BODY_LEN, RecordBody};
use crate::fingerprint::Fingerprint;
use crate::table::crc32;

/// 一份完整解码的记录。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DecodedRecord {
    pub body: RecordBody,
    pub frames: Vec<Frame>,
}

/// 记录编解码失败的原因。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecordError {
    /// 字节数不足。
    TooShort { needed: usize, actual: usize },
    /// 标识不是本实现的格式。
    ForeignFormat { magic: [u8; 4] },
    /// 头 CRC 与计算值不符。
    HeaderCrcMismatch,
    /// 载荷 CRC 与计算值不符。
    PayloadCrcMismatch,
    /// 载荷声明长度超过实现上限。
    PayloadTooLarge(u32),
    /// 载荷前缀自身的 CRC 不符。
    BodyCrcMismatch,
    /// 载荷之后还有多余字节。
    TrailingBytes,
    /// 帧字节不足。
    FrameIncomplete,
    /// 帧 CRC 不符。
    FrameCrcMismatch { seq: u32 },
    /// 帧序号不连续。
    FrameSequence { expected: u32, actual: u32 },
    /// 帧类型不属于本实现。
    UnknownFrameKind(u8),
    /// 帧标记位含有未定义位。
    ReservedFrameFlags(u8),
    /// 终态帧之后仍有帧。
    FramesAfterTerminal,
    /// 意图长度超出 `u16`。
    IntentTooLong,
    /// 区间数超出 `u16`。
    TooManyRegions,
}

impl From<HeaderError> for RecordError {
    fn from(error: HeaderError) -> Self {
        match error {
            HeaderError::TooShort { needed, actual } => Self::TooShort { needed, actual },
            HeaderError::ForeignFormat { magic } => Self::ForeignFormat { magic },
            HeaderError::HeaderCrcMismatch => Self::HeaderCrcMismatch,
            HeaderError::PayloadTooLarge(len) => Self::PayloadTooLarge(len),
        }
    }
}

impl From<FrameError> for RecordError {
    fn from(error: FrameError) -> Self {
        match error {
            FrameError::Incomplete => Self::FrameIncomplete,
            FrameError::CrcMismatch { seq } => Self::FrameCrcMismatch { seq },
            FrameError::UnknownKind(byte) => Self::UnknownFrameKind(byte),
            FrameError::ReservedFlags(flags) => Self::ReservedFrameFlags(flags),
            FrameError::TooManyRegions => Self::TooManyRegions,
            FrameError::IntentTooLong => Self::IntentTooLong,
        }
    }
}

/// 编码一份记录。载荷超过上限或帧字段越界时返回错误。
pub fn encode(body: &RecordBody, frames: &[Frame]) -> Result<Vec<u8>, RecordError> {
    let mut payload = Vec::new();
    encode_body(body, &mut payload);
    for frame in frames {
        frame.encode_into(&mut payload)?;
    }
    let header =
        RecordHeader::of_payload(&payload).ok_or(RecordError::PayloadTooLarge(u32::MAX))?;
    let mut out = Vec::with_capacity(HEADER_LEN + payload.len());
    out.extend_from_slice(&header.to_bytes());
    out.extend_from_slice(&payload);
    Ok(out)
}

/// 严格解码一份记录：长度、两段 CRC、前缀 CRC、帧序列全部成立才返回。
pub fn decode(bytes: &[u8]) -> Result<DecodedRecord, RecordError> {
    let header = RecordHeader::parse(bytes)?;
    let payload = &bytes[HEADER_LEN..];
    if payload.len() < header.payload_len as usize {
        return Err(RecordError::TooShort {
            needed: HEADER_LEN + header.payload_len as usize,
            actual: bytes.len(),
        });
    }
    if payload.len() > header.payload_len as usize {
        return Err(RecordError::TrailingBytes);
    }
    if crc32(payload) != header.payload_crc {
        return Err(RecordError::PayloadCrcMismatch);
    }
    let body = decode_body(payload)?;
    let mut frames = Vec::new();
    let mut offset = BODY_LEN;
    while offset < payload.len() {
        let (frame, used) = Frame::parse(&payload[offset..])?;
        check_sequence(&frames, &frame)?;
        if !frames.is_empty() && frames.last().is_some_and(|prev| prev.kind.is_terminal()) {
            return Err(RecordError::FramesAfterTerminal);
        }
        frames.push(frame);
        offset += used;
    }
    Ok(DecodedRecord { body, frames })
}

/// 写出载荷固定前缀。
pub fn encode_body(body: &RecordBody, out: &mut Vec<u8>) {
    let start = out.len();
    out.extend_from_slice(&body.target.bytes());
    out.extend_from_slice(&body.geometry_generation.to_le_bytes());
    out.extend_from_slice(&body.plan_digest);
    let crc = crc32(&out[start..]);
    out.extend_from_slice(&crc.to_le_bytes());
}

/// 解析载荷固定前缀并校验其 CRC。
pub fn decode_body(payload: &[u8]) -> Result<RecordBody, RecordError> {
    if payload.len() < BODY_LEN {
        return Err(RecordError::TooShort {
            needed: BODY_LEN,
            actual: payload.len(),
        });
    }
    let crc_at = BODY_LEN - 4;
    let stored = u32::from_le_bytes([
        payload[crc_at],
        payload[crc_at + 1],
        payload[crc_at + 2],
        payload[crc_at + 3],
    ]);
    if crc32(&payload[..crc_at]) != stored {
        return Err(RecordError::BodyCrcMismatch);
    }
    let mut fingerprint = [0u8; 32];
    fingerprint.copy_from_slice(&payload[..32]);
    let mut plan_digest = [0u8; 32];
    plan_digest.copy_from_slice(&payload[40..72]);
    Ok(RecordBody {
        target: Fingerprint::strong(fingerprint),
        geometry_generation: u64::from_le_bytes([
            payload[32],
            payload[33],
            payload[34],
            payload[35],
            payload[36],
            payload[37],
            payload[38],
            payload[39],
        ]),
        plan_digest,
    })
}

/// 帧序号必须从 0 起连续。
pub fn check_sequence(frames: &[Frame], frame: &Frame) -> Result<(), RecordError> {
    let expected = u32::try_from(frames.len()).unwrap_or(u32::MAX);
    if frame.seq != expected {
        return Err(RecordError::FrameSequence {
            expected,
            actual: frame.seq,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::frame::{FrameKind, Preimage};

    fn body() -> RecordBody {
        RecordBody {
            target: Fingerprint::strong([0x42; 32]),
            geometry_generation: 9,
            plan_digest: [0x7e; 32],
        }
    }

    fn frame(seq: u32) -> Frame {
        Frame {
            seq,
            kind: if seq == 2 {
                FrameKind::Committed
            } else {
                FrameKind::Preimage
            },
            completed: seq < 2,
            intent: b"step".to_vec(),
            regions: vec![Preimage {
                offset: 1024,
                len: 512,
                digest: [0x33; 16],
            }],
        }
    }

    #[test]
    fn round_trip_is_exact() {
        let frames = vec![frame(0), frame(1), frame(2)];
        let bytes = encode(&body(), &frames).expect("编码应成立");
        let decoded = decode(&bytes).expect("解码应成立");
        assert_eq!(decoded.body, body());
        assert_eq!(decoded.frames, frames);
    }

    #[test]
    fn foreign_format_is_rejected() {
        let mut bytes = encode(&body(), &[]).expect("编码应成立");
        bytes[3] = b'X';
        assert!(matches!(
            decode(&bytes),
            Err(RecordError::ForeignFormat { .. })
        ));
    }

    #[test]
    fn payload_crc_detects_payload_change() {
        let mut bytes = encode(&body(), &[frame(0)]).expect("编码应成立");
        let last = bytes.len() - 1;
        bytes[last] ^= 0xff;
        assert_eq!(decode(&bytes), Err(RecordError::PayloadCrcMismatch));
    }

    #[test]
    fn body_crc_is_independent_of_the_whole_payload_crc() {
        let mut payload = Vec::new();
        encode_body(&body(), &mut payload);
        payload[0] ^= 0xff;
        assert_eq!(decode_body(&payload), Err(RecordError::BodyCrcMismatch));
    }

    #[test]
    fn frame_sequence_must_be_contiguous() {
        let bytes = encode(&body(), &[frame(1)]).expect("编码应成立");
        assert_eq!(
            decode(&bytes),
            Err(RecordError::FrameSequence {
                expected: 0,
                actual: 1
            })
        );
    }

    #[test]
    fn frames_after_a_terminal_frame_are_rejected() {
        let bytes = encode(&body(), &[frame(0), frame(1), frame(2), frame(3)]).expect("编码应成立");
        assert_eq!(decode(&bytes), Err(RecordError::FramesAfterTerminal));
    }

    #[test]
    fn trailing_bytes_are_rejected() {
        let mut bytes = encode(&body(), &[]).expect("编码应成立");
        bytes.push(0);
        assert_eq!(decode(&bytes), Err(RecordError::TrailingBytes));
    }
}
