//! `move`：平移一个分区。
//!
//! 新起点由 `--start` 与 `--align` 折算；新区间必须落在可用范围内且不与其它
//! 分区重叠。计划声明旧区间与新区间两块写区。

use diskedit_core::evidence::Observation;
use diskedit_core::fs::{ContentKind, FsKind};

use super::common::{admission, capacity};
use super::plan::Plan;
use super::refusal::Refusal;
use super::step::{Step, StepKind};
use crate::app::observe::{MoveRequest, Snapshot, StartSpec, TableFormat};

/// 省略 `--chunk` 时的块大小。
pub const DEFAULT_CHUNK: u64 = 1 << 20;

pub fn plan(snapshot: &Snapshot, request: &MoveRequest) -> Result<Plan, Refusal> {
    let admitted = admission::admit(snapshot)?;
    let partition = admission::select(snapshot, request.partition)?;
    let facts = admission::partition(snapshot, partition)?;
    let table = admission::table(snapshot)?;
    let copy = match table {
        admission::TableView::Gpt(copy) => copy,
        admission::TableView::Mbr(_) => {
            return Err(Refusal::ContentUnsupported {
                what: "MBR 分区平移".to_owned(),
                why: "平移只对 GPT 目标开放".to_owned(),
            });
        }
    };
    let step = align_step(request.align, admitted.sector)?;
    let new_start = placement(request.start, &copy.header, facts.partition.sectors, step)?;
    if new_start == facts.partition.start_lba {
        return Err(Refusal::ArgumentOutOfRange {
            what: "平移后的起点与当前位置相同".to_owned(),
        });
    }
    let end_lba = new_start
        .checked_add(facts.partition.sectors - 1)
        .ok_or_else(|| Refusal::ArgumentOutOfRange {
            what: "平移后的终点越过可寻址上界".to_owned(),
        })?;
    if new_start < copy.header.first_usable_lba || end_lba > copy.header.last_usable_lba {
        return Err(Refusal::CapacityInsufficient {
            needed: facts.partition.sectors.saturating_mul(admitted.sector),
            available: (copy
                .header
                .last_usable_lba
                .saturating_sub(copy.header.first_usable_lba)
                + 1)
            .saturating_mul(admitted.sector),
        });
    }
    for (number, entry) in admission::gpt_used(copy) {
        if number == partition {
            continue;
        }
        if admission::overlaps(
            new_start,
            facts.partition.sectors,
            entry.starting_lba,
            entry.ending_lba - entry.starting_lba + 1,
        ) {
            return Err(Refusal::PartitionOverlap { number });
        }
    }
    let old = capacity::partition_region(
        facts.partition.start_lba,
        facts.partition.sectors,
        admitted.sector,
    )?;
    let new = capacity::partition_region(new_start, facts.partition.sectors, admitted.sector)?;
    let layout = capacity::gpt_layout(admitted.length, admitted.sector)?;
    let table_regions = capacity::table_regions(TableFormat::Gpt, Some(layout), admitted.sector)?;
    let offset_sectors = i64::try_from(
        i128::from(new_start) - i128::from(facts.partition.start_lba),
    )
    .map_err(|_| Refusal::ArgumentOutOfRange {
        what: "平移距离超出表示范围".to_owned(),
    })?;
    // 已识别的 NTFS 卷在平移后要修正引导扇区；内容未知时不做这个假设。
    let ntfs_hidden = matches!(
        &facts.content,
        Observation::Known {
            value: ContentKind::Filesystem(FsKind::Ntfs),
            ..
        }
    );
    let steps = vec![
        Step::new(
            StepKind::MovePartition {
                partition,
                offset_sectors,
                chunk: request.chunk.unwrap_or(DEFAULT_CHUNK).max(1),
                ntfs_hidden,
            },
            vec![old, new],
        ),
        Step::new(
            StepKind::ResizePartition {
                format: TableFormat::Gpt,
                partition,
                start_lba: new_start,
                sectors: facts.partition.sectors,
            },
            table_regions,
        ),
        Step::new(StepKind::RefreshKernelView, Vec::new()),
    ];
    Plan::write(steps, admitted.identity)
}

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
            what: "对齐不能为 0".to_owned(),
        });
    }
    Ok(bytes.div_ceil(sector).max(1))
}
