//! `mkfs`：在指定分区上建卷。
//!
//! 建卷不依赖分区里现有内容；所需程序由内容档案给出。标签随步骤一起进入计划，
//! 由执行侧的建卷动作使用。

use diskedit_core::fs::profile::ContentKind;
use diskedit_core::fs::{FsKind, Verb};

use super::common::{admission, capacity};
use super::plan::Plan;
use super::refusal::Refusal;
use super::step::{Step, StepKind};
use crate::app::observe::{PartitionSelector, Snapshot};

pub fn plan(
    snapshot: &Snapshot,
    selector: PartitionSelector,
    filesystem: FsKind,
    label: Option<String>,
) -> Result<Plan, Refusal> {
    let admitted = admission::admit(snapshot)?;
    let partition = admission::select(snapshot, selector)?;
    let facts = admission::partition(snapshot, partition)?;
    let content = ContentKind::Filesystem(filesystem);
    let programs = admission::programs(content, Verb::Mkfs)?;
    let Some(programs) = programs else {
        return Err(Refusal::ContentUnsupported {
            what: format!("{} 建卷", content.as_str()),
            why: "该内容不由建卷产生".to_owned(),
        });
    };
    admission::require_tools(snapshot, programs)?;
    let region = capacity::partition_region(
        facts.partition.start_lba,
        facts.partition.sectors,
        admitted.sector,
    )?;
    let steps = vec![Step::new(
        StepKind::MakeFilesystem {
            partition,
            filesystem,
            label,
        },
        vec![region],
    )];
    Plan::write(steps, admitted.identity)
}
