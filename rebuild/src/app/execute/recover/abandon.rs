//! 放弃回滚：只把记录移入隔离区，目标字节不变。
//!
//! 归属与对象由记录目录与目标身份决定；载荷是否完好不影响放弃动作。平台负责
//! 在同目录以 `.abandoned` 后缀归档，命名冲突顺延，不覆盖已有文件。

use crate::app::execute::ExecError;
use crate::platform::RecordStore;

/// 归档活跃记录，返回归档后的位置或标识；没有记录时是成功空操作。
pub fn apply(store: &mut dyn RecordStore) -> Result<String, ExecError> {
    if store.load().map_err(ExecError::from)?.is_none() {
        return Ok(String::new());
    }
    store.quarantine().map_err(ExecError::from)
}
