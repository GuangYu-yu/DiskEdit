//! `resizefs`：把分区内的文件系统撑满所在分区。
//!
//! 两种形态最后都汇成同一件事：把文件系统扩到分区容量。
//!
//! - 离线形态带分区选择器，直接对整盘上的该分区操作；
//! - 挂载点形态（没有分区选择器）先由平台把目录反查成整盘目标与分区号，改写请求后
//!   走同一条判定与执行路径。
//!
//! 尺寸参数只会出现在挂载点形态，且旧产品的在线语义只允许「撑满当前分区」：大于
//! 分区容量要先改表，小于分区容量是收缩，这两种都在这里明确拒绝，不假装成功。
//! `resizefs` 从不改分区表。

use diskedit_core::fs::Verb;

use super::common::{admission, capacity};
use super::plan::Plan;
use super::refusal::Refusal;
use super::step::{Step, StepKind};
use crate::app::observe::{PartitionSelector, Snapshot};

pub fn plan(
    snapshot: &Snapshot,
    selector: Option<PartitionSelector>,
    bytes: Option<u64>,
    mount: bool,
) -> Result<Plan, Refusal> {
    let admitted = admission::admit(snapshot)?;
    let Some(selector) = selector else {
        // 挂载点形态在命令行层就应被改写成分区目标；走到这里说明上游漏了反查。
        return Err(Refusal::ContentUnsupported {
            what: "挂载点形态的 resizefs".to_owned(),
            why: "请求尚未把挂载点反查成整盘目标与分区号".to_owned(),
        });
    };
    if !mount && bytes.is_some() {
        // 离线形态没有尺寸参数；保留这条判据，供绕过命令行直接构造请求的调用方一并拒绝。
        return Err(Refusal::ArgumentOutOfRange {
            what: "离线 resizefs 不接受尺寸参数".to_owned(),
        });
    }
    let partition = admission::select(snapshot, selector)?;
    let facts = admission::partition(snapshot, partition)?;
    let capacity_bytes = facts
        .partition
        .sectors
        .checked_mul(admitted.sector)
        .ok_or_else(|| Refusal::GeometryUnknown {
            why: "分区容量越过可寻址上界".to_owned(),
        })?;
    if capacity_bytes == 0 {
        return Err(Refusal::ArgumentOutOfRange {
            what: "分区长度为零".to_owned(),
        });
    }
    if let Some(requested) = bytes {
        if requested > capacity_bytes {
            return Err(Refusal::ArgumentOutOfRange {
                what: "请求尺寸超过分区容量".to_owned(),
            });
        }
        if requested < capacity_bytes {
            return Err(Refusal::ContentUnsupported {
                what: "在线 resizefs 收缩文件系统".to_owned(),
                why: "该形态只把文件系统撑满分区；缩小请改用 resize".to_owned(),
            });
        }
    }
    let content = admission::content(facts)?;
    let filesystem = content
        .filesystem()
        .ok_or_else(|| Refusal::ContentUnsupported {
            what: format!("{} 调整大小", content.as_str()),
            why: "该内容不是文件系统".to_owned(),
        })?;
    let programs =
        admission::programs(content, Verb::Grow)?.ok_or_else(|| Refusal::ContentUnsupported {
            what: format!("{} 扩容", content.as_str()),
            why: "该文件系统不支持扩容".to_owned(),
        })?;
    admission::require_tools(snapshot, programs)?;
    let region = capacity::partition_region(
        facts.partition.start_lba,
        facts.partition.sectors,
        admitted.sector,
    )?;
    Plan::write(
        vec![Step::new(
            StepKind::GrowFilesystem {
                partition,
                filesystem,
                bytes: capacity_bytes,
            },
            vec![region],
        )],
        admitted.identity,
    )
}
