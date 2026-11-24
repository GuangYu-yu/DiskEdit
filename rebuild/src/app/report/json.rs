//! 结构化输出的 JSON 渲染。

use serde_json::{Value, json};

use crate::app::decide::report::{InfoReport, RecordsReport};

/// `info` 的整盘 JSON。
pub fn info(report: &InfoReport) -> String {
    let partitions: Vec<Value> = report
        .partitions
        .iter()
        .map(|partition| {
            json!({
                "num": partition.number,
                "first_lba": partition.first_lba,
                "last_lba": partition.last_lba,
                "size_bytes": partition.size_bytes,
                "type": partition.type_text,
                "name": partition.name,
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
    json!({
        "label": report.label.as_str(),
        "damaged": report.damaged,
        "sector_size": report.sector_size,
        "size_bytes": report.size_bytes,
        "disk_guid": report.disk_guid,
        "last_part": report.last_part,
        "partitions": partitions,
        "preview": preview,
    })
    .to_string()
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
