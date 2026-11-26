//! `resize` 的现场：可编辑表事实、所选分区几何、占用、记录与所需外部程序的一次采集。
//!
//! 判定层需要"当前表长什么样""所选分区占哪几个 LBA""分区里是什么内容""哪些工具
//! 可用"，这些事实在锁前采一次；判定只消费它们，不再读盘。

use diskedit_core::fs::{ContentKind, Program, Support, Verb, catalog};

use crate::platform::{Occupancy, Platform};

use super::edit::{self, TableFacts};
use super::new::{PriorState, prior_state, stamp_of};
use super::records::RecordState;
use super::snapshot::{CaptureError, TargetFacts, open_target, record_state};
use super::table::{self, PartitionView, TableView};
use super::{Command, PartitionSelector, Request};

/// `resize` 消费的现场。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResizeSnapshot {
    pub target: TargetFacts,
    pub occupancy: Occupancy,
    pub record: RecordState,
    pub prior: PriorState,
    pub view: TableView,
    /// 表不可编辑时为 `None`；此时 `view` 说明原因。
    pub facts: Option<TableFacts>,
    /// 无分区表（superfloppy）时整盘内容；有表时为 `None`。
    pub whole_content: Option<ContentKind>,
    pub stamp: [u8; 32],
    pub is_partition_node: bool,
    pub selector: Option<PartitionSelector>,
    /// 选择器在表里的落点。
    pub selected: Option<PartitionView>,
    /// 所选内容在扩容与收缩两个方向上需要的程序及其可用性。
    pub tools: Vec<(Program, bool)>,
}

/// 采集一次 `resize` 现场。
pub fn capture_resize(
    platform: &dyn Platform,
    request: &Request,
) -> Result<ResizeSnapshot, CaptureError> {
    let Command::Resize(resize) = request.command() else {
        return Err(CaptureError::Unreadable {
            why: "capture_resize called for a different command".to_owned(),
        });
    };
    let target = open_target(platform, request)?;
    let handle = platform.open_target_read_only(&target.path)?;
    let view = table::observe(handle.as_ref(), request.sector_size(), handle.sector_size())
        .map_err(|error| CaptureError::Unreadable {
            why: error.to_string(),
        })?;
    let record = record_state(platform, &target)?;
    let occupancy = platform.occupancy(&target.path)?;
    let is_partition_node = platform.is_partition_node(&target.path);
    let sector_size = view.sector_size().unwrap_or(target.sector_size);
    let stamp = stamp_of(handle.as_ref(), target.bytes, sector_size)?;
    let facts = match &view {
        TableView::Gpt(_) => Some(edit::capture_gpt(
            handle.as_ref(),
            target.bytes,
            sector_size,
        )?),
        TableView::Mbr(_) => Some(edit::capture_mbr(
            handle.as_ref(),
            target.bytes,
            sector_size,
        )?),
        TableView::Damaged { .. } | TableView::None => None,
    };
    let whole_content = match &view {
        TableView::None => Some(crate::platform::probe::identify(
            handle.as_ref(),
            0,
            target.bytes,
        )?),
        TableView::Gpt(_) | TableView::Mbr(_) | TableView::Damaged { .. } => None,
    };
    let prior = prior_state(&record);
    let selector = resize.partition;
    let selected = selector
        .and_then(|selector| resolve(&view, selector))
        .cloned();
    let tools = match (&selected, whole_content) {
        (Some(partition), _) => programs_for(partition.content),
        (None, Some(content)) => programs_for(content),
        (None, None) => Vec::new(),
    }
    .into_iter()
    .map(|program| (program, platform.tool_available(program)))
    .collect();
    Ok(ResizeSnapshot {
        target: TargetFacts {
            sector_size,
            ..target
        },
        occupancy,
        record,
        prior,
        view,
        facts,
        whole_content,
        stamp,
        is_partition_node,
        selector,
        selected,
        tools,
    })
}

fn resolve(view: &TableView, selector: PartitionSelector) -> Option<&PartitionView> {
    match selector {
        PartitionSelector::Number(number) => view.find(number),
        PartitionSelector::Last => view.last(),
    }
}

fn programs_for(content: ContentKind) -> Vec<Program> {
    let profile = catalog::profile(content);
    let mut programs: Vec<Program> = Vec::new();
    for verb in [Verb::Grow, Verb::Shrink] {
        let Support::Tools(tools) = profile.support(verb) else {
            continue;
        };
        for program in tools {
            if !programs.contains(program) {
                programs.push(*program);
            }
        }
    }
    programs
}
