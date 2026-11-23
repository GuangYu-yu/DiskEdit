//! `check`：按内容档案调用一致性检查。
//!
//! 内容未知时拒绝，不把"没查成"当作"没问题"；不支持检查的类别给出固定原因。

use diskedit_core::fs::Verb;

use super::common::admission;
use super::common::capacity;
use super::plan::Plan;
use super::refusal::Refusal;
use super::step::{Step, StepKind};
use crate::app::observe::Snapshot;

pub fn plan(snapshot: &Snapshot) -> Result<Plan, Refusal> {
    let sector = admission::sector(snapshot)?;
    let partitions = snapshot.contents().ok_or_else(|| Refusal::FactUnknown {
        what: "partition topology".to_owned(),
    })?;
    let mut steps = Vec::new();
    for facts in partitions {
        let content = admission::content(facts)?;
        let programs = admission::programs(content, Verb::Check)?;
        let Some(programs) = programs else {
            continue;
        };
        admission::require_tools(snapshot, programs)?;
        let region =
            capacity::partition_region(facts.partition.start_lba, facts.partition.sectors, sector)?;
        steps.push(Step::new(
            StepKind::Check {
                partition: facts.partition.number,
            },
            vec![region],
        ));
    }
    Ok(Plan::read_only(steps))
}
