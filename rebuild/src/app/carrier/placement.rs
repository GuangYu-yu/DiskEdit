//! 放置：整份作业在本机执行，还是交给虚拟机载体。
//!
//! 放置一次决定，步骤之间不再改变。只有文件系统层步骤需要内核能力与外部工具；
//! 其余步骤宿主总能直接完成。宿主不满足条件时，清单通过校验的载体接手；两者都
//! 不可用就在写入之前说明缺口与两条出路。

use diskedit_core::fs::{ContentKind, Program, catalog};

use super::manifest::{self, Bundle};
use crate::app::decide::common::admission;
use crate::app::decide::step::{Step, StepKind};
use crate::app::execute::ExecError;
use crate::app::observe::SetProperty;
use crate::platform::Platform;

/// 一份作业的执行位置。
pub enum Placement {
    Native,
    Vm(Bundle),
}

/// 决定放置。
pub fn decide(platform: &dyn Platform, steps: &[Step]) -> Result<Placement, ExecError> {
    let mut required: Vec<Program> = Vec::new();
    let mut filesystem = false;
    for step in steps {
        let Some((content, verb)) = requirement(&step.kind) else {
            continue;
        };
        filesystem = true;
        let programs = admission::programs(content, verb).map_err(|_| {
            ExecError::Unsupported(format!("{} 的文件系统步骤没有工具矩阵", content.as_str()))
        })?;
        if let Some(list) = programs {
            for program in list {
                if !required.contains(program) {
                    required.push(*program);
                }
            }
        }
    }
    if !filesystem {
        return Ok(Placement::Native);
    }

    let ready = platform.filesystem_ready();
    let missing: Vec<Program> = required
        .iter()
        .copied()
        .filter(|program| platform.require(*program).is_err())
        .collect();
    if ready && missing.is_empty() {
        return Ok(Placement::Native);
    }

    let mut gaps = Vec::new();
    if !ready {
        gaps.push("宿主不能直接操作块设备（需要 root 与分区设备视图）".to_owned());
    }
    if !missing.is_empty() {
        let names: Vec<String> = missing
            .iter()
            .map(|program| format!("{program:?}"))
            .collect();
        gaps.push(format!("缺少程序：{}", names.join("、")));
    }
    let detail = gaps.join("；");

    match manifest::resolve() {
        Ok(Some(bundle)) if platform.carrier_available() => Ok(Placement::Vm(bundle)),
        Ok(Some(_)) => Err(ExecError::Unsupported(format!(
            "{detail}；本宿主不提供虚拟机载体"
        ))),
        Ok(None) => Err(ExecError::Unsupported(format!(
            "{detail}；安装缺失程序后重试，或设置 {}=指向包含 manifest.json、kernel、initramfs 的目录以启用虚拟机载体",
            manifest::VARIABLE
        ))),
        Err(why) => Err(ExecError::Unsupported(format!(
            "{detail}；虚拟机包不可用：{why}"
        ))),
    }
}

/// 步骤对应的内容类别与能力动词；非文件系统步骤返回 `None`。
fn requirement(kind: &StepKind) -> Option<(ContentKind, catalog::Verb)> {
    match kind {
        StepKind::MakeFilesystem { filesystem, .. } => {
            Some((filesystem.content(), catalog::Verb::Mkfs))
        }
        StepKind::GrowFilesystem { filesystem, .. } => {
            Some((ContentKind::Filesystem(*filesystem), catalog::Verb::Grow))
        }
        StepKind::ShrinkFilesystem { filesystem, .. } => {
            Some((ContentKind::Filesystem(*filesystem), catalog::Verb::Shrink))
        }
        StepKind::SetProperty {
            property: SetProperty::Label | SetProperty::Uuid,
            content: Some(content),
            ..
        } => Some((*content, catalog::Verb::Attrs)),
        StepKind::WriteTable { .. }
        | StepKind::AddPartition { .. }
        | StepKind::DeletePartition { .. }
        | StepKind::ResizePartition { .. }
        | StepKind::SetProperty { .. }
        | StepKind::CopyPartition { .. }
        | StepKind::MovePartition { .. }
        | StepKind::RefreshKernelView
        | StepKind::RecoverRecords { .. }
        | StepKind::Inspect
        | StepKind::Check { .. }
        | StepKind::ListRecords
        | StepKind::VerifyRecord { .. }
        | StepKind::ScanRecords => None,
    }
}
