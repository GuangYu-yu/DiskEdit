//! `layout` 的判定：把快照变成整盘视图；`--json` 的选择由请求携带。

use crate::app::observe::Request;
use crate::app::observe::snapshot::Snapshot;
use crate::app::observe::table::{PartitionView, TableView};

use super::report::{self, LayoutLabel, LayoutPartition, LayoutReport, Report};
use super::{Decision, Refusal, Step, StepKind};

/// 判定 `layout`。
pub fn decide(snapshot: &Snapshot, _request: &Request) -> Decision {
    let report = match build(snapshot) {
        Ok(report) => report,
        Err(refusal) => return Decision::Refuse(refusal),
    };
    report::decision(
        Report::Layout(report),
        vec![Step::new(StepKind::Inspect, Vec::new())],
    )
}

fn build(snapshot: &Snapshot) -> Result<LayoutReport, Refusal> {
    let (label, partition_views, disk_guid) = match &snapshot.table {
        TableView::Gpt(view) => (
            LayoutLabel::Gpt,
            view.partitions.as_slice(),
            Some(view.disk_guid.clone()),
        ),
        TableView::Mbr(view) => (LayoutLabel::Mbr, view.partitions.as_slice(), None),
        TableView::None => (LayoutLabel::None, &[][..], None),
        TableView::Damaged { why, .. } => {
            return Err(Refusal::TableDamaged { why: why.clone() });
        }
    };
    let partitions: Vec<LayoutPartition> = partition_views.iter().map(layout_partition).collect();
    // GPT 头保留字节损伤：表可用，但在诊断里标 damaged 提示头部被越界写过。
    let damaged = matches!(&snapshot.table, TableView::Gpt(view) if view.reserved_damaged);
    Ok(LayoutReport {
        label,
        damaged,
        sector_size: snapshot.sector_size(),
        size_bytes: snapshot.target.bytes,
        disk_guid,
        last_part: partitions.last().map(|partition| partition.number),
        partitions,
    })
}

fn layout_partition(partition: &PartitionView) -> LayoutPartition {
    LayoutPartition {
        number: partition.number,
        first_lba: partition.first_lba,
        last_lba: partition.last_lba,
        size_bytes: partition.bytes,
        type_text: partition.type_text.clone(),
        content: partition.content,
        name: partition.name.clone(),
    }
}
