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
        (Some(bytes), _) => RecordVerdict::Present {
            bytes: bytes.len() as u64,
            decoded: decode_summary(bytes),
        },
        (None, Some(why)) => RecordVerdict::Unreadable { why: why.clone() },
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

fn decode_summary(bytes: &[u8]) -> String {
    match diskedit_core::record::decode(bytes) {
        Ok(decoded) => format!(
            "geometry generation {}, plan digest {}",
            decoded.body.geometry_generation,
            hex(&decoded.body.plan_digest)
        ),
        Err(error) => format!("record is unreadable: {error:?}"),
    }
}

fn hex(bytes: &[u8]) -> String {
    let mut text = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        text.push_str(&format!("{byte:02x}"));
    }
    text
}
