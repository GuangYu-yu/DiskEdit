//! `check` 的 JSON。
//!
//! 输出单个分区的编号与内容类别三态；检查器的结论由报告层在文本通道给出，
//! 这里不重复判定。

use diskedit_core::evidence::Observation;
use diskedit_core::fs::ContentKind;

use super::{number, object, quote};
use crate::app::observe::snapshot::PartitionFacts;

/// 渲染 `check` 的 JSON 对象。
pub fn render(target: &str, facts: &PartitionFacts) -> String {
    object(&[
        ("target", quote(target)),
        ("number", number(u64::from(facts.partition.number))),
        ("start_lba", number(facts.partition.start_lba)),
        ("sectors", number(facts.partition.sectors)),
        ("content", content(&facts.content)),
    ])
}

fn content(observation: &Observation<ContentKind>) -> String {
    match observation {
        Observation::Known {
            value,
            source,
            generation,
        } => object(&[
            ("state", quote("known")),
            ("value", quote(value.as_str())),
            ("source", quote(source.name())),
            ("generation", number(generation.get())),
        ]),
        Observation::Unknown { why } => object(&[("state", quote("unknown")), ("why", quote(why))]),
        Observation::NotApplicable { why } => {
            object(&[("state", quote("not_applicable")), ("why", quote(why))])
        }
    }
}
