//! `resizefs`：把文件系统撑满所在分区。
//!
//! 两种形态最后都汇成同一件事：把文件系统扩到分区容量，且从不改分区表。
//!
//! - 离线形态带分区选择器，直接对整盘上的该分区操作；
//! - 挂载点形态（没有分区选择器）消费平台反查出的挂载事实：目标必须已挂载、且
//!   挂载设备是分区。它不要求目标空闲，也绝不卸载用户的挂载。
//!
//! 尺寸参数只出现在挂载点形态，且只允许「撑满当前分区」：大于分区容量要先改表，
//! 小于分区容量是收缩，两者都在写盘之前拒绝。

use std::path::Path;

use diskedit_core::fs::Verb;
use diskedit_core::fs::catalog;

use super::common::{admission, capacity};
use super::plan::Plan;
use super::refusal::Refusal;
use super::step::{Step, StepKind};
use crate::app::observe::{PartitionSelector, Snapshot};
use crate::platform::MountFacts;

pub fn plan(
    snapshot: &Snapshot,
    selector: Option<PartitionSelector>,
    bytes: Option<u64>,
    mount_point: &Path,
) -> Result<Plan, Refusal> {
    match selector {
        Some(selector) => plan_offline(snapshot, selector, bytes),
        None => plan_online(snapshot, bytes, mount_point),
    }
}

fn plan_offline(
    snapshot: &Snapshot,
    selector: PartitionSelector,
    bytes: Option<u64>,
) -> Result<Plan, Refusal> {
    let admitted = admission::admit(snapshot)?;
    if bytes.is_some() {
        // 离线形态没有尺寸参数；保留这条判据，供绕过命令行直接构造请求的调用方一并拒绝。
        return Err(Refusal::ArgumentOutOfRange {
            what: "offline resizefs does not accept a size argument".to_owned(),
        });
    }
    let partition = admission::select(snapshot, selector)?;
    let facts = admission::partition(snapshot, partition)?;
    let capacity_bytes = capacity_bytes(facts, admitted.sector)?;
    let content = admission::content(facts)?;
    let filesystem = filesystem_of(content)?;
    let programs =
        admission::programs(content, Verb::Grow)?.ok_or_else(|| Refusal::ContentUnsupported {
            what: format!("{} grow", content.as_str()),
            why: "this filesystem does not support growing".to_owned(),
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

fn plan_online(
    snapshot: &Snapshot,
    bytes: Option<u64>,
    mount_point: &Path,
) -> Result<Plan, Refusal> {
    let (device, partition) = mounted_partition(snapshot, mount_point)?;
    let admitted = admission::admit_online(snapshot)?;
    let facts = admission::partition(snapshot, partition.number)?;
    let capacity_bytes = capacity_bytes(facts, admitted.sector)?;
    if let Some(requested) = bytes {
        if requested > capacity_bytes {
            return Err(Refusal::CapacityInsufficient {
                needed: requested,
                available: capacity_bytes,
            });
        }
        if requested < capacity_bytes {
            return Err(Refusal::OnlineShrink {
                why: "this form grows the filesystem to fill its partition and never changes the partition table"
                    .to_owned(),
            });
        }
    }
    let content = admission::content(facts)?;
    let filesystem = filesystem_of(content)?;
    let grow = catalog::online_grow(content);
    let program = grow.program().ok_or_else(|| Refusal::ContentUnsupported {
        what: format!("{} online grow", content.as_str()),
        why: grow.reason().unwrap_or("no online grow program").to_owned(),
    })?;
    admission::require_tools(snapshot, &[program])?;
    Plan::write(
        vec![Step::new(
            StepKind::GrowFilesystemOnline {
                mount_point: mount_point.to_path_buf(),
                device,
                disk: partition.disk.clone(),
                partition: partition.number,
                filesystem,
                bytes: capacity_bytes,
            },
            Vec::new(),
        )],
        admitted.identity,
    )
}

/// 挂载事实必须落在某个分区上，否则在线形态没有可操作的目标。
fn mounted_partition<'a>(
    snapshot: &'a Snapshot,
    mount_point: &Path,
) -> Result<(std::path::PathBuf, &'a crate::platform::PartitionRef), Refusal> {
    use diskedit_core::evidence::Observation;
    let facts = match &snapshot.mount {
        Observation::Known { value, .. } => value,
        Observation::Unknown { why } => {
            return Err(Refusal::FactUnknown {
                what: format!("mount facts: {why}"),
            });
        }
        Observation::NotApplicable { why } => {
            return Err(Refusal::FactUnknown {
                what: format!("mount facts: {why}"),
            });
        }
    };
    match facts {
        MountFacts::Mounted {
            device,
            partition: Some(partition),
        } => Ok((device.clone(), partition)),
        MountFacts::Mounted {
            device,
            partition: None,
        } => Err(Refusal::MountNotPartition {
            device: device.display().to_string(),
        }),
        MountFacts::NotMounted => Err(Refusal::MountNotMounted {
            path: mount_point.display().to_string(),
        }),
        MountFacts::Ambiguous => Err(Refusal::MountAmbiguous {
            path: mount_point.display().to_string(),
        }),
        MountFacts::NotBlockDevice { source } => Err(Refusal::MountNotBlockDevice {
            source: source.display().to_string(),
        }),
        MountFacts::NotPartition { device } => Err(Refusal::MountNotPartition {
            device: device.display().to_string(),
        }),
        MountFacts::Unmapped { reason } => Err(Refusal::MountUnmapped {
            reason: reason.clone(),
        }),
    }
}

fn capacity_bytes(
    facts: &crate::app::observe::snapshot::PartitionFacts,
    sector: u64,
) -> Result<u64, Refusal> {
    let capacity =
        facts
            .partition
            .sectors
            .checked_mul(sector)
            .ok_or_else(|| Refusal::GeometryUnknown {
                why: "partition capacity exceeds the addressable bound".to_owned(),
            })?;
    if capacity == 0 {
        return Err(Refusal::ArgumentOutOfRange {
            what: "partition length is zero".to_owned(),
        });
    }
    Ok(capacity)
}

fn filesystem_of(
    content: diskedit_core::fs::ContentKind,
) -> Result<diskedit_core::fs::FsKind, Refusal> {
    content
        .filesystem()
        .ok_or_else(|| Refusal::ContentUnsupported {
            what: format!("{} resize", content.as_str()),
            why: "this content is not a filesystem".to_owned(),
        })
}
