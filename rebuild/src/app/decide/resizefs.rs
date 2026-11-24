//! `resizefs`：把分区内的文件系统撑满所在分区。
//!
//! 离线形态只接受分区选择器，不接受尺寸参数——尺寸是挂载点在线形态的给法，离线形态
//! 的目的就是让文件系统吃掉分区里的剩余空间。挂载点形态（没有分区选择器）需要按挂载
//! 点反查分区，当前端口未提供该事实，明确拒绝。

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
) -> Result<Plan, Refusal> {
    let admitted = admission::admit(snapshot)?;
    let Some(selector) = selector else {
        return Err(Refusal::ContentUnsupported {
            what: "挂载点形态的 resizefs".to_owned(),
            why: "按挂载点反查分区的端口尚未接入".to_owned(),
        });
    };
    // 离线形态没有尺寸参数；保留这条判据，供绕过命令行直接构造请求的调用方一并拒绝。
    if bytes.is_some() {
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
