//! 纯前缀恢复：任意完整帧前缀都给出确定结论。
//!
//! 写目标字节前，该步的原像帧必须已完整落盘并携带 CRC；因此序列在第一个
//! 不完整或 CRC 不符的帧之前停止是安全的：该帧对应的写入尚未发生。
//! 恢复器不看时钟、不读目标，结果只由记录字节与当前指纹决定。

use super::BODY_LEN;
use super::codec::decode_body;
use super::frame::{Frame, FrameError, FrameKind};
use super::geometry_generation;
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
    NeedsManual { why: ManualReason },
}

/// 记录不能被归入四种结论之一的原因。
///
/// 用枚举而不是文案：调用方要按原因分流（例如"记录属于别的目标"不能被放弃），
/// 按文案分流的分支会在文案改写时悄悄失效。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ManualReason {
    /// 头 CRC 与计算值不符。
    HeaderCrc,
    /// 字节数不足一个头。
    ShortHeader { needed: usize, actual: usize },
    /// 头声明的载荷长度超过实现上限。
    PayloadTooLarge(u32),
    /// 记录字节比头声明的载荷长：追加帧时没有同步重写头。
    ExcessBytes(usize),
    /// 载荷固定前缀的 CRC 不符。
    BodyCrc,
    /// 载荷 CRC 与头声明不符。
    PayloadCrc,
    /// 记录自称属于别的目标。
    DifferentTarget,
    /// 记录声明的几何代次与当前目标不符，或为零。
    LegacyGeneration,
    /// 帧前缀自身不自洽。
    Frames(String),
}

impl std::fmt::Display for ManualReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::HeaderCrc => f.write_str("record header CRC mismatch"),
            Self::ShortHeader { needed, actual } => write!(
                f,
                "record is shorter than its header: need {needed} bytes, got {actual}"
            ),
            Self::PayloadTooLarge(len) => write!(f, "declared payload length {len} is too large"),
            Self::ExcessBytes(count) => write!(
                f,
                "record carries {count} bytes past the length its header declares"
            ),
            Self::BodyCrc => f.write_str("record payload prefix CRC mismatch"),
            Self::PayloadCrc => f.write_str("record payload CRC mismatch"),
            Self::DifferentTarget => f.write_str("record targets a different device"),
            Self::LegacyGeneration => {
                f.write_str("record's geometry generation does not match the target")
            }
            Self::Frames(why) => f.write_str(why),
        }
    }
}

/// 恢复期对当前目标的期望：身份指纹与总字节数。
///
/// 只有身份而没有几何证据时（由 [`Fingerprint`] 转换得到），几何核对无从成立，
/// 记录按需要人工处理拒绝，而不是跳过核对。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RecoveryExpectation {
    identity: Fingerprint,
    /// 目标总字节数；`None` 表示调用方没有几何证据。
    total_bytes: Option<u64>,
}

impl RecoveryExpectation {
    /// 身份与几何都已知的目标。
    pub const fn new(identity: Fingerprint, total_bytes: u64) -> Self {
        Self {
            identity,
            total_bytes: Some(total_bytes),
        }
    }
}

impl From<Fingerprint> for RecoveryExpectation {
    /// 只有身份：没有几何证据，几何核对只能拒绝。
    fn from(identity: Fingerprint) -> Self {
        Self {
            identity,
            total_bytes: None,
        }
    }
}

