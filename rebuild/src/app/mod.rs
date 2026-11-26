//! 应用层：命令行、观察、判定、执行与报告。
//!
//! 层内依赖方向固定为 `cli → report → execute → decide → observe`。
//! `execute` 只读取 `decide` 已铸造的计划值，不调用判定函数。

use std::ffi::OsString;

pub mod cli;
pub mod decide;
pub mod execute;
pub mod lvm;
pub mod observe;
pub mod report;

/// 运行一次进程调用，返回退出码。
pub fn run(args: impl IntoIterator<Item = OsString>) -> u8 {
    cli::dispatch(args)
}
