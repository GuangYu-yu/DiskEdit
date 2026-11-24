//! `resizefs`：把分区内的文件系统调整到指定字节数。
//!
//! 省略字节数表示撑满分区。目标大于当前文件系统大小时走扩容；小于时走收缩，
//! 且必须已观测到最小尺寸并证明目标不小于它。目标大于分区容量直接拒绝。挂载点
//! 形态（没有分区选择器）需要按挂载点反查分区，当前端口未提供该事实，明确拒绝。

use diskedit_core::evidence::Observation;
use diskedit_core::fs::{FsKind, Verb};

use super::common::{admission, capacity};
use super::plan::Plan;
use super::refusal::Refusal;
use super::step::{Step, StepKind};
use crate::app::observe::{PartitionSelector, Snapshot};

pub fn plan(
    snapshot: &Snapshot,
    selector: Option<PartitionSelector>,
    bytes: Option<u64>,
) -> Result<Plan, Refusal> {
    let admitted = admission::admit(snapshot)?;
    let Some(selector) = selector else {
        return Err(Refusal::ContentUnsupported {
            what: "挂载点形态的 resizefs".to_owned(),
            why: "按挂载点反查分区的端口尚未接入".to_owned(),
        });
    };
    let partition = admission::select(snapshot, selector)?;
    let facts = admission::partition(snapshot, partition)?;
    let capacity_bytes = facts
        .partition
        .sectors
        .checked_mul(admitted.sector)
        .ok_or_else(|| Refusal::GeometryUnknown {
            why: "分区容量越过可寻址上界".to_owned(),
        })?;
    let bytes = bytes.unwrap_or(capacity_bytes);
    if bytes == 0 {
        return Err(Refusal::ArgumentOutOfRange {
            what: "目标大小为 0".to_owned(),
        });
    }
    let content = admission::content(facts)?;
    let filesystem: FsKind = content
        .filesystem()
        .ok_or_else(|| Refusal::ContentUnsupported {
            what: format!("{} 调整大小", content.as_str()),
            why: "该内容不是文件系统".to_owned(),
        })?;
    if bytes > capacity_bytes {
        return Err(Refusal::CapacityInsufficient {
            needed: bytes,
            available: capacity_bytes,
        });
    }
    let observed = match &facts.size {
        Observation::Known { value, .. } => *value,
        Observation::Unknown { why } => {
            return Err(Refusal::MinimumSizeUnknown {
                number: partition,
                why: why.clone(),
            });
        }
        Observation::NotApplicable { why } => {
            return Err(Refusal::MinimumSizeUnknown {
                number: partition,
                why: why.clone(),
            });
        }
    };
    let verb = if bytes < observed.bytes {
        Verb::Shrink
    } else {
        Verb::Grow
    };
    let programs =
        admission::programs(content, verb)?.ok_or_else(|| Refusal::ContentUnsupported {
            what: format!("{} {}", content.as_str(), verb_name(verb)),
            why: "该内容不支持该方向的大小调整".to_owned(),
        })?;
    if matches!(verb, Verb::Shrink) {
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
    }
    admission::require_tools(snapshot, programs)?;
    let region = capacity::partition_region(
        facts.partition.start_lba,
        facts.partition.sectors,
        admitted.sector,
    )?;
    let kind = match verb {
        Verb::Shrink => StepKind::ShrinkFilesystem {
            partition,
            filesystem,
            bytes,
        },
        Verb::Grow | Verb::Attrs | Verb::Check | Verb::Mkfs => StepKind::GrowFilesystem {
            partition,
            filesystem,
            bytes,
        },
    };
    Plan::write(vec![Step::new(kind, vec![region])], admitted.identity)
}

const fn verb_name(verb: Verb) -> &'static str {
    match verb {
        Verb::Grow => "扩容",
        Verb::Shrink => "收缩",
        Verb::Attrs => "属性",
        Verb::Check => "检查",
        Verb::Mkfs => "建卷",
    }
}
