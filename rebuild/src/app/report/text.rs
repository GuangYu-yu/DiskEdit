//! 现场结论的文本渲染。

use crate::app::decide::report::{InspectReport, RecordVerdict, ResizePreview};
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

/// `resize --dry-run` 的计算预览；不写字节。
pub fn resize(report: &ResizePreview) -> String {
    let action = match report.filesystem {
        Some(content) if report.grow => format!("grow filesystem ({content:?})"),
        Some(content) => format!("shrink filesystem ({content:?})"),
        None => "partition table only".to_owned(),
    };
    format!(
        "partition #{}: LBA {}..{} -> {}..{} (sector {}); {action}\n(dry run: no bytes were written)",
        report.partition,
        report.start_lba,
        report.old_end_lba,
        report.start_lba,
        report.new_end_lba,
        report.sector_size
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
