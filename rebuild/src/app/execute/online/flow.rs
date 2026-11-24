//! 在线执行：在用户已有的挂载上扩容文件系统。
//!
//! 与离线事务的分工写在类型上：这里不写分区表、不登记原像、不卸载也不重新挂载。
//! 执行仍先取整盘租约并复核身份，再把零散的前置条件（挂载点仍指向同一分区、
//! 目标不是活动 swap、没有未完成记录、内核分区视图与盘上表一致、btrfs 单设备）
//! 全部查完，最后才运行该族的在线扩容工具。
//!
//! 工具一旦启动，写入就已离开本实现的记录模型：失败按"目标可能已改变"报告为
//! 部分完成，而不是拒绝，也不会出现"报告未改动"的路径。

use std::path::Path;

use diskedit_core::fs::{FsKind, Program};
use diskedit_core::record::RecoveryPlan;

use super::guard;
use crate::app::decide::ConfirmedPlan;
use crate::app::decide::step::{Step, StepKind};
use crate::app::execute::steps::filesystem::reaches_end;
use crate::app::execute::{FailureKind, Faults, Outcome};
use crate::platform::{MountFacts, Platform, PlatformError};

/// 计划中的在线步骤；离线计划返回 `None`。
pub fn step(confirmed: &ConfirmedPlan) -> Option<&Step> {
    let mut steps = confirmed
        .steps()
        .iter()
        .filter(|step| matches!(step.kind, StepKind::GrowFilesystemOnline { .. }));
    let step = steps.next()?;
    if steps.next().is_some() {
        return None;
    }
    Some(step)
}

/// 在线计划只能由这一个入口执行；它不进入事务的写通道。
pub fn run(confirmed: &ConfirmedPlan, platform: &dyn Platform, faults: Faults) -> Outcome {
    let Some(step) = step(confirmed) else {
        return Outcome::unwritten(
            FailureKind::Internal,
            "online executor received a plan without a single online step",
        );
    };
    if confirmed.steps().len() != 1 {
        return Outcome::unwritten(
            FailureKind::Internal,
            "online plan must contain exactly one step",
        );
    }
    let StepKind::GrowFilesystemOnline {
        mount_point,
        device,
        disk,
        partition,
        filesystem,
        bytes,
    } = &step.kind
    else {
        return Outcome::unwritten(
            FailureKind::Internal,
            "online executor received a non-online step",
        );
    };
    let mut handle = match platform.open_target(disk) {
        Ok(handle) => handle,
        Err(error) => {
            return Outcome::unwritten(FailureKind::FailedBeforeWrite, error.to_string());
        }
    };
    if let Err(error) = handle.lock_exclusive() {
        return Outcome::unwritten(FailureKind::FailedBeforeWrite, error.to_string());
    }
    let observed = match platform.fingerprint(disk) {
        Ok(fingerprint) => fingerprint,
        Err(error) => {
            return Outcome::unwritten(FailureKind::FailedBeforeWrite, error.to_string());
        }
    };
    if !confirmed.identity().matches(observed) {
        return Outcome::unwritten(
            FailureKind::Refused,
            "target identity differs from confirmation after locking",
        );
    }
    match platform.mount_facts(mount_point) {
        Ok(MountFacts::Mounted {
            device: current,
            partition: Some(facts),
        }) if facts.number == *partition && current == *device && facts.disk == *disk => {
            if let Err(outcome) = kernel_view_matches(platform, disk, partition, *bytes) {
                return outcome;
            }
        }
        Ok(_) => {
            return Outcome::unwritten(
                FailureKind::Refused,
                "mount point no longer resolves to the same partition",
            );
        }
        Err(error) => return resolve_failure(error),
    }
    match platform.swap_active(device) {
        Ok(true) => {
            return Outcome::unwritten(
                FailureKind::Refused,
                "target partition is active swap; run swapoff first",
            );
        }
        Ok(false) => {}
        Err(error) => return Outcome::unwritten(FailureKind::FailedBeforeWrite, error.to_string()),
    }
    let identity = confirmed.identity();
    let mut store = match platform.records(disk, identity) {
        Ok(store) => store,
        Err(error) => {
            return Outcome::unwritten(FailureKind::FailedBeforeWrite, error.to_string());
        }
    };
    match store.load() {
        Ok(None) => {}
        Ok(Some(existing)) => match diskedit_core::record::recover(&existing, identity) {
            RecoveryPlan::Absent | RecoveryPlan::Committed => {
                if let Err(error) = store.clear() {
                    return Outcome::unwritten(FailureKind::Refused, error.to_string());
                }
            }
            RecoveryPlan::RollBack { .. }
            | RecoveryPlan::Replay { .. }
            | RecoveryPlan::NeedsManual { .. }
            | RecoveryPlan::Foreign { .. } => {
                return Outcome::unwritten(
                    FailureKind::Refused,
                    "unfinished record present; recover first",
                );
            }
        },
        Err(error) => {
            return Outcome::unwritten(FailureKind::FailedBeforeWrite, error.to_string());
        }
    }
    let btrfs = if *filesystem == FsKind::Btrfs {
        match guard::capture(platform, device) {
            Ok(identity) => match guard::ensure_mapped(&identity) {
                Ok(()) => Some(identity),
                Err(error) => {
                    return Outcome::unwritten(FailureKind::Unsupported, error.to_string());
                }
            },
            Err(error) => {
                return Outcome::unwritten(FailureKind::FailedBeforeWrite, error.to_string());
            }
        }
    } else {
        None
    };
    let program = match filesystem {
        FsKind::Ext => Program::Resize2fs,
        FsKind::Xfs => Program::XfsGrowfs,
        FsKind::Btrfs => Program::Btrfs,
        FsKind::Squashfs
        | FsKind::Erofs
        | FsKind::F2fs
        | FsKind::Exfat
        | FsKind::Vfat
        | FsKind::Ntfs
        | FsKind::HfsPlus
        | FsKind::Apfs
        | FsKind::Bcachefs
        | FsKind::Jfs
        | FsKind::Reiserfs
        | FsKind::Udf => {
            return Outcome::unwritten(
                FailureKind::Unsupported,
                format!("{filesystem:?} has no online grow program"),
            );
        }
    };
    if let Err(error) = platform.require(program) {
        return Outcome::unwritten(FailureKind::Unsupported, error.to_string());
    }
    if faults.before_write {
        return Outcome::unwritten(
            FailureKind::FailedBeforeWrite,
            "fault injection: interrupted before the online tool ran",
        );
    }
    let device_text = device.to_string_lossy().into_owned();
    let mount_text = mount_point.to_string_lossy().into_owned();
    let args: Vec<&str> = match filesystem {
        FsKind::Ext => vec![device_text.as_str()],
        FsKind::Xfs => vec![mount_text.as_str()],
        FsKind::Btrfs => vec!["filesystem", "resize", "max", mount_text.as_str()],
        FsKind::Squashfs
        | FsKind::Erofs
        | FsKind::F2fs
        | FsKind::Exfat
        | FsKind::Vfat
        | FsKind::Ntfs
        | FsKind::HfsPlus
        | FsKind::Apfs
        | FsKind::Bcachefs
        | FsKind::Jfs
        | FsKind::Reiserfs
        | FsKind::Udf => Vec::new(),
    };
    let output = match platform.run(program, &args) {
        Ok(output) => output,
        Err(error) => {
            return Outcome::unwritten(FailureKind::FailedBeforeWrite, error.to_string());
        }
    };
    if !output.succeeded() {
        return Outcome::needs_recovery(format!(
            "{program:?} exited with {:?}: {}",
            output.exit,
            output.stderr_text().trim()
        ));
    }
    if faults.after_write {
        return Outcome::needs_recovery("fault injection: interrupted after the online tool ran");
    }
    if let Some(expected) = btrfs {
        match guard::capture(platform, device)
            .and_then(|actual| guard::verify_unchanged(&expected, &actual))
        {
            Ok(()) => {}
            Err(error) => return Outcome::needs_recovery(error.to_string()),
        }
    }
    match platform.filesystem_size_of(device, *bytes, *filesystem) {
        Ok(size) if reaches_end(size.bytes, size.unit, *bytes) => Outcome::completed(),
        Ok(size) => Outcome::needs_recovery(format!(
            "filesystem size {} did not reach the partition end {}",
            size.bytes, bytes
        )),
        Err(error) => Outcome::needs_recovery(format!(
            "failed to read back filesystem size after the online grow: {error}"
        )),
    }
}

