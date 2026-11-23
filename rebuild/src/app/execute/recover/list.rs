//! 列出记录目录中的条目。
//!
//! 只读；不改目标字节，也不清理记录。报告层用它与 `records` 命令共享同一
//! 条目来源。

use crate::app::execute::ExecError;
use crate::platform::{RecordEntry, RecordStore};

/// 当前记录目录下的普通文件条目。
pub fn entries(store: &dyn RecordStore) -> Result<Vec<RecordEntry>, ExecError> {
    store.entries().map_err(ExecError::from)
}
