//! `check` 的 JSON。
//!
//! 输出单个分区的编号、内容类别三态与刚跑过的检查器；检查器非零时不会走到这里，
//! 失败按拒绝在文本通道上报。

use diskedit_core::evidence::Observation;
use diskedit_core::fs::ContentKind;

use super::{array, number, object, quote};
use crate::app::execute::check::CheckReport;
use crate::app::observe::snapshot::PartitionFacts;

/// 渲染 `check` 的 JSON 对象；`report` 是刚跑完的检查器。
pub fn render(target: &str, facts: &PartitionFacts, report: &CheckReport) -> String {
    object(&[
        ("target", quote(target)),
        ("number", number(u64::from(facts.partition.number))),
        ("start_lba", number(facts.partition.start_lba)),
        ("sectors", number(facts.partition.sectors)),
        ("content", content(&facts.content)),
        ("checker", checker(report)),
    ])
}

fn checker(report: &CheckReport) -> String {
    let programs = report
        .programs
        .iter()
        .map(|program| quote(&format!("{program:?}")))
        .collect();
    object(&[("programs", array(programs))])
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
