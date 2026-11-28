//! 现场结论的文本渲染。

use super::LayoutField;
use crate::app::decide::report::{
    AddPreview, LayoutPartition, LayoutReport, RecordSiteReport, RecordVerdict, ResizePreview,
};
use crate::platform::Occupancy;

/// `layout` 的固定文本行；`--json` 的结构化明细走 `json::layout`。
pub fn layout(report: &LayoutReport) -> String {
    let mut lines = vec![
        field_line(LayoutField::Label, report.label.as_str()),
        field_line(LayoutField::Damaged, report.damaged),
        field_line(LayoutField::SectorSize, report.sector_size),
        field_line(LayoutField::SizeBytes, report.size_bytes),
    ];
    if let Some(guid) = &report.disk_guid {
        lines.push(field_line(LayoutField::DiskGuid, guid));
    }
    if let Some(last) = report.last_part {
        lines.push(field_line(LayoutField::LastPart, last));
    }
    for partition in &report.partitions {
        lines.push(partition_line(partition));
    }
    lines.join("\n")
}

fn field_line(field: LayoutField, value: impl std::fmt::Display) -> String {
    format!("{} {}", field.text_prefix(), value)
}

fn partition_line(partition: &LayoutPartition) -> String {
    let name = if partition.name.is_empty() {
        "-".to_owned()
    } else {
        partition.name.clone()
    };
    format!(
        "{}{}: LBA {}..{} ({} bytes) type {} name {} fs {}",
        LayoutField::Partitions.text_prefix(),
        partition.number,
        partition.first_lba,
        partition.last_lba,
        partition.size_bytes,
        partition.type_text,
        name,
        partition.content.as_str()
    )
}

/// `records show` 的固定文本行。
pub fn show(report: &RecordSiteReport) -> String {
    let evidence = match &report.evidence {
        Some(path) => path.display().to_string(),
        None => "(none)".to_owned(),
    };
    let record = match &report.record {
        RecordVerdict::Unresolved { why } => format!("unresolved: {why}"),
        RecordVerdict::Absent => "absent".to_owned(),
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

/// `add --dry-run` 的落位预览；不写字节。
pub fn add_preview(preview: &AddPreview) -> String {
    let bytes = (preview.end_lba - preview.start_lba + 1).saturating_mul(preview.sector_size);
    format!(
        "partition #{}: LBA {}..{} ({} bytes)\n(dry run: no bytes were written)",
        preview.partition, preview.start_lba, preview.end_lba, bytes
    )
}

/// 退出码 `5` 的人读指引：三要素（发生了什么、证据在哪、下一步）固定，
/// 末尾追加本次运行的具体原因，便于排障。
pub fn recovery(cause: &str) -> String {
    format!(
        "— the target may already have been changed; check with `diskedit layout` before retrying\n\
         evidence: `diskedit records show <target>` names any recovery file left for the target\n\
         next: `diskedit undo` restores the bytes recorded for this target when the record still \
         allows it; `diskedit release` releases the record and leaves those bytes in place\n\
         cause: {cause}"
    )
}

fn verdict(record: &RecordVerdict, occupancy: &Occupancy) -> &'static str {
    match (record, occupancy) {
        (RecordVerdict::Unresolved { .. }, _) => "nothing inspected",
        (RecordVerdict::Present { .. }, _) => "record present; run undo before writing",
        (_, Occupancy::Used { .. }) => "target is in use; read-only commands remain available",
        (_, Occupancy::Unknown { .. }) => {
            "occupancy cannot be decided; treat the target as occupied"
        }
        (RecordVerdict::Absent, Occupancy::Idle) => "no record and no usage; target is idle",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recovery_text_keeps_the_three_elements_and_adds_the_cause() {
        let text = recovery("the write barrier failed");
        assert!(
            text.contains("the target may already have been changed"),
            "{text}"
        );
        assert!(text.contains("evidence:"), "{text}");
        assert!(text.contains("next:"), "{text}");
        assert!(text.contains("cause: the write barrier failed"), "{text}");
    }
}
