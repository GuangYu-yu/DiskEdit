//! `info` 的判定：把快照变成整盘视图；`--grow` 形态给出扩容预览。

use crate::app::observe::snapshot::Snapshot;
use crate::app::observe::table::{PartitionView, TableView};
use crate::app::observe::{PartitionSelector, Request};

use super::report::{self, GrowMove, GrowPreview, InfoLabel, InfoPartition, InfoReport, Report};
use super::{Decision, Refusal, Step, StepKind};

/// 判定 `info`；`grow` 由解析层从命令形态带下来。
pub fn decide(snapshot: &Snapshot, request: &Request, grow: bool) -> Decision {
    let report = match build(snapshot, request, grow) {
        Ok(report) => report,
        Err(refusal) => return Decision::Refuse(refusal),
    };
    report::decision(
        Report::Info(report),
        vec![Step::new(StepKind::Inspect, Vec::new())],
    )
}

fn build(snapshot: &Snapshot, request: &Request, grow: bool) -> Result<InfoReport, Refusal> {
    let (label, partition_views, disk_guid, last_usable) = match &snapshot.table {
        TableView::Gpt(view) => (
            InfoLabel::Gpt,
            view.partitions.as_slice(),
            Some(view.disk_guid.clone()),
            Some(view.last_usable_lba),
        ),
        TableView::Mbr(view) => (InfoLabel::Mbr, view.partitions.as_slice(), None, None),
        TableView::None => (InfoLabel::None, &[][..], None, None),
        TableView::Damaged { why, .. } => {
            return Err(Refusal::TableDamaged { why: why.clone() });
        }
    };
    let partitions: Vec<InfoPartition> = partition_views.iter().map(info_partition).collect();
    let preview = if grow {
        Some(grow_preview(
            request,
            snapshot,
            label,
            partition_views,
            last_usable,
        )?)
    } else {
        None
    };
    Ok(InfoReport {
        label,
        damaged: false,
        sector_size: snapshot.sector_size(),
        size_bytes: snapshot.target.bytes,
        disk_guid,
        last_part: partitions.last().map(|partition| partition.number),
        partitions,
        preview,
    })
}

fn info_partition(partition: &PartitionView) -> InfoPartition {
    InfoPartition {
        number: partition.number,
        first_lba: partition.first_lba,
        last_lba: partition.last_lba,
        size_bytes: partition.bytes,
        type_text: partition.type_text.clone(),
        content: partition.content,
        name: partition.name.clone(),
    }
}

fn grow_preview(
    request: &Request,
    snapshot: &Snapshot,
    label: InfoLabel,
    partitions: &[PartitionView],
    last_usable: Option<u64>,
) -> Result<GrowPreview, Refusal> {
    if label != InfoLabel::Gpt {
        return Err(Refusal::GptRequired {
            what: "info --grow needs a GPT partition table".to_owned(),
        });
    }
    let number = match request.partition_selector() {
        Some(PartitionSelector::Number(number)) => number,
        Some(PartitionSelector::Last) => partitions
            .last()
            .map(|partition| partition.number)
            .ok_or(Refusal::TableMissing)?,
        None => {
            return Err(Refusal::ArgumentOutOfRange {
                what: "info --grow needs a partition selector".to_owned(),
            });
        }
    };
    let target = partitions
        .iter()
        .find(|partition| partition.number == number)
        .ok_or(Refusal::PartitionMissing { number })?;
    let sector_size = snapshot.sector_size();
    let last_usable = last_usable.ok_or(Refusal::TableMissing)?;
    let tail_free = last_usable.saturating_sub(
        partitions
            .last()
            .map(|partition| partition.last_lba)
            .unwrap_or(last_usable),
    );
    let new_end_lba = target.last_lba.saturating_add(tail_free).min(last_usable);
    let delta = new_end_lba.saturating_sub(target.last_lba);
    let moves = partitions
        .iter()
        .filter(|partition| partition.first_lba > target.last_lba)
        .map(|partition| GrowMove {
            number: partition.number,
            first_lba: partition.first_lba,
            last_lba: partition.last_lba,
            delta_lba: delta,
            bytes: delta.saturating_mul(sector_size),
            swap: false,
        })
        .collect();
    Ok(GrowPreview {
        partition: target.number,
        new_end_lba,
        sector_size,
        moves,
    })
}
