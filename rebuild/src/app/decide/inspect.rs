//! `inspect` 的判定：记录状态与占用来源的现场视图。

use crate::app::observe::snapshot::InspectSnapshot;

use super::report::{self, InspectReport, RecordVerdict, Report};
use super::{Decision, Refusal, Step, StepKind};

/// 判定 `inspect`。
pub fn decide(snapshot: &InspectSnapshot) -> Decision {
    let Some(directory) = &snapshot.record.directory else {
        // 目标给不出记录目录的键：没有可检查的记录位置，读侧如实报告而不是拒绝。
        let report = InspectReport {
            evidence: None,
            record: RecordVerdict::Unresolved {
                why: snapshot
                    .record
                    .unavailable
                    .clone()
                    .unwrap_or_else(|| "the target exposes no stable identity".to_owned()),
            },
            occupancy: snapshot.occupancy.clone(),
        };
        return report::decision(
            Report::Inspect(report),
            vec![Step::new(StepKind::Inspect, Vec::new())],
        );
    };
    if !snapshot.record.directory_exists {
        return Decision::Refuse(Refusal::RecordStoreMissing {
            path: directory.display().to_string(),
        });
    }
    let record = match (&snapshot.record.bytes, &snapshot.record.unavailable) {
        (Some(bytes), _) => {
            let decoded = match diskedit_core::record::decode(bytes) {
                Ok(decoded) => decoded,
                Err(error) => {
                    return Decision::Refuse(Refusal::RecordDamaged {
                        why: damage(&error),
                    });
                }
            };
            // 记录里写着它属于哪个目标：能自行证明身份的目标必须与之相符，否则记录是
            // 别的工作留下的证据，不能被当作本目标的现状（基线判定为外来）。
            if let Some(identity) = snapshot.target.identity {
                if decoded.body.target != identity {
                    return Decision::Refuse(Refusal::RecordForeign {
                        why: "identity: the record belongs to a different target — it must not be \
                              cleaned up or replayed for this target"
                            .to_owned(),
                    });
                }
            }
            RecordVerdict::Present {
                bytes: bytes.len() as u64,
                decoded: decode_summary(&decoded),
            }
        }
        (None, Some(_)) => {
            let path = snapshot
                .record
                .file
                .clone()
                .unwrap_or_else(|| directory.clone());
            return Decision::Refuse(Refusal::RecordUnreadable {
                path: path.display().to_string(),
            });
        }
        (None, None) => RecordVerdict::Absent,
    };
    let report = InspectReport {
        evidence: snapshot.record.file.clone(),
        record,
        occupancy: snapshot.occupancy.clone(),
    };
    report::decision(
        Report::Inspect(report),
        vec![Step::new(StepKind::Inspect, Vec::new())],
    )
}

fn decode_summary(decoded: &diskedit_core::record::DecodedRecord) -> String {
    format!(
        "geometry generation {}, plan digest {}",
        decoded.body.geometry_generation,
        hex(&decoded.body.plan_digest)
    )
}

/// 把解码失败归类成与基线一致的三种结构结论：截断、校验不符、格式不认。
fn damage(error: &diskedit_core::record::RecordError) -> String {
    use diskedit_core::record::RecordError;
    match error {
        RecordError::TooShort { .. } | RecordError::FrameIncomplete => {
            "structure: the record is shorter than its declared length — keep the file for manual \
             inspection"
                .to_owned()
        }
        RecordError::HeaderCrcMismatch
        | RecordError::PayloadCrcMismatch
        | RecordError::BodyCrcMismatch
        | RecordError::FrameCrcMismatch { .. } => {
            "structure: the record's CRC does not match — keep the file for manual inspection"
                .to_owned()
        }
        RecordError::ForeignFormat { .. }
        | RecordError::PayloadTooLarge(_)
        | RecordError::TrailingBytes
        | RecordError::FrameSequence { .. }
        | RecordError::UnknownFrameKind(_)
        | RecordError::ReservedFrameFlags(_)
        | RecordError::FramesAfterTerminal
        | RecordError::IntentTooLong
        | RecordError::TooManyRegions
        | RecordError::PreimageLengthMismatch
        | RecordError::PreimageTooLarge => {
            "structure: the record's magic is not recognized or its payload does not match the \
             layout this build reads — keep the file; `diskedit recover --release` will not \
             quarantine it automatically, so remove it by hand once you are sure it is stale, and \
             nothing may write to this target until then"
                .to_owned()
        }
    }
}

fn hex(bytes: &[u8]) -> String {
    let mut text = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        text.push_str(&format!("{byte:02x}"));
    }
    text
}
