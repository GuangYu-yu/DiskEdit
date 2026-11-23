//! `resize`：改变分区在表里的起止。
//!
//! 三种形态：绝对尺寸、`--end` 终点、`--grow-to-end` 撑到末尾。收缩需要先证明
//! 文件系统已缩到新范围；当前快照不含该事实，收缩按未知拒绝，不把"没读到"当作
//! "已经缩好"。

use super::common::{admission, capacity};
use super::plan::Plan;
use super::refusal::Refusal;
use super::step::{Step, StepKind};
use crate::app::observe::{ResizeRequest, Snapshot, TableFormat};

pub fn plan(snapshot: &Snapshot, request: &ResizeRequest) -> Result<Plan, Refusal> {
    let admitted = admission::admit(snapshot)?;
    let Some(selector) = request.partition else {
        return Err(Refusal::ContentUnsupported {
            what: "未指定分区的 resize".to_owned(),
            why: "resize 需要分区选择器 :N 或 :last".to_owned(),
        });
    };
    if request.grow_lv || request.lv.is_some() {
        return Err(Refusal::ContentUnsupported {
            what: "LVM 扩容".to_owned(),
            why: "逻辑卷扩容尚未接入执行器".to_owned(),
        });
    }
    let partition = admission::select(snapshot, selector)?;
    let facts = admission::partition(snapshot, partition)?;
    let table = admission::table(snapshot)?;
    let format = table.format();
    let start = facts.partition.start_lba;
    let sectors = match (request.size, request.end, request.grow_to_end) {
        (Some(_), Some(_), _) | (Some(_), _, true) | (None, Some(_), true) => {
            return Err(Refusal::ArgumentOutOfRange {
                what: "尺寸、--end 与 --grow-to-end 只能出现一个".to_owned(),
            });
        }
        (Some(size), None, false) => {
            let sectors = size.div_ceil(admitted.sector);
            if sectors == 0 {
                return Err(Refusal::ArgumentOutOfRange {
                    what: "分区长度为零".to_owned(),
                });
            }
            sectors
        }
        (None, Some(end), false) => end
            .checked_sub(start)
            .and_then(|span| span.checked_add(1))
            .ok_or_else(|| Refusal::ArgumentOutOfRange {
                what: "--end 早于分区起点".to_owned(),
            })?,
        (None, None, true) => match table {
            admission::TableView::Gpt(copy) => copy
                .header
                .last_usable_lba
                .checked_sub(start)
                .and_then(|span| span.checked_add(1))
                .ok_or(Refusal::CapacityInsufficient {
                    needed: 1,
                    available: 0,
                })?,
            admission::TableView::Mbr(_) => {
                return Err(Refusal::ContentUnsupported {
                    what: "MBR 撑到末尾".to_owned(),
                    why: "MBR 没有可用的末尾边界".to_owned(),
                });
            }
        },
        (None, None, false) => {
            return Err(Refusal::ArgumentOutOfRange {
                what: "resize 缺少尺寸、--end 或 --grow-to-end".to_owned(),
            });
        }
    };
    if request.align.is_some() && !request.grow_to_end && request.end.is_none() {
        return Err(Refusal::ArgumentOutOfRange {
            what: "--align 只用于 --end 与 --grow-to-end 形态".to_owned(),
        });
    }
    if sectors < facts.partition.sectors {
        return Err(Refusal::FactUnknown {
            what: "filesystem used bytes".to_owned(),
        });
    }
    let end_lba = start
        .checked_add(sectors - 1)
        .ok_or_else(|| Refusal::ArgumentOutOfRange {
            what: "分区终点越过可寻址上界".to_owned(),
        })?;
    match table {
        admission::TableView::Gpt(copy) => {
            if start < copy.header.first_usable_lba || end_lba > copy.header.last_usable_lba {
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
                if number == partition {
                    continue;
                }
                if admission::overlaps(
                    start,
                    sectors,
                    entry.starting_lba,
                    entry.ending_lba - entry.starting_lba + 1,
                ) {
                    return Err(Refusal::PartitionOverlap { number });
                }
            }
        }
        admission::TableView::Mbr(mbr) => {
            if mbr.extended().is_some() {
                return Err(Refusal::ContentUnsupported {
                    what: "MBR 扩展分区".to_owned(),
                    why: "含扩展分区的目标不参与调整".to_owned(),
                });
            }
            u32::try_from(start).map_err(|_| Refusal::ArgumentOutOfRange {
                what: "MBR 起点超过 32 位".to_owned(),
            })?;
            u32::try_from(sectors).map_err(|_| Refusal::ArgumentOutOfRange {
                what: "MBR 长度超过 32 位".to_owned(),
            })?;
            for (number, entry) in admission::mbr_used(mbr) {
                if number == partition {
                    continue;
                }
                if admission::overlaps(
                    start,
                    sectors,
                    u64::from(entry.start_lba),
                    u64::from(entry.size_lba),
                ) {
                    return Err(Refusal::PartitionOverlap { number });
                }
            }
        }
    }
    let layout = match format {
        TableFormat::Gpt => Some(capacity::gpt_layout(admitted.length, admitted.sector)?),
        TableFormat::Mbr => None,
    };
    let regions = capacity::table_regions(format, layout, admitted.sector)?;
    let steps = vec![
        Step::new(
            StepKind::ResizePartition {
                format,
                partition,
                start_lba: start,
                sectors,
            },
            regions,
        ),
        Step::new(StepKind::RefreshKernelView, Vec::new()),
    ];
    Plan::write(steps, admitted.identity)
}