/// 由记录字节与当前目标推出恢复结论。
pub fn recover(bytes: &[u8], expected: impl Into<RecoveryExpectation>) -> RecoveryPlan {
    let expected = expected.into();
    if bytes.is_empty() {
        return RecoveryPlan::Absent;
    }
    let header = match RecordHeader::parse(bytes) {
        Ok(header) => header,
        Err(HeaderError::ForeignFormat { magic }) => return RecoveryPlan::Foreign { magic },
        Err(HeaderError::HeaderCrcMismatch) => return needs_manual(ManualReason::HeaderCrc),
        Err(HeaderError::TooShort { needed, actual }) => {
            return needs_manual(ManualReason::ShortHeader { needed, actual });
        }
        Err(HeaderError::PayloadTooLarge(len)) => {
            return needs_manual(ManualReason::PayloadTooLarge(len));
        }
    };
    let available = bytes.len() - HEADER_LEN;
    let declared = header.payload_len as usize;
    // 记录只由"整条重写"产生：头声明的长度就是载荷的全部长度。多出字节只可能
    // 来自"追加了帧却没更新头"，那种记录里的终态不可见，不能按前缀解释成更早
    // 的状态——否则已提交的写入会被当成待回滚。
    if available > declared {
        return needs_manual(ManualReason::ExcessBytes(available - declared));
    }
    let payload = &bytes[HEADER_LEN..];
    if payload.len() < BODY_LEN {
        // 前缀未写全：没有任何帧完整落盘，因此没有目标写入发生。
        return RecoveryPlan::RollBack { frames: Vec::new() };
    }
    let body = match decode_body(payload) {
        Ok(body) => body,
        Err(_) => return needs_manual(ManualReason::BodyCrc),
    };
    if !body.target.matches(expected.identity) {
        return needs_manual(ManualReason::DifferentTarget);
    }
    // 身份与几何在同一条恢复路径上核对：没有几何证据、几何代次为零、或与当前
    // 目标总字节数重算出的代次不等，都不能按记录里的偏移回放。
    let geometry = expected.total_bytes.map(geometry_generation);
    if body.legacy_generation == 0 || geometry != Some(body.legacy_generation) {
        return needs_manual(ManualReason::LegacyGeneration);
    }
    if available == declared && crate::table::crc32(payload) != header.payload_crc {
        return needs_manual(ManualReason::PayloadCrc);
    }
    let frames = match parse_prefix(payload) {
        Ok(frames) => frames,
        Err(why) => return needs_manual(ManualReason::Frames(why)),
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
            | Err(FrameError::PostimageLengthMismatch)
            | Err(FrameError::PreimageTooLarge) => break,
        }
    }
    Ok(frames)
}

