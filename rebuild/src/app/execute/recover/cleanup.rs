//! 清理记录目录中的活跃记录。
//!
//! 只删除本实现写下的记录文件；调用方先经纯恢复器确认记录已提交或已放弃。

use crate::app::execute::ExecError;
use crate::platform::RecordStore;

/// 删除活跃记录；目录保持存在。
pub fn clear(store: &mut dyn RecordStore) -> Result<(), ExecError> {
    store.clear().map_err(ExecError::from)
}
