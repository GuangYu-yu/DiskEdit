//! 结构化输出的 JSON 渲染。

use serde_json::{Value, json};

use crate::app::decide::report::{InfoLabel, InfoReport, RecordsReport};

/// `info` 的整盘 JSON。
pub fn info(report: &InfoReport) -> String {
    let partitions: Vec<Value> = report
        .partitions
        .iter()
        .map(|partition| {
            // MBR 没有分区名；基线用 `null`，不是空串。
            let name = if report.label == InfoLabel::Mbr {
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
    let preview = report.preview.as_ref().map(|preview| {
        let moves: Vec<Value> = preview
            .moves
            .iter()
            .map(|entry| {
                json!({
                    "num": entry.number,
                    "first_lba": entry.first_lba,
                    "last_lba": entry.last_lba,
                    "delta_lba": entry.delta_lba,
                    "bytes": entry.bytes,
                    "swap": entry.swap,
                })
            })
            .collect();
        json!({
            "grow_part": preview.partition,
            "new_end_lba": preview.new_end_lba,
            "sector_size": preview.sector_size,
            "moves": moves,
        })
    });
    let mut document = json!({
        "label": report.label.as_str(),
        "damaged": report.damaged,
        "sector_size": report.sector_size,
        "size_bytes": report.size_bytes,
        "disk_guid": report.disk_guid,
        "partitions": partitions,
    });
    let object = document.as_object_mut().expect("info 文档是对象");
    // 基线在无分区时省略 `last_part`，而不是输出 `null`。
    if let Some(last) = report.last_part {
        object.insert("last_part".to_owned(), json!(last));
    }
    // 基线在无预览时完全不输出该字段；`null` 会改变字段集。
    if let Some(preview) = preview {
        object.insert("preview".to_owned(), preview);
    }
    document.to_string()
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
