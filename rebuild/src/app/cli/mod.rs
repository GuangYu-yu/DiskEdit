//! 命令行层：命令声明、`argv → Request`、帮助与用法错误。
//!
//! 本层只把字面量解析成请求，不认识目标与文件系统。

use std::ffi::OsString;

/// 解析参数并返回退出码。
///
/// 命令声明表接入前，任何调用都只能落用法错误。
pub fn dispatch(_args: impl IntoIterator<Item = OsString>) -> u8 {
    2
}
