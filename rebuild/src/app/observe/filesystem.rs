//! 文件系统命令的现场：目标、表视图、记录、占用与所选分区的事实。
//!
//! 外部程序能否运行是宿主事实，与分区内容一起一次采齐；判定层据此给出
//! `ToolMissing`，而不是等执行时才发现。这里不决定任何做法，只如实记录。

use diskedit_core::fs::{ContentKind, FsKind, Program, Verb, catalog};

use crate::platform::{Occupancy, Platform};

use super::new::{PriorState, prior_state};
use super::records::RecordState;
use super::snapshot::{CaptureError, TargetFacts, open_target, record_state};
use super::table::{self, PartitionView, TableView};
use super::{Command, PartitionSelector, Request};

/// 所选分区在目标上的事实。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PartitionFacts {
    pub number: u32,
    pub base_bytes: u64,
    pub bytes: u64,
    pub content: ContentKind,
}

/// 文件系统命令消费的现场。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FilesystemSnapshot {
    pub target: TargetFacts,
    pub view: TableView,
    pub record: RecordState,
    pub occupancy: Occupancy,
    pub prior: PriorState,
    pub is_partition_node: bool,
    /// 用户给出的分区选择器；整目标形态为 `None`。
    pub selector: Option<PartitionSelector>,
    /// 选择器在表里的落点；选择器为空或落点不存在时为 `None`。
    pub selected: Option<PartitionFacts>,
    /// 所选能力需要的程序及其可用性，按执行顺序。
    pub tools: Vec<(Program, bool)>,
}

/// 采集文件系统命令的现场。
pub fn capture(
    platform: &dyn Platform,
    request: &Request,
) -> Result<FilesystemSnapshot, CaptureError> {
    let target = open_target(platform, request)?;
    let handle = platform.open_target_read_only(&target.path)?;
    let view = table::observe(handle.as_ref(), request.sector_size(), handle.sector_size())
        .map_err(|error| CaptureError::Unreadable {
            why: error.to_string(),
        })?;
    let record = record_state(platform, &target)?;
    let occupancy = platform.occupancy(&target.path)?;
    let is_partition_node = platform.is_partition_node(&target.path);
    let prior = prior_state(&record);
    let sector_size = handle.sector_size().unwrap_or(target.sector_size);
    let selector = selector_of(request.command());
    let selected = selector
        .and_then(|selector| resolve(&view, selector))
        .map(PartitionFacts::from);
    let mut tools: Vec<(Program, bool)> = required_programs(request.command(), selected.as_ref())
        .into_iter()
        .map(|program| (program, platform.tool_available(program)))
        .collect();
    // 离线扩容经临时挂载的家族还要证明挂载与卸载程序可用。
    if matches!(request.command(), Command::Resizefs { .. }) {
        if let Some(partition) = &selected {
            if matches!(
                partition.content,
                ContentKind::Filesystem(FsKind::Xfs) | ContentKind::Filesystem(FsKind::Btrfs)
            ) {
                for program in [Program::Mount, Program::Umount] {
                    if !tools.iter().any(|(known, _)| *known == program) {
                        tools.push((program, platform.tool_available(program)));
                    }
                }
            }
        }
    }
    Ok(FilesystemSnapshot {
        target: TargetFacts {
            sector_size,
            ..target
        },
        view,
        record,
        occupancy,
        prior,
        is_partition_node,
        selector,
        selected,
        tools,
    })
}

impl From<&PartitionView> for PartitionFacts {
    fn from(partition: &PartitionView) -> Self {
        Self {
            number: partition.number,
            base_bytes: partition.base_bytes,
            bytes: partition.bytes,
            content: partition.content,
        }
    }
}

fn resolve(view: &TableView, selector: PartitionSelector) -> Option<&PartitionView> {
    match selector {
        PartitionSelector::Number(number) => view.find(number),
        PartitionSelector::Last => view.last(),
    }
}

/// 命令携带的分区选择器；只有分区目标形态才有。
fn selector_of(command: &Command) -> Option<PartitionSelector> {
    match command {
        Command::Mkfs { partition, .. } => *partition,
        Command::Delete { partition } | Command::Check { partition } => Some(*partition),
        Command::Resizefs { partition, .. } => *partition,
        Command::Info { .. }
        | Command::Inspect
        | Command::RecordsList
        | Command::RecordsVerify { .. }
        | Command::RecordsScan
        | Command::Resize(_)
        | Command::Move(_)
        | Command::Copy(_)
        | Command::Set(_)
        | Command::Recover(_)
        | Command::New { .. }
        | Command::Add(_) => None,
    }
}

/// 该命令所选能力需要的程序；能力不支持时为空。
fn required_programs(command: &Command, selected: Option<&PartitionFacts>) -> Vec<Program> {
    match command {
        Command::Mkfs { filesystem, .. } => {
            programs(catalog::profile(filesystem.content()).support(Verb::Mkfs))
        }
        Command::Resizefs { .. } => selected
            .map(|partition| programs(catalog::profile(partition.content).support(Verb::Grow)))
            .unwrap_or_default(),
        Command::Info { .. }
        | Command::Inspect
        | Command::RecordsList
        | Command::RecordsVerify { .. }
        | Command::RecordsScan
        | Command::Resize(_)
        | Command::Move(_)
        | Command::Copy(_)
        | Command::Delete { .. }
        | Command::Set(_)
        | Command::Check { .. }
        | Command::Recover(_)
        | Command::New { .. }
        | Command::Add(_) => Vec::new(),
    }
}

fn programs(support: catalog::Support) -> Vec<Program> {
    support.tools().map(<[Program]>::to_vec).unwrap_or_default()
}
