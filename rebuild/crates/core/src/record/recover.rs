//! 纯前缀恢复：任意完整帧前缀都给出确定结论。
//!
//! 写目标字节前，该步的原像帧必须已完整落盘并携带 CRC；因此序列在第一个
//! 不完整或 CRC 不符的帧之前停止是安全的：该帧对应的写入尚未发生。
//! 恢复器不看时钟、不读目标，结果只由记录字节与当前指纹决定。

use super::BODY_LEN;
use super::codec::decode_body;
use super::frame::{Frame, FrameError, FrameKind};
use super::header::{HEADER_LEN, HeaderError, RecordHeader};
use crate::fingerprint::Fingerprint;

/// 恢复结论。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RecoveryPlan {
    /// 目标上没有本实现的记录。
    Absent,
    /// 记录完整且事务已提交；只需清理记录。
    Committed,
    /// 完整前缀要求写回原像。
    RollBack { frames: Vec<Frame> },
    /// 最后一个完整帧已落盘但事务未收尾，从该处继续。
    Replay { frames: Vec<Frame> },
    /// 记录不是本实现写下的，拒绝且不读后续帧。
    Foreign { magic: [u8; 4] },
    /// 结构损坏或证据冲突，需要人工。
    NeedsManual { why: String },
}

/// 由记录字节与当前目标指纹推出恢复结论。
pub fn recover(bytes: &[u8], current: Fingerprint) -> RecoveryPlan {
    if bytes.is_empty() {
        return RecoveryPlan::Absent;
    }
    let header = match RecordHeader::parse(bytes) {
        Ok(header) => header,
        Err(HeaderError::ForeignFormat { magic }) => return RecoveryPlan::Foreign { magic },
        Err(HeaderError::HeaderCrcMismatch) => {
            return needs_manual("record header CRC mismatch");
        }
        Err(HeaderError::TooShort { needed, actual }) => {
            return needs_manual(format!(
                "record is shorter than its header: need {needed} bytes, got {actual}"
            ));
        }
        Err(HeaderError::PayloadTooLarge(len)) => {
            return needs_manual(format!("declared payload length {len} is too large"));
        }
    };
    let available = bytes.len() - HEADER_LEN;
    let declared = header.payload_len as usize;
    // 头声明的长度是最后一次成功落盘的载荷；其后多出的字节是尚未提交的追加，
    // 按不完整帧处理，不据此判定记录损坏。
    let usable = available.min(declared);
    let payload = &bytes[HEADER_LEN..HEADER_LEN + usable];
    if payload.len() < BODY_LEN {
        // 前缀未写全：没有任何帧完整落盘，因此没有目标写入发生。
        return RecoveryPlan::RollBack { frames: Vec::new() };
    }
    let body = match decode_body(payload) {
        Ok(body) => body,
        Err(_) => return needs_manual("record payload prefix CRC mismatch"),
    };
    if !body.target.matches(current) {
        return needs_manual("record targets a different device");
    }
    if available >= declared && crate::table::crc32(payload) != header.payload_crc {
        return needs_manual("record payload CRC mismatch");
    }
    let frames = match parse_prefix(payload) {
        Ok(frames) => frames,
        Err(why) => return RecoveryPlan::NeedsManual { why },
    };
    match frames.last() {
        None => RecoveryPlan::RollBack { frames },
        Some(frame) if frame.kind == FrameKind::Committed => RecoveryPlan::Committed,
        Some(frame) if frame.kind == FrameKind::Aborted => RecoveryPlan::RollBack { frames },
        Some(frame) if frame.completed => RecoveryPlan::Replay { frames },
        Some(_) => RecoveryPlan::RollBack { frames },
    }
}

fn parse_prefix(payload: &[u8]) -> Result<Vec<Frame>, String> {
    let mut frames: Vec<Frame> = Vec::new();
    let mut offset = BODY_LEN;
    while offset < payload.len() {
        match Frame::parse(&payload[offset..]) {
            Ok((frame, used)) => {
                let expected = u32::try_from(frames.len()).unwrap_or(u32::MAX);
                if frame.seq != expected {
                    return Err(format!(
                        "frame sequence gap: expected {expected}, found {}",
                        frame.seq
                    ));
                }
                if frames.last().is_some_and(|prev| prev.kind.is_terminal()) {
                    return Err("frames follow a terminal frame".to_owned());
                }
                frames.push(frame);
                offset += used;
            }
            Err(FrameError::Incomplete) => break,
            Err(FrameError::CrcMismatch { .. }) => break,
            Err(FrameError::UnknownKind(byte)) => {
                return Err(format!("unknown frame kind {byte} with a valid frame CRC"));
            }
            Err(FrameError::ReservedFlags(flags)) => {
                return Err(format!("frame flags {flags:#04x} set reserved bits"));
            }
            Err(FrameError::TooManyRegions)
            | Err(FrameError::IntentTooLong)
            | Err(FrameError::PreimageLengthMismatch)
            | Err(FrameError::PreimageTooLarge) => break,
        }
    }
    Ok(frames)
}

