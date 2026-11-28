//! 报告字段的单一来源：JSON 键与 TEXT 行名都必须覆盖 `LayoutField`。

use std::collections::BTreeSet;

use diskedit::app::decide::report::{LayoutLabel, LayoutPartition, LayoutReport};
use diskedit::app::report::{LayoutField, json, text};
use diskedit_core::fs::ContentKind;

fn sample() -> LayoutReport {
    LayoutReport {
        label: LayoutLabel::Gpt,
        damaged: false,
        sector_size: 512,
        size_bytes: 1_048_576,
        disk_guid: Some("01234567-89ab-cdef-0123-456789abcdef".to_owned()),
        last_part: Some(1),
        partitions: vec![LayoutPartition {
            number: 1,
            first_lba: 2048,
            last_lba: 4095,
            size_bytes: 1_048_576,
            type_text: "linux".to_owned(),
            content: ContentKind::Unknown,
            name: "root".to_owned(),
        }],
    }
}

#[test]
fn layout_json_and_text_cover_every_defined_field() {
    let report = sample();

    let document: serde_json::Value =
        serde_json::from_str(&json::layout(&report)).expect("layout JSON 可解析");
    let keys: BTreeSet<&str> = document
        .as_object()
        .expect("layout JSON 是对象")
        .keys()
        .map(String::as_str)
        .collect();
    let defined: BTreeSet<&str> = LayoutField::ALL.iter().map(|field| field.name()).collect();
    assert_eq!(keys, defined, "JSON 字段与定义点不一致");

    let rendered = text::layout(&report);
    for field in LayoutField::ALL {
        assert!(
            rendered.contains(field.text_prefix()),
            "TEXT 缺少字段 {}: {rendered}",
            field.name()
        );
    }
}
