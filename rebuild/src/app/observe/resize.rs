//! `resize` 的现场：可编辑表事实、所选分区几何、占用、记录与所需外部程序的一次采集。
//!
//! 判定层需要"当前表长什么样""所选分区占哪几个 LBA""分区里是什么内容""哪些工具
//! 可用"，这些事实在锁前采一次；判定只消费它们，不再读盘。

use std::path::PathBuf;

use diskedit_core::fs::{ContentKind, Program, Support, Verb, catalog};

use crate::app::lvm::{self, LvSelection, LvmError};
use crate::platform::{MountFacts, Occupancy, PartitionMount, Platform, TargetKind};

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
    /// 所选分区是 LVM2 PV 时的只读现场；其他内容或 `--no-fs` 时为 `None`。
    pub lvm: Option<LvmFacts>,
    /// 所选分区正在本命名空间里挂载时的在线现场；离线形态为 `None`。
    pub online: Option<OnlineFacts>,
    /// 设备形态下所选分区在全部可读挂载命名空间里的使用情况。
    pub partition_mount: Option<PartitionMount>,
    /// 设备形态下所选分区是否是活动 swap。
    pub swap_active: Option<bool>,
}

/// 已挂载分区的在线几何调整现场。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OnlineFacts {
    /// 本命名空间里的挂载点；文件系统工具按它定位目标。
    pub mount_point: PathBuf,
    /// 分区设备节点；按节点寻址的文件系统工具用它。
    pub device: PathBuf,
    /// 分区所在的整盘；表写与内核视图调整都在它上面。
    pub disk: PathBuf,
    pub partition: u32,
}

/// LVM PV 链在判定前取到的只读事实。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LvmFacts {
    /// `--grow-lv` 时选定的目标 LV；未请求时为 `None`。
    pub selection: Option<Result<LvSelection, LvmError>>,
    /// 缩容方向必须让出的已分配末端字节数。
    pub allocated_end: Result<u64, LvmError>,
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
    if let Some(path) = request.target()
        && platform.is_directory(path)
    {
        return capture_mounted(platform, request, path);
    }
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
    let is_pv = selected
        .as_ref()
        .is_some_and(|partition| partition.content == ContentKind::Lvm2Pv);
    let occupancy = if is_pv && target.kind.is_block_device() {
        // 整盘占用看不到分区级的 holders（dm/LV 持有 PV 分区）；PV 的离线路径要能在
        // 仍被持有的分区上写表，必须先单独确认该分区空闲。
        let base = platform.occupancy_for_lvm(&target.path)?;
        match &selected {
            Some(partition) if matches!(base, Occupancy::Idle) => {
                let device = platform.partition_device(&target.path, partition.number)?;
                platform.occupancy(&device)?
            }
            _ => base,
        }
    } else if is_pv {
        platform.occupancy_for_lvm(&target.path)?
    } else {
        occupancy
    };
    // 设备形态：所选分区可能正被挂载或用作 swap。整盘占用看不到分区级使用，
    // 必须在锁前如实采集；读不出来就是采集失败，不折叠成"未挂载"。
    let (partition_mount, swap_active) = match &selected {
        Some(partition) if target.kind.is_block_device() => (
            Some(platform.partition_mount(&target.path, partition.number)?),
            Some(platform.swap_active(&target.path, partition.number)?),
        ),
        _ => (None, None),
    };
    let online = match (&partition_mount, &selected) {
        (Some(PartitionMount::Here { mount_point }), Some(partition)) if !is_pv => {
            let device = platform.partition_device(&target.path, partition.number)?;
            Some(OnlineFacts {
                mount_point: mount_point.clone(),
                device,
                disk: target.path.clone(),
                partition: partition.number,
            })
        }
        _ => None,
    };
    let mut programs = match (&selected, whole_content) {
        (Some(partition), _) => programs_for(partition.content),
        (None, Some(content)) => programs_for(content),
        (None, None) => Vec::new(),
    };
    if is_pv && !resize.no_fs && online.is_none() {
        for program in lvm_programs(resize.grow_lv, target.kind) {
            if !programs.contains(&program) {
                programs.push(program);
            }
        }
    }
    let tools: Vec<(Program, bool)> = programs
        .into_iter()
        .map(|program| (program, platform.tool_available(program)))
        .collect();
    let lvm = match (&selected, resize.no_fs) {
        (Some(partition), false) if is_pv && online.is_none() => Some(capture_lvm(
            platform,
            &target,
            partition,
            resize.grow_lv,
            resize.lv.as_deref(),
            &tools,
        )),
        _ => None,
    };
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
        lvm,
        online,
        partition_mount,
        swap_active,
    })
}

