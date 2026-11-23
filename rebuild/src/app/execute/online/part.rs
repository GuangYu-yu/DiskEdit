//! 上表之后让内核视图追上盘面。
//!
//! 顺序固定：写盘上的表先经记录与回读核对落定，随后才调用本模块。平台内部
//! 先试在线重读，失败再按盘上表补建分区；本模块不关心二者差异。

use std::path::Path;

use crate::app::execute::ExecError;
use crate::platform::Platform;

/// 让内核重新读取给定目标的分区表。
pub fn publish(platform: &dyn Platform, target: &Path) -> Result<(), ExecError> {
    platform.update_table(target).map_err(ExecError::from)
}