/// 内核分区视图必须与盘上表同尺寸；过期视图会让工具扩过头，写前即停。
///
/// sysfs 的分区长度以 512 字节扇区计，盘上表以逻辑扇区计；统一换算成字节比较。
fn kernel_view_matches(
    platform: &dyn Platform,
    disk: &Path,
    partition: &u32,
    expected_bytes: u64,
) -> Result<(), Outcome> {
    let partitions = platform
        .partitions(disk)
        .map_err(|error| Outcome::unwritten(FailureKind::FailedBeforeWrite, error.to_string()))?;
    let Some(facts) = partitions.iter().find(|facts| facts.number == *partition) else {
        return Err(Outcome::unwritten(
            FailureKind::FailedBeforeWrite,
            format!("partition {partition} is missing from the kernel view"),
        ));
    };
    if facts.sectors.checked_mul(512) != Some(expected_bytes) {
        return Err(Outcome::unwritten(
            FailureKind::FailedBeforeWrite,
            "kernel partition view is stale; reread the partition table first",
        ));
    }
    Ok(())
}

fn resolve_failure(error: PlatformError) -> Outcome {
    match error {
        PlatformError::Parse { .. } => Outcome::unwritten(FailureKind::Refused, error.to_string()),
        PlatformError::NotSupported { .. } | PlatformError::UnsupportedFs { .. } => {
            Outcome::unwritten(FailureKind::Unsupported, error.to_string())
        }
        PlatformError::Io { .. }
        | PlatformError::Busy { .. }
        | PlatformError::Denied { .. }
        | PlatformError::Tools { .. } => {
            Outcome::unwritten(FailureKind::FailedBeforeWrite, error.to_string())
        }
    }
}
