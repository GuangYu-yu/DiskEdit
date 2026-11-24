//! `resize`：改变分区在表里的起止。
//!
//! 三种形态：绝对尺寸、`--end` 终点、`--grow-to-end` 撑到末尾。与基线一致，改完
//! 分区几何后把文件系统调到新的区间：扩容在表提交与内核重读之后做，收缩在新旧
//! 几何之间先缩文件系统再缩分区；`--no-fs` 只改几何。收缩要求观测到文件系统当前
//! 大小与可收缩的最小尺寸，不把"没读到"当作"已经缩好"。

use diskedit_core::evidence::Observation;
use diskedit_core::fs::{FsKind, Verb};

use super::common::{admission, capacity};
use super::plan::Plan;
use super::refusal::Refusal;
use super::step::{Step, StepKind};
use crate::app::observe::{ResizeRequest, SizeSpec, Snapshot, TableFormat};

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
    let grow_to_end = request.grow_to_end || matches!(request.size, Some(SizeSpec::Grow));
    let size = request.size.filter(|spec| !matches!(spec, SizeSpec::Grow));
    let current_bytes = facts
        .partition
        .sectors
        .checked_mul(admitted.sector)
        .ok_or_else(|| Refusal::ArgumentOutOfRange {
            what: "分区容量越过可寻址上界".to_owned(),
        })?;
    let sectors = match (size, request.end, grow_to_end) {
        (Some(_), Some(_), _) | (Some(_), _, true) | (None, Some(_), true) => {
            return Err(Refusal::ArgumentOutOfRange {
                what: "尺寸、--end 与 --grow-to-end 只能出现一个".to_owned(),
            });
        }
        (Some(spec), None, false) => {
            let target = resolve_size(spec, current_bytes)?;
            let sectors = target.div_ceil(admitted.sector);
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
    if request.no_fs && sectors < facts.partition.sectors {
        let needed =
            sectors
                .checked_mul(admitted.sector)
                .ok_or_else(|| Refusal::ArgumentOutOfRange {
                    what: "新分区容量越过可寻址上界".to_owned(),
                })?;
        match &facts.size {
            Observation::Known { value, .. } if value.bytes <= needed => {}
            Observation::Known { value, .. } => {
                return Err(Refusal::CapacityInsufficient {
                    needed: value.bytes,
                    available: needed,
                });
            }
            Observation::Unknown { .. } | Observation::NotApplicable { .. } => {
                return Err(Refusal::FactUnknown {
                    what: "filesystem used bytes".to_owned(),
                });
            }
        }
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
    let new_bytes =
        sectors
            .checked_mul(admitted.sector)
            .ok_or_else(|| Refusal::ArgumentOutOfRange {
                what: "新分区容量越过可寻址上界".to_owned(),
            })?;
    // 几何不变时不碰文件系统：没有方向，调用工具只会白写一遍。
    let filesystem = if request.no_fs || sectors == facts.partition.sectors {
        None
    } else {
        filesystem_step(snapshot, facts, partition, new_bytes)?
    };
    let mut steps = Vec::new();
    if let Some(Coupling::ShrinkFilesystem { filesystem, bytes }) = filesystem {
        let old_region = capacity::partition_region(
            facts.partition.start_lba,
            facts.partition.sectors,
            admitted.sector,
        )?;
        steps.push(Step::new(
            StepKind::ShrinkFilesystem {
                partition,
                filesystem,
                bytes,
            },
            vec![old_region],
        ));
    }
    steps.push(Step::new(
        StepKind::ResizePartition {
            format,
            partition,
            start_lba: start,
            sectors,
        },
        regions,
    ));
    steps.push(Step::new(StepKind::RefreshKernelView, Vec::new()));
    if let Some(Coupling::GrowFilesystem { filesystem, bytes }) = filesystem {
        let new_region = capacity::partition_region(start, sectors, admitted.sector)?;
        steps.push(Step::new(
            StepKind::GrowFilesystem {
                partition,
                filesystem,
                bytes,
            },
            vec![new_region],
        ));
    }
    Plan::write(steps, admitted.identity)
}

/// 文件系统要跟着走的那一步。
enum Coupling {
    GrowFilesystem { filesystem: FsKind, bytes: u64 },
    ShrinkFilesystem { filesystem: FsKind, bytes: u64 },
}

/// 按新容量与当前文件系统大小决定是否、以及往哪边调整文件系统。
fn filesystem_step(
    snapshot: &Snapshot,
    facts: &crate::app::observe::snapshot::PartitionFacts,
    partition: u32,
    bytes: u64,
) -> Result<Option<Coupling>, Refusal> {
    let content = match &facts.content {
        Observation::Known { value, .. } => *value,
        Observation::Unknown { .. } | Observation::NotApplicable { .. } => {
            return Err(Refusal::ContentUnsupported {
                what: "未知内容的分区调整".to_owned(),
                why: "无法识别的文件系统；只改几何请加 --no-fs".to_owned(),
            });
        }
    };
    let filesystem = content
        .filesystem()
        .ok_or_else(|| Refusal::ContentUnsupported {
            what: format!("{} 跟随分区调整", content.as_str()),
            why: "该内容不是文件系统；只改几何请加 --no-fs".to_owned(),
        })?;
    let observed = match &facts.size {
        Observation::Known { value, .. } => *value,
        Observation::Unknown { why } | Observation::NotApplicable { why } => {
            return Err(Refusal::MinimumSizeUnknown {
                number: partition,
                why: why.clone(),
            });
        }
    };
    match bytes.cmp(&observed.bytes) {
        std::cmp::Ordering::Equal => Ok(None),
        std::cmp::Ordering::Greater => {
            let programs = admission::programs(content, Verb::Grow)?.ok_or_else(|| {
                Refusal::ContentUnsupported {
                    what: format!("{} 扩容", content.as_str()),
                    why: "该内容不支持扩容".to_owned(),
                }
            })?;
            admission::require_tools(snapshot, programs)?;
            Ok(Some(Coupling::GrowFilesystem { filesystem, bytes }))
        }
        std::cmp::Ordering::Less => {
            let programs = admission::programs(content, Verb::Shrink)?.ok_or_else(|| {
                Refusal::ContentUnsupported {
                    what: format!("{} 收缩", content.as_str()),
                    why: "该内容不支持收缩".to_owned(),
                }
            })?;
            let minimum = observed
                .min_bytes
                .ok_or_else(|| Refusal::MinimumSizeUnknown {
                    number: partition,
                    why: "该文件系统未给出可收缩的最小尺寸".to_owned(),
                })?;
            if bytes < minimum {
                return Err(Refusal::BelowMinimumSize {
                    number: partition,
                    minimum,
                    requested: bytes,
                });
            }
            admission::require_tools(snapshot, programs)?;
            Ok(Some(Coupling::ShrinkFilesystem { filesystem, bytes }))
        }
    }
}

/// SIZE 写法 → 目标字节数；增量与百分比以当前分区字节数为锚。
fn resolve_size(spec: SizeSpec, current: u64) -> Result<u64, Refusal> {
    match spec {
        SizeSpec::Absolute(bytes) => Ok(bytes),
        SizeSpec::Grow => Err(Refusal::ArgumentOutOfRange {
            what: "grow 不能作为绝对尺寸解析".to_owned(),
        }),
        SizeSpec::Delta { grow: true, bytes } => {
            current
                .checked_add(bytes)
                .ok_or_else(|| Refusal::ArgumentOutOfRange {
                    what: "SIZE 超过可寻址上界".to_owned(),
                })
        }
        SizeSpec::Delta { grow: false, bytes } => {
            current
                .checked_sub(bytes)
                .ok_or_else(|| Refusal::ArgumentOutOfRange {
                    what: format!("SIZE 超过当前分区大小 {current}"),
                })
        }
        SizeSpec::Percent { grow, num, denom } => {
            const MIB: u64 = 1 << 20;
            let raw = u128::from(current)
                .checked_mul(u128::from(num))
                .and_then(|scaled| scaled.checked_div(100u128 * u128::from(denom)))
                .filter(|value| *value <= u128::from(u64::MAX))
                .ok_or_else(|| Refusal::ArgumentOutOfRange {
                    what: "SIZE 超过可寻址上界".to_owned(),
                })? as u64;
            let delta = raw / MIB * MIB;
            if grow {
                current
                    .checked_add(delta)
                    .ok_or_else(|| Refusal::ArgumentOutOfRange {
                        what: "SIZE 超过可寻址上界".to_owned(),
                    })
            } else {
                current
                    .checked_sub(delta)
                    .ok_or_else(|| Refusal::ArgumentOutOfRange {
                        what: format!("SIZE 超过当前分区大小 {current}"),
                    })
            }
        }
    }
}
