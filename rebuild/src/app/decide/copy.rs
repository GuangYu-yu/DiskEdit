//! `copy`：把源分区内容复制到一个新起点，并在该处新增分区。
//!
//! 只有 GPT 目标参与；新位置必须放得下源分区内容且不与既有分区重叠。复制与新增
//! 分成两步：先写数据区，再把新条目落进分区表。

use super::common::{admission, capacity};
use super::plan::Plan;
use super::refusal::Refusal;
use super::step::{Step, StepKind};
use crate::app::observe::{CopyRequest, Snapshot, StartSpec, TableFormat};

/// 省略 `--chunk` 时的块大小。
pub const DEFAULT_CHUNK: u64 = 1 << 20;

pub fn plan(snapshot: &Snapshot, request: &CopyRequest) -> Result<Plan, Refusal> {
    let admitted = admission::admit(snapshot)?;
    let source = admission::select(snapshot, request.partition)?;
    let table = admission::table(snapshot)?;
    let copy = match table {
        admission::TableView::Gpt(copy) => copy,
        admission::TableView::Mbr(_) => {
            return Err(Refusal::ContentUnsupported {
                what: "MBR partition copy".to_owned(),
                why: "copy is only available for GPT targets".to_owned(),
            });
        }
    };
    let source_entry = admission::gpt_used(copy)
        .find(|(number, _)| *number == source)
        .map(|(_, entry)| entry)
        .ok_or(Refusal::PartitionMissing { number: source })?;
    let sectors = source_entry.ending_lba - source_entry.starting_lba + 1;
    let step = align_step(request.align, admitted.sector)?;
    let start_lba = placement(request.start, &copy.header, sectors, step)?;
    let end_lba =
        start_lba
            .checked_add(sectors - 1)
            .ok_or_else(|| Refusal::ArgumentOutOfRange {
                what: "copy end exceeds the addressable bound".to_owned(),
            })?;
    if start_lba < copy.header.first_usable_lba || end_lba > copy.header.last_usable_lba {
        return Err(Refusal::CapacityInsufficient {
            needed: sectors.saturating_mul(admitted.sector),
            available: (copy
                .header
                .last_usable_lba
                .saturating_sub(copy.header.first_usable_lba)
                + 1)
            .saturating_mul(admitted.sector),
        });
    }
    for (number, entry) in admission::gpt_used(copy) {
        if admission::overlaps(
            start_lba,
            sectors,
            entry.starting_lba,
            entry.ending_lba - entry.starting_lba + 1,
        ) {
            return Err(Refusal::PartitionOverlap { number });
        }
    }
    if admission::gpt_free_slot(copy).is_none() {
        return Err(Refusal::CapacityInsufficient {
            needed: 1,
            available: 0,
        });
    }
    let destination = capacity::partition_region(start_lba, sectors, admitted.sector)?;
    let name = request
        .name
        .clone()
        .unwrap_or_else(|| source_entry.name.to_string_lossy());
    let unique = if request.keep_partuuid {
        Some(source_entry.unique_partition_guid)
    } else {
        None
    };
    let layout = capacity::gpt_layout(admitted.length, admitted.sector)?;
    let table_regions = capacity::table_regions(TableFormat::Gpt, Some(layout), admitted.sector)?;
    let steps = vec![
        Step::new(
            StepKind::CopyPartition {
                source,
                start_lba,
                sectors,
                chunk: request.chunk.unwrap_or(DEFAULT_CHUNK).max(1),
            },
            vec![destination],
        ),
        Step::new(
            StepKind::AddPartition {
                format: TableFormat::Gpt,
                start_lba,
                sectors,
                type_guid: source_entry.partition_type_guid,
                name,
                part_number: None,
                unique,
            },
            table_regions,
        ),
        Step::new(StepKind::RefreshKernelView, Vec::new()),
    ];
    Plan::write(steps, admitted.identity)
}

/// 把 `--start` 与 `--align` 折算成具体起点。
fn placement(
    start: StartSpec,
    header: &diskedit_core::table::gpt::header::GptHeader,
    sectors: u64,
    step: u64,
) -> Result<u64, Refusal> {
    let raw = match start {
        StartSpec::Lba(lba) => lba,
        StartSpec::End => header
            .last_usable_lba
            .checked_sub(sectors - 1)
            .ok_or_else(|| Refusal::CapacityInsufficient {
                needed: sectors,
                available: header
                    .last_usable_lba
                    .saturating_sub(header.first_usable_lba)
                    + 1,
            })?,
        StartSpec::Align => header.first_usable_lba,
    };
    Ok(raw.div_ceil(step).saturating_mul(step))
}

fn align_step(align: Option<u64>, sector: u64) -> Result<u64, Refusal> {
    let bytes = align.unwrap_or(super::add::DEFAULT_ALIGN_BYTES);
    if bytes == 0 {
        return Err(Refusal::ArgumentOutOfRange {
            what: "alignment cannot be 0".to_owned(),
        });
    }
    Ok(bytes.div_ceil(sector).max(1))
}
