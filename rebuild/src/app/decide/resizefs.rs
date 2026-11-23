//! `resizefs`：把分区内的文件系统调整到指定字节数。
//!
//! 省略字节数表示撑满分区。目标等于分区容量时是确定的扩容；更小的目标需要先
//! 知道当前文件系统大小与最小尺寸，快照未提供时按未知拒绝。挂载点形态（没有
//! 分区选择器）需要按挂载点反查分区，当前端口未提供该事实，明确拒绝。

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
    if bytes != capacity_bytes {
        return Err(Refusal::MinimumSizeUnknown {
            number: partition,
            why: "当前文件系统大小与最小尺寸未观测".to_owned(),
        });
    }
    let programs = admission::programs(content, Verb::Grow)?;
    let Some(programs) = programs else {
        return Err(Refusal::ContentUnsupported {
            what: format!("{} 扩容", content.as_str()),
            why: "该内容不支持原地扩容".to_owned(),
        });
    };
    admission::require_tools(snapshot, programs)?;
    let region = capacity::partition_region(
        facts.partition.start_lba,
        facts.partition.sectors,
        admitted.sector,
    )?;
    let steps = vec![Step::new(
        StepKind::GrowFilesystem {
            partition,
            filesystem,
            bytes,
        },
        vec![region],
    )];
    Plan::write(steps, admitted.identity)
}
