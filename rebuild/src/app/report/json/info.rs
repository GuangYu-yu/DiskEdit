//! `info` 的 JSON。
//!
//! 只输出目标级事实：身份、种类、长度、逻辑块大小、分区表形态与分区数。未知
//! 项保留三态，不减字段、不填默认值。

use diskedit_core::evidence::{Evidence, Observation};
use diskedit_core::table::GptEvidence;

use super::{number, object, observed_number, quote};
use crate::app::observe::Snapshot;
use crate::platform::TargetKind;

/// 渲染 `info` 的 JSON 对象。
pub fn render(target: &str, snapshot: &Snapshot) -> String {
    object(&[
        ("target", quote(target)),
        ("generation", number(snapshot.generation.get())),
        ("identity", identity(&snapshot.identity)),
        (
            "kind",
            match snapshot.disk.kind.value() {
                Some(TargetKind::File) => quote("file"),
                Some(TargetKind::Block) => quote("block"),
                None => "null".to_owned(),
            },
        ),
        ("bytes", observed_number(&snapshot.disk.byte_len)),
        ("sector_size", observed_number(&snapshot.disk.sector_size)),
        ("table", table_format(snapshot)),
        (
            "partitions",
            snapshot
                .contents()
                .map_or_else(|| "null".to_owned(), |facts| number(facts.len() as u64)),
        ),
        ("occupancy", occupancy(&snapshot.occupancy)),
    ])
}

/// 占用视图的摘要；未知与不适用保留原因。
fn occupancy(observation: &Observation<crate::platform::Occupancy>) -> String {
    match observation {
        Observation::Known {
            value,
            source,
            generation,
        } => object(&[
            ("state", quote("known")),
            ("mounts", number(value.mounts.len() as u64)),
            ("swaps", number(value.swaps.len() as u64)),
            ("holders", number(value.holders.len() as u64)),
            ("array_member", value.array_member.to_string()),
            ("loops", number(value.loops.len() as u64)),
            ("source", quote(source.name())),
            ("generation", number(generation.get())),
        ]),
        Observation::Unknown { why } => object(&[("state", quote("unknown")), ("why", quote(why))]),
        Observation::NotApplicable { why } => {
            object(&[("state", quote("not_applicable")), ("why", quote(why))])
        }
    }
}

/// 身份以十六进制呈现；未知时保留原因。
fn identity(observation: &Observation<diskedit_core::fingerprint::Fingerprint>) -> String {
    match observation {
        Observation::Known { value, .. } => quote(&hex(value.bytes())),
        Observation::Unknown { why } => object(&[("state", quote("unknown")), ("why", quote(why))]),
        Observation::NotApplicable { why } => {
            object(&[("state", quote("not_applicable")), ("why", quote(why))])
        }
    }
}

/// 分区表形态：GPT 可用时为 `gpt`，否则 MBR 存在时为 `mbr`，都读不出为 `null`。
pub(crate) fn table_format(snapshot: &Snapshot) -> String {
    match &snapshot.table.gpt {
        Observation::Known {
            value: GptEvidence::Usable(_),
            ..
        } => quote("gpt"),
        Observation::Known {
            value: GptEvidence::Absent,
            ..
        } => match &snapshot.table.mbr {
            Observation::Known {
                value: Evidence::Present(_),
                ..
            } => quote("mbr"),
            Observation::Known {
                value: Evidence::Absent,
                ..
            }
            | Observation::Known {
                value: Evidence::UnknownFormat { .. },
                ..
            }
            | Observation::Known {
                value: Evidence::Unreadable { .. },
                ..
            }
            | Observation::Unknown { .. }
            | Observation::NotApplicable { .. } => "null".to_owned(),
        },
        Observation::Known {
            value: GptEvidence::Damaged(_),
            ..
        }
        | Observation::Unknown { .. }
        | Observation::NotApplicable { .. } => "null".to_owned(),
    }
}

fn hex(bytes: [u8; 32]) -> String {
    let mut out = String::with_capacity(64);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}
