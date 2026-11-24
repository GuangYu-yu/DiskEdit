//! 只读诊断的执行：把检查请求变成一个可检查的设备节点并运行外部程序。
//!
//! 检查不写目标字节；镜像分区先绑定临时块设备视图，诊断结束即解除。结果只分
//! 四类，由报告层映射成退出码。

use std::path::PathBuf;

use crate::app::decide::report::{CheckQuery, CheckTarget};
use crate::platform::{Platform, PlatformError};

/// 一次检查的结果。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DiagnoseResult {
    /// 检查完成且没有报告问题。
    Clean { message: String },
    /// 检查完成并报告了问题。
    Problems { message: String },
    /// 缺少所需程序。
    Missing { message: String },
    /// 环境或 I/O 故障，未结论。
    Environment { message: String },
}

/// 运行一次 `check`。
pub fn run(platform: &dyn Platform, query: &CheckQuery) -> DiagnoseResult {
    if !platform.tool_available(query.program) {
        return DiagnoseResult::Missing {
            message: format!(
                "{} is not available on this host",
                platform.tool_name(query.program)
            ),
        };
    }
    let device = match resolve(platform, &query.target) {
        Ok(device) => device,
        Err(error) => {
            return DiagnoseResult::Environment {
                message: format!("check failed: {error}"),
            };
        }
    };
    let run = match platform.run(query.program, &[device.device().display().to_string()]) {
        Ok(run) => run,
        Err(error) => {
            return DiagnoseResult::Environment {
                message: format!("check failed: {error}"),
            };
        }
    };
    let output = [run.stdout.trim(), run.stderr.trim()]
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    match run.code {
        Some(0) => DiagnoseResult::Clean {
            message: format!(
                "check done on partition #{} ({})",
                query.partition,
                query.content.as_str()
            ),
        },
        Some(_) => DiagnoseResult::Problems {
            message: format!(
                "check reported problems on partition #{} ({}){}",
                query.partition,
                query.content.as_str(),
                suffix(&output)
            ),
        },
        None => DiagnoseResult::Environment {
            message: format!(
                "check failed: the tool terminated without an exit status{}",
                suffix(&output)
            ),
        },
    }
}

/// 检查句柄：块设备视图或既有分区设备。
#[derive(Debug)]
struct Device {
    path: PathBuf,
    binding: Option<crate::platform::LoopBinding>,
}

impl Device {
    fn device(&self) -> &std::path::Path {
        match &self.binding {
            Some(binding) => binding.device(),
            None => &self.path,
        }
    }
}

fn resolve(platform: &dyn Platform, target: &CheckTarget) -> Result<Device, PlatformError> {
    match target {
        CheckTarget::PartitionDevice { disk, number } => Ok(Device {
            path: platform.partition_device(disk, *number)?,
            binding: None,
        }),
        CheckTarget::ImageRegion { path, offset, len } => Ok(Device {
            path: path.clone(),
            binding: Some(platform.bind_loop(path, *offset, *len)?),
        }),
    }
}

fn suffix(output: &str) -> String {
    if output.is_empty() {
        String::new()
    } else {
        format!(": {output}")
    }
}
