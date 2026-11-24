//! 现场结论的文本渲染。

use crate::app::decide::report::{InspectReport, RecordVerdict};
use crate::platform::Occupancy;

/// `inspect` 的固定文本行。
pub fn inspect(report: &InspectReport) -> String {
    let evidence = match &report.evidence {
        Some(path) => path.display().to_string(),
        None => "(none)".to_owned(),
    };
    let record = match &report.record {
        RecordVerdict::Unresolved { why } => format!("unresolved: {why}"),
        RecordVerdict::Absent => "absent".to_owned(),
        RecordVerdict::Unreadable { why } => format!("unreadable: {why}"),
        RecordVerdict::Present { bytes, decoded } => format!("present ({bytes} bytes; {decoded})"),
    };
    let occupancy = match &report.occupancy {
        Occupancy::Idle => "idle".to_owned(),
        Occupancy::Used { evidence } => format!("used: {evidence}"),
        Occupancy::Unknown { why } => format!("undecidable: {why}"),
    };
    format!(
        "evidence: {evidence}\nrecord: {record}\nverdict: {}\noccupancy: {occupancy}",
        verdict(&report.record, &report.occupancy)
    )
}

fn verdict(record: &RecordVerdict, occupancy: &Occupancy) -> &'static str {
    match (record, occupancy) {
        (RecordVerdict::Unresolved { .. }, _) => "nothing inspected",
        (RecordVerdict::Present { .. }, _) => "record present; run recover before writing",
        (_, Occupancy::Used { .. }) => "target is in use; read-only commands remain available",
        (_, Occupancy::Unknown { .. }) => {
            "occupancy cannot be decided; treat the target as occupied"
        }
        (RecordVerdict::Unreadable { .. }, _) => "record state cannot be proved; do not write",
        (RecordVerdict::Absent, Occupancy::Idle) => "no record and no usage; target is idle",
    }
}
