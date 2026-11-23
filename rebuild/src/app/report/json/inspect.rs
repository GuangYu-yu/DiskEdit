//! `inspect` 的 JSON。
//!
//! 逐分区输出编号、起止与内容类别的三态观测；拓扑未知时 `partitions` 为
//! `null` 并保留原因，不输出空数组冒充"没有分区"。

use diskedit_core::evidence::Observation;
use diskedit_core::fs::ContentKind;

use super::{array, number, object, quote};
use crate::app::observe::Snapshot;
use crate::app::observe::snapshot::PartitionFacts;

/// 渲染 `inspect` 的 JSON 对象。
pub fn render(target: &str, snapshot: &Snapshot) -> String {
    let partitions = match snapshot.contents() {
        Some(facts) => array(facts.iter().map(partition).collect()),
        None => "null".to_owned(),
    };
    object(&[
        ("target", quote(target)),
        ("partitions", partitions),
        ("topology", topology(&snapshot.topology)),
    ])
}

/// 拓扑的三态；已知时给出分区数。
fn topology(observation: &Observation<crate::app::observe::snapshot::Topology>) -> String {
    match observation {
        Observation::Known {
            value,
            source,
            generation,
        } => object(&[
            ("state", quote("known")),
            ("partitions", number(value.partitions.len() as u64)),
            ("source", quote(source.name())),
            ("generation", number(generation.get())),
        ]),
        Observation::Unknown { why } => object(&[("state", quote("unknown")), ("why", quote(why))]),
        Observation::NotApplicable { why } => {
            object(&[("state", quote("not_applicable")), ("why", quote(why))])
        }
    }
}

fn partition(facts: &PartitionFacts) -> String {
    object(&[
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
