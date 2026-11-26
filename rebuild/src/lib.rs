//! DiskEdit 库入口。
//!
//! 依赖方向固定为 `app → platform → core`。本文件只声明这两层子系统的入口；
//! 每层内部的文件职责由各自的 `mod.rs` 说明。

pub mod app;
pub mod platform;

/// 进程入口：先看是不是来宾工作进程，否则交给应用层。
pub fn entry(args: impl IntoIterator<Item = std::ffi::OsString>) -> u8 {
    let args: Vec<std::ffi::OsString> = args.into_iter().collect();
    match platform::worker_entry(&args) {
        Some(code) => code,
        None => app::run(args),
    }
}
