//! `check`：按内容档案调用一致性检查。
//!
//! 内容未知时拒绝，不把"没查成"当作"没问题"；不支持检查的类别给出固定原因。

use diskedit_core::fs::Verb;

use super::common::{admission, capacity};
use super::plan::Plan;
use super::refusal::Refusal;
use super::step::{Step, StepKind};
use crate::app::observe::{PartitionSelector, Snapshot};

pub fn plan(snapshot: &Snapshot, selector: PartitionSelector) -> Result<Plan, Refusal> {
    let sector = admission::sector(snapshot)?;
    let partition = admission::select(snapshot, selector)?;
    let facts = admission::partition(snapshot, partition)?;
    let content = admission::content(facts)?;
    let programs = admission::programs(content, Verb::Check)?;
    let Some(programs) = programs else {
        return Err(Refusal::ContentUnsupported {
            what: format!("{} 一致性检查", content.as_str()),
            why: "该内容没有可用的检查器".to_owned(),
        });
    };
    admission::require_tools(snapshot, programs)?;
    let region =
        capacity::partition_region(facts.partition.start_lba, facts.partition.sectors, sector)?;
    Ok(Plan::read_only(vec![Step::new(
        StepKind::Check { partition },
        vec![region],
    )]))
}
