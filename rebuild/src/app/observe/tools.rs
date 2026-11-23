//! 外部程序可用性：逐个程序询问宿主端口。
//!
//! 程序是否存在是运行期事实，不是编译期条件；结果进入快照，供判定按族取用。
//! 平台整体不提供外部程序时记 `NotApplicable`，找不到某个程序记已观测的缺失。

use std::path::PathBuf;

use diskedit_core::evidence::{Generation, Observation, Source};
use diskedit_core::fs::Program;

use crate::platform::{Platform, PlatformError};

/// 程序可用性的产生点。
pub const SOURCE: Source = Source::new("platform.require");

/// 一个程序的可用性。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ToolState {
    /// 已解析到可执行文件。
    Available(PathBuf),
    /// 宿主上找不到该程序；这是已观测的事实，不是未知。
    Missing { why: String },
}

/// 一个程序的可用性事实。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolAvailability {
    pub program: Program,
    pub state: Observation<ToolState>,
}

/// 询问全部已登记程序。
pub fn observe(platform: &dyn Platform, generation: Generation) -> Vec<ToolAvailability> {
    Program::ALL
        .iter()
        .copied()
        .map(|program| ToolAvailability {
            program,
            state: probe(platform, program, generation),
        })
        .collect()
}

fn probe(
    platform: &dyn Platform,
    program: Program,
    generation: Generation,
) -> Observation<ToolState> {
    match platform.require(program) {
        Ok(path) => Observation::known(ToolState::Available(path), SOURCE, generation),
        Err(PlatformError::Tools { why, .. }) => {
            Observation::known(ToolState::Missing { why }, SOURCE, generation)
        }
        Err(PlatformError::NotSupported { why }) => Observation::not_applicable(why),
        Err(
            error @ (PlatformError::Io { .. }
            | PlatformError::UnsupportedFs { .. }
            | PlatformError::Busy { .. }
            | PlatformError::Denied { .. }
            | PlatformError::Parse { .. }),
        ) => Observation::unknown(format!("探测程序 {program:?} 失败：{error}")),
    }
}