fn needs_manual(why: impl Into<String>) -> RecoveryPlan {
    RecoveryPlan::NeedsManual { why: why.into() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::RecordBody;
    use crate::record::frame::Preimage;
    use crate::record::{encode, frame};

    const TARGET: Fingerprint = Fingerprint::strong([0x42; 32]);

    fn body() -> RecordBody {
        RecordBody {
            target: TARGET,
            geometry_generation: 3,
            plan_digest: [0x11; 32],
        }
    }

    fn frame(seq: u32, kind: FrameKind, completed: bool) -> Frame {
        Frame {
            seq,
            kind,
            completed,
            irreversible: false,
            intent: b"step".to_vec(),
            regions: vec![Preimage {
                offset: 2048,
                len: 1024,
                digest: [0x55; 16],
                data: Vec::new(),
            }],
        }
    }

    fn all_frames() -> Vec<Frame> {
        vec![
            frame(0, FrameKind::Preimage, false),
            frame(1, FrameKind::Progress, true),
            frame(2, FrameKind::Committed, true),
        ]
    }

    fn bytes_with(frames: &[Frame]) -> Vec<u8> {
        encode(&body(), frames).expect("编码应成立")
    }

    #[test]
    fn empty_bytes_is_absent() {
        assert_eq!(recover(&[], TARGET), RecoveryPlan::Absent);
    }

    #[test]
    fn committed_record_needs_only_cleanup() {
        let bytes = bytes_with(&all_frames());
        assert_eq!(recover(&bytes, TARGET), RecoveryPlan::Committed);
        assert!(matches!(
            crate::record::decode(&bytes),
            Ok(decoded) if decoded.body == body()
        ));
    }

    #[test]
    fn prefix_ending_in_a_completed_frame_is_replayed() {
        let all = all_frames();
        let bytes = bytes_with(&all[..2]);
        match recover(&bytes, TARGET) {
            RecoveryPlan::Replay { frames } => assert_eq!(frames, all[..2].to_vec()),
            RecoveryPlan::Absent
            | RecoveryPlan::Committed
            | RecoveryPlan::RollBack { .. }
            | RecoveryPlan::Foreign { .. }
            | RecoveryPlan::NeedsManual { .. } => panic!("expected replay"),
        }
    }

    #[test]
    fn prefix_ending_in_an_incomplete_frame_rolls_back() {
        let all = all_frames();
        let bytes = bytes_with(&all[..1]);
        match recover(&bytes, TARGET) {
            RecoveryPlan::RollBack { frames } => assert_eq!(frames, all[..1].to_vec()),
            RecoveryPlan::Absent
            | RecoveryPlan::Committed
            | RecoveryPlan::Replay { .. }
            | RecoveryPlan::Foreign { .. }
            | RecoveryPlan::NeedsManual { .. } => panic!("expected rollback"),
        }
    }

    #[test]
    fn truncation_at_any_byte_has_a_deterministic_plan() {
        let bytes = bytes_with(&all_frames());
        for cut in 0..=bytes.len() {
            let first = recover(&bytes[..cut], TARGET);
            let second = recover(&bytes[..cut], TARGET);
            assert_eq!(first, second, "cut {cut} must be deterministic");
            if cut >= HEADER_LEN {
                assert!(
                    !matches!(first, RecoveryPlan::NeedsManual { .. }),
                    "cut {cut} is a record prefix and must not need manual repair: {first:?}"
                );
            }
        }
    }

    #[test]
    fn a_torn_trailing_frame_does_not_poison_earlier_frames() {
        let all = all_frames();
        let mut bytes = bytes_with(&all[..2]);
        let mut partial = Vec::new();
        frame::Frame::encode_into(&all[2], &mut partial).expect("编码应成立");
        bytes.extend_from_slice(&partial[..partial.len() / 2]);
        let plan = recover(&bytes, TARGET);
        match plan {
            RecoveryPlan::Replay { frames } => assert_eq!(frames, all[..2].to_vec()),
            RecoveryPlan::Absent
            | RecoveryPlan::Committed
            | RecoveryPlan::RollBack { .. }
            | RecoveryPlan::Foreign { .. }
            | RecoveryPlan::NeedsManual { .. } => panic!("expected replay from the intact prefix"),
        }
    }

    #[test]
    fn foreign_magic_is_reported_without_reading_frames() {
        let mut bytes = bytes_with(&all_frames());
        bytes[0] = b'Z';
        assert_eq!(
            recover(&bytes, TARGET),
            RecoveryPlan::Foreign { magic: *b"ZRB1" }
        );
    }

    #[test]
    fn another_target_is_needs_manual() {
        let bytes = bytes_with(&all_frames());
        let other = Fingerprint::strong([0x99; 32]);
        assert!(matches!(
            recover(&bytes, other),
            RecoveryPlan::NeedsManual { .. }
        ));
    }

    #[test]
    fn a_sequence_gap_is_needs_manual() {
        let gap = vec![frame(1, FrameKind::Preimage, false)];
        let bytes = bytes_with(&gap);
        assert!(matches!(
            recover(&bytes, TARGET),
            RecoveryPlan::NeedsManual { .. }
        ));
    }

    #[test]
    fn frames_after_terminal_are_needs_manual() {
        let mut all = all_frames();
        all.push(frame(3, FrameKind::Progress, true));
        let bytes = bytes_with(&all);
        assert!(matches!(
            recover(&bytes, TARGET),
            RecoveryPlan::NeedsManual { .. }
        ));
    }
}
