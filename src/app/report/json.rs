//! 结构化输出的 JSON 渲染。

use serde_json::{Map, Value, json};

use super::LayoutField;
use crate::app::decide::report::{LayoutLabel, LayoutReport, RecordsReport};

/// `layout --json` 的整盘 JSON。
pub fn layout(report: &LayoutReport) -> String {
    let partitions: Vec<Value> = report
        .partitions
        .iter()
        .map(|partition| {
            // MBR 没有分区名：输出 `null`，不是空串。
            let name = if report.label == LayoutLabel::Mbr {
                Value::Null
            } else {
                Value::String(partition.name.clone())
            };
            json!({
                "num": partition.number,
                "first_lba": partition.first_lba,
                "last_lba": partition.last_lba,
                "size_bytes": partition.size_bytes,
                "type": partition.type_text,
                "name": name,
                "fs": partition.content.as_str(),
            })
        })
        .collect();
    let mut document = Map::new();
    document.insert(
        LayoutField::Label.name().to_owned(),
        json!(report.label.as_str()),
    );
    document.insert(
        LayoutField::Damaged.name().to_owned(),
        json!(report.damaged),
    );
    document.insert(
        LayoutField::SectorSize.name().to_owned(),
        json!(report.sector_size),
    );
    document.insert(
        LayoutField::SizeBytes.name().to_owned(),
        json!(report.size_bytes),
    );
    document.insert(
        LayoutField::DiskGuid.name().to_owned(),
        json!(report.disk_guid),
    );
    document.insert(
        LayoutField::Partitions.name().to_owned(),
        Value::Array(partitions),
    );
    // 无分区时省略 `last_part`，而不是输出 `null`。
    if let Some(last) = report.last_part {
        document.insert(LayoutField::LastPart.name().to_owned(), json!(last));
    }
    Value::Object(document).to_string()
}

/// `records` 的 JSON。
pub fn records(report: &RecordsReport) -> String {
    let records: Vec<Value> = report
        .records
        .iter()
        .map(|record| {
            json!({
                "id": record.id,
                "path": record.path.display().to_string(),
                "bytes": record.bytes,
                "decoded": record.decoded,
            })
        })
        .collect();
    json!({
        "root": report.root.as_ref().map(|root| root.display().to_string()),
        "scanned": report.scanned.as_ref().map(|root| root.display().to_string()),
        "complete": report.complete,
        "records": records,
    })
    .to_string()
}
