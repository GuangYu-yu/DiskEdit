//! DiskEdit 库入口。
//!
//! 依赖方向固定为 `app → platform → core`。本文件只声明这两层子系统的入口；
//! 每层内部的文件职责由各自的 `mod.rs` 说明。

pub mod app;
pub mod platform;
