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
            what: "resize without a partition".to_owned(),
            why: "resize requires a partition selector :N or :last".to_owned(),
        });
    };
    if request.grow_lv || request.lv.is_some() {
        return Err(Refusal::ContentUnsupported {
            what: "LVM grow".to_owned(),
            why: "logical volume grow is not yet wired to an executor".to_owned(),
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
            what: "partition capacity exceeds the addressable bound".to_owned(),
        })?;
    let sectors = match (size, request.end, grow_to_end) {
        (Some(_), Some(_), _) | (Some(_), _, true) | (None, Some(_), true) => {
            return Err(Refusal::ArgumentOutOfRange {
                what: "only one of size, --end, and --grow-to-end may be given".to_owned(),
            });
        }
        (Some(spec), None, false) => {
            let target = resolve_size(spec, current_bytes)?;
            let sectors = target.div_ceil(admitted.sector);
            if sectors == 0 {
                return Err(Refusal::ArgumentOutOfRange {
                    what: "partition length is zero".to_owned(),
                });
            }
            sectors
        }
        (None, Some(end), false) => end
            .checked_sub(start)
            .and_then(|span| span.checked_add(1))
            .ok_or_else(|| Refusal::ArgumentOutOfRange {
                what: "--end precedes the partition start".to_owned(),
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
                    what: "MBR grow to end".to_owned(),
                    why: "MBR has no usable end boundary".to_owned(),
                });
            }
        },
        (None, None, false) => {
            return Err(Refusal::ArgumentOutOfRange {
                what: "resize missing size, --end, or --grow-to-end".to_owned(),
            });
        }
    };
    if request.align.is_some() && !request.grow_to_end && request.end.is_none() {
        return Err(Refusal::ArgumentOutOfRange {
            what: "--align only applies to --end and --grow-to-end forms".to_owned(),
        });
    }
    if request.no_fs && sectors < facts.partition.sectors {
        let needed =
            sectors
                .checked_mul(admitted.sector)
                .ok_or_else(|| Refusal::ArgumentOutOfRange {
                    what: "new partition capacity exceeds the addressable bound".to_owned(),
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
            what: "partition end exceeds the addressable bound".to_owned(),
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
                    what: "MBR extended partition".to_owned(),
                    why: "targets with extended partitions are not supported for resize".to_owned(),
                });
            }
            u32::try_from(start).map_err(|_| Refusal::ArgumentOutOfRange {
                what: "MBR start exceeds 32 bits".to_owned(),
            })?;
            u32::try_from(sectors).map_err(|_| Refusal::ArgumentOutOfRange {
                what: "MBR length exceeds 32 bits".to_owned(),
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
                what: "new partition capacity exceeds the addressable bound".to_owned(),
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
                what: "partition resize of unknown content".to_owned(),
                why: "unrecognized filesystem; add --no-fs to change geometry only".to_owned(),
            });
        }
    };
    let filesystem = content
        .filesystem()
        .ok_or_else(|| Refusal::ContentUnsupported {
            what: format!("{} follow partition resize", content.as_str()),
            why: "this content is not a filesystem; add --no-fs to change geometry only".to_owned(),
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
                    what: format!("{} grow", content.as_str()),
                    why: "this content does not support growing".to_owned(),
                }
            })?;
            admission::require_tools(snapshot, programs)?;
            Ok(Some(Coupling::GrowFilesystem { filesystem, bytes }))
        }
        std::cmp::Ordering::Less => {
            let programs = admission::programs(content, Verb::Shrink)?.ok_or_else(|| {
                Refusal::ContentUnsupported {
                    what: format!("{} shrink", content.as_str()),
                    why: "this content does not support shrinking".to_owned(),
                }
            })?;
            let minimum = observed
                .min_bytes
                .ok_or_else(|| Refusal::MinimumSizeUnknown {
                    number: partition,
                    why: "this filesystem did not report a minimum shrinkable size".to_owned(),
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
            what: "grow cannot be resolved as an absolute size".to_owned(),
        }),
        SizeSpec::Delta { grow: true, bytes } => {
            current
                .checked_add(bytes)
                .ok_or_else(|| Refusal::ArgumentOutOfRange {
                    what: "SIZE exceeds the addressable bound".to_owned(),
                })
        }
        SizeSpec::Delta { grow: false, bytes } => {
            current
                .checked_sub(bytes)
                .ok_or_else(|| Refusal::ArgumentOutOfRange {
                    what: format!("SIZE exceeds the current partition size {current}"),
                })
        }
        SizeSpec::Percent { grow, num, denom } => {
            const MIB: u64 = 1 << 20;
            let raw = u128::from(current)
                .checked_mul(u128::from(num))
                .and_then(|scaled| scaled.checked_div(100u128 * u128::from(denom)))
                .filter(|value| *value <= u128::from(u64::MAX))
                .ok_or_else(|| Refusal::ArgumentOutOfRange {
                    what: "SIZE exceeds the addressable bound".to_owned(),
                })? as u64;
            let delta = raw / MIB * MIB;
            if grow {
                current
                    .checked_add(delta)
                    .ok_or_else(|| Refusal::ArgumentOutOfRange {
                        what: "SIZE exceeds the addressable bound".to_owned(),
                    })
            } else {
                current
                    .checked_sub(delta)
                    .ok_or_else(|| Refusal::ArgumentOutOfRange {
                        what: format!("SIZE exceeds the current partition size {current}"),
                    })
            }
        }
    }
}
