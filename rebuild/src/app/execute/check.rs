//! 一致性检查：按内容档案跑只读检查器。
//!
//! 检查器在分区设备视图上运行，不改分区表、不写记录；非零退出是检查结论，按
//! 拒绝上报，而不是当作成功。f2fs 的检查器可能回写全局元数据，这一偏差在档案表
//! 里只有一个程序可选，这里不为它另开一条写路径。

use std::path::Path;

use diskedit_core::fs::{ContentKind, Program, Verb, profile};
use diskedit_core::region::Region;

use crate::platform::{Platform, Region as DeviceRegion};

use super::ExecError;
use super::steps::Host;
use super::steps::filesystem::with_device;

/// 一次检查跑了哪些程序。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CheckReport {
    pub programs: Vec<Program>,
}

/// 在 `partition` 上跑检查器。
pub fn run(
    platform: &dyn Platform,
    target: &Path,
    partition: u32,
    region: Region,
    content: ContentKind,
) -> Result<CheckReport, ExecError> {
    let programs = profile(content)
        .support(Verb::Check)
        .tools()
        .ok_or_else(|| ExecError::Unsupported(format!("{} 没有可用的检查器", content.as_str())))?;
    let host = Host { platform, target };
    let device_region = DeviceRegion::new(region.start(), region.len());
    with_device(&host, partition, device_region, |device| {
        let text = device.to_string_lossy().into_owned();
        let mut ran = Vec::new();
        for program in programs {
            run_checker(platform, *program, &text)?;
            ran.push(*program);
        }
        Ok(CheckReport { programs: ran })
    })
}

/// 跑一条检查器命令；参数形态是平台无关的档案知识，设备路径由调用方给。
fn run_checker(platform: &dyn Platform, program: Program, device: &str) -> Result<(), ExecError> {
    let mut args: Vec<&str> = match program {
        Program::E2fsck
        | Program::Ntfsfix
        | Program::XfsRepair
        | Program::FsckVfat
        | Program::FsckExfat => vec!["-n"],
        Program::Btrfs => vec!["check"],
        Program::FsckF2fs => Vec::new(),
        Program::Resize2fs
        | Program::Dumpe2fs
        | Program::Mke2fs
        | Program::Tune2fs
        | Program::Ntfsresize
        | Program::Ntfslabel
        | Program::ResizeF2fs
        | Program::XfsGrowfs
        | Program::XfsAdmin
        | Program::Btrfstune
        | Program::Fatresize
        | Program::Fatlabel
        | Program::Exfatlabel
        | Program::Mkswap
        | Program::Swaplabel
        | Program::Pvs
        | Program::Lvs
        | Program::Vgs
        | Program::Pvresize
        | Program::MkfsVfat
        | Program::MkfsExfat
        | Program::MkfsNtfs
        | Program::MkfsXfs
        | Program::MkfsBtrfs
        | Program::MkfsF2fs
        | Program::Mount
        | Program::Umount
        | Program::Losetup
        | Program::Sfdisk
        | Program::Partx
        | Program::Blkid => {
            return Err(ExecError::InvalidPlan(format!(
                "{program:?} 不是一致性检查器"
            )));
        }
    };
    args.push(device);
    let output = platform
        .run(program, &args)
        .map_err(|error| ExecError::Io(format!("运行 {program:?} 失败: {error}")))?;
    if output.succeeded() {
        return Ok(());
    }
    let detail = output.stderr_text();
    let detail = detail.trim();
    Err(ExecError::Check(if detail.is_empty() {
        format!("{program:?} 报告问题（{:?}）", output.exit)
    } else {
        format!("{program:?} 报告问题（{:?}）：{detail}", output.exit)
    }))
}