fn needs_manual(why: ManualReason) -> RecoveryPlan {
    RecoveryPlan::NeedsManual { why }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::RecordBody;
    use crate::record::frame::Preimage;
    use crate::record::{encode, frame, geometry_generation};

    const TARGET: Fingerprint = Fingerprint::strong([0x42; 32]);
    /// 恢复期看到的目标总字节数。
    const TOTAL: u64 = 1 << 20;

    fn expectation(identity: Fingerprint) -> RecoveryExpectation {
        RecoveryExpectation::new(identity, TOTAL)
    }

    fn body() -> RecordBody {
        RecordBody {
            target: TARGET,
            legacy_generation: geometry_generation(TOTAL),
            plan_digest: [0x11; 32],
        }
    }

    fn body_with_generation(generation: u64) -> RecordBody {
        RecordBody {
            legacy_generation: generation,
            ..body()
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
                after: Vec::new(),
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
    fn a_generation_that_is_zero_or_differs_from_the_target_is_not_replayed() {
        // 零代次是没有几何证据的保留值，非零但不等于当前目标重算值的代次同样不成立：
        // 两者都按需要人工处理拒绝，而不是照常回滚或重放。
        let bytes = encode(&body_with_generation(0), &all_frames()).expect("编码应成立");
        assert!(matches!(
            recover(&bytes, expectation(TARGET)),
            RecoveryPlan::NeedsManual {
                why: ManualReason::LegacyGeneration
            }
        ));
        let bytes = encode(&body_with_generation(3), &all_frames()).expect("编码应成立");
        assert!(matches!(
            recover(&bytes, expectation(TARGET)),
            RecoveryPlan::NeedsManual {
                why: ManualReason::LegacyGeneration
            }
        ));
    }

    #[test]
    fn empty_bytes_is_absent() {
        assert_eq!(recover(&[], TARGET), RecoveryPlan::Absent);
    }

    #[test]
    fn a_record_without_geometry_evidence_is_refused() {
        // 只有身份、没有总字节数时，几何核对无从成立：记录不能回放。
        let bytes = bytes_with(&all_frames());
        assert!(matches!(
            recover(&bytes, TARGET),
            RecoveryPlan::NeedsManual {
                why: ManualReason::LegacyGeneration
            }
        ));
    }

    #[test]
    fn committed_record_needs_only_cleanup() {
        let bytes = bytes_with(&all_frames());
        assert_eq!(
            recover(&bytes, expectation(TARGET)),
            RecoveryPlan::Committed
        );
        assert!(matches!(
            crate::record::decode(&bytes),
            Ok(decoded) if decoded.body == body()
        ));
    }

    #[test]
    fn prefix_ending_in_a_completed_frame_is_replayed() {
        let all = all_frames();
        let bytes = bytes_with(&all[..2]);
        match recover(&bytes, expectation(TARGET)) {
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
        match recover(&bytes, expectation(TARGET)) {
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
            let first = recover(&bytes[..cut], expectation(TARGET));
            let second = recover(&bytes[..cut], expectation(TARGET));
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
    fn frames_appended_without_rewriting_the_header_are_refused() {
        // 帧格式定案为"整条重写"：只追加帧而不更新头时，追加部分不可见。这种记录
        // 不能按前缀读成"更早的状态"，否则已提交的写入会被当成待回滚；它是两头
        // 不一致的记录，交人工判定。
        let all = all_frames();
        let mut bytes = bytes_with(&all[..2]);
        let mut extra = Vec::new();
        frame::Frame::encode_into(&all[2], &mut extra).expect("编码应成立");
        bytes.extend_from_slice(&extra);
        assert!(matches!(
            recover(&bytes, expectation(TARGET)),
            RecoveryPlan::NeedsManual { .. }
        ));
    }

    #[test]
    fn the_target_and_generation_checks_apply_to_every_state() {
        // 四态各自成立时，目标指纹与几何代次的核对仍然独立生效：它们不是
        // "看到终态就跳过"的旁路。
        let cases: Vec<(Vec<Frame>, RecoveryPlan)> = vec![
            (Vec::new(), RecoveryPlan::RollBack { frames: Vec::new() }),
            (
                vec![frame(0, FrameKind::Preimage, false)],
                RecoveryPlan::RollBack {
                    frames: vec![frame(0, FrameKind::Preimage, false)],
                },
            ),
            (
                vec![frame(0, FrameKind::Progress, true)],
                RecoveryPlan::Replay {
                    frames: vec![frame(0, FrameKind::Progress, true)],
                },
            ),
            (all_frames(), RecoveryPlan::Committed),
        ];
        let other = Fingerprint::strong([0x99; 32]);
        for (frames, expected) in cases {
            let bytes = bytes_with(&frames);
            assert_eq!(
                recover(&bytes, expectation(TARGET)),
                expected,
                "四态必须分开"
            );
            assert!(
                matches!(
                    recover(&bytes, expectation(other)),
                    RecoveryPlan::NeedsManual { .. }
                ),
                "目标指纹不符时必须拒绝"
            );
            let generation = encode(&body_with_generation(3), &frames).expect("编码应成立");
            assert!(
                matches!(
                    recover(&generation, expectation(TARGET)),
                    RecoveryPlan::NeedsManual { .. }
                ),
                "几何代次不符时必须拒绝"
            );
        }
    }

    #[test]
    fn foreign_magic_is_reported_without_reading_frames() {
        let mut bytes = bytes_with(&all_frames());
        bytes[0] = b'Z';
        assert_eq!(
            recover(&bytes, expectation(TARGET)),
            RecoveryPlan::Foreign { magic: *b"ZREC" },
            "首个字节被改写的记录按外来格式报告，且不读后续帧"
        );
    }

    #[test]
    fn another_target_is_needs_manual() {
        let bytes = bytes_with(&all_frames());
        let other = Fingerprint::strong([0x99; 32]);
        assert!(matches!(
            recover(&bytes, expectation(other)),
            RecoveryPlan::NeedsManual { .. }
        ));
    }

    #[test]
    fn a_sequence_gap_is_needs_manual() {
        let gap = vec![frame(1, FrameKind::Preimage, false)];
        let bytes = bytes_with(&gap);
        assert!(matches!(
            recover(&bytes, expectation(TARGET)),
            RecoveryPlan::NeedsManual { .. }
        ));
    }

    #[test]
    fn frames_after_terminal_are_needs_manual() {
        let mut all = all_frames();
        all.push(frame(3, FrameKind::Progress, true));
        let bytes = bytes_with(&all);
        assert!(matches!(
            recover(&bytes, expectation(TARGET)),
            RecoveryPlan::NeedsManual { .. }
        ));
    }
}