/// 挂载点形态：先反查挂载事实，再以所在整盘为现场；判定层按在线几何调整处理。
fn capture_mounted(
    platform: &dyn Platform,
    request: &Request,
    path: &std::path::Path,
) -> Result<ResizeSnapshot, CaptureError> {
    let facts = platform.mount_facts(path)?;
    let (disk, device, number) = match &facts {
        MountFacts::Mounted {
            disk,
            device,
            partition,
            ..
        } => (disk.clone(), device.clone(), *partition),
        MountFacts::NotMounted => {
            return Err(CaptureError::Unreadable {
                why: format!("{} is not a mount point", path.display()),
            });
        }
        MountFacts::Ambiguous => {
            return Err(CaptureError::Unreadable {
                why: format!("{} has more than one mount record", path.display()),
            });
        }
        MountFacts::NotPartition { device } => {
            return Err(CaptureError::Unreadable {
                why: format!(
                    "{} is not a partition; use the :N form for a whole disk or volume",
                    device.display()
                ),
            });
        }
        MountFacts::Unmapped { why } => {
            return Err(CaptureError::Unreadable {
                why: format!("cannot map {} to a whole disk: {why}", path.display()),
            });
        }
    };
    let target = super::filesystem::open_disk(platform, &disk, request)?;
    let handle = platform.open_target_read_only(&target.path)?;
    let view = table::observe(handle.as_ref(), request.sector_size(), handle.sector_size())
        .map_err(|error| CaptureError::Unreadable {
            why: error.to_string(),
        })?;
    let record = record_state(platform, &target)?;
    let prior = prior_state(&record);
    let sector_size = view.sector_size().unwrap_or(target.sector_size);
    let stamp = stamp_of(handle.as_ref(), target.bytes, sector_size)?;
    let table_facts = match &view {
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
    let selector = PartitionSelector::Number(number);
    let selected = view.find(number).cloned();
    let tools: Vec<(Program, bool)> = selected
        .as_ref()
        .map(|partition| programs_for(partition.content))
        .unwrap_or_default()
        .into_iter()
        .map(|program| (program, platform.tool_available(program)))
        .collect();
    let swap_active = Some(platform.swap_active(&target.path, number)?);
    Ok(ResizeSnapshot {
        target: TargetFacts {
            sector_size,
            ..target
        },
        // 在线形态跳过占用闸：目标正被用户的挂载占用。
        occupancy: Occupancy::Idle,
        record,
        prior,
        view,
        facts: table_facts,
        whole_content: None,
        stamp,
        is_partition_node: false,
        selector: Some(selector),
        selected,
        tools,
        lvm: None,
        online: Some(OnlineFacts {
            mount_point: path.to_path_buf(),
            device,
            disk,
            partition: number,
        }),
        partition_mount: None,
        swap_active,
    })
}

/// PV 链需要的只读工具。
fn lvm_programs(grow_lv: bool, kind: TargetKind) -> Vec<Program> {
    let mut programs = vec![Program::Pvs, Program::Pvresize];
    if grow_lv {
        programs.push(Program::Lvs);
        programs.push(Program::Lvextend);
    }
    if kind == TargetKind::File {
        programs.push(Program::Losetup);
    }
    programs
}

/// 采集 PV 的只读现场；工具缺失时如实记录，不尝试调用。
fn capture_lvm(
    platform: &dyn Platform,
    target: &TargetFacts,
    partition: &PartitionView,
    grow_lv: bool,
    want_lv: Option<&str>,
    tools: &[(Program, bool)],
) -> LvmFacts {
    let available = |program: Program| tools.iter().any(|(known, ok)| *known == program && *ok);
    if let Some(program) = lvm_programs(grow_lv, target.kind)
        .into_iter()
        .find(|program| !available(*program))
    {
        let error = LvmError::ToolMissing(format!(
            "LVM tooling missing: {} not found in PATH (install lvm2)",
            platform.tool_name(program)
        ));
        return LvmFacts {
            selection: grow_lv.then(|| Err(error.clone())),
            allocated_end: Err(error),
        };
    }
    let start = partition.first_lba.saturating_mul(target.sector_size);
    let bytes = partition.bytes;
    let device = match target.kind {
        TargetKind::BlockDevice => platform
            .partition_device(&target.path, partition.number)
            .map_err(|error| LvmError::Failed(error.to_string())),
        TargetKind::File => platform
            .bind_loop(&target.path, start, bytes)
            .map(|binding| binding.device().to_path_buf())
            .map_err(|error| LvmError::Failed(error.to_string())),
        TargetKind::Other => Err(LvmError::Failed(
            "the target is neither an image nor a block device".to_owned(),
        )),
    };
    let device = match device {
        Ok(path) => path,
        Err(error) => {
            return LvmFacts {
                selection: grow_lv.then(|| Err(error.clone())),
                allocated_end: Err(error),
            };
        }
    };
    let Some(device) = device.to_str() else {
        let error = LvmError::Failed("the partition device path is not valid UTF-8".to_owned());
        return LvmFacts {
            selection: grow_lv.then(|| Err(error.clone())),
            allocated_end: Err(error),
        };
    };
    LvmFacts {
        selection: grow_lv.then(|| lvm::select_lv(platform, device, want_lv)),
        allocated_end: lvm::pv_allocated_end_bytes(platform, device),
    }
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
