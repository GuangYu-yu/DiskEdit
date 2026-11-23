//! macOS 端口：结构已就位，设备级能力显式不支持。
//!
//! 这里不返回空视图、不假装成功；接入磁盘设备、分区表、挂载等能力时在本子树补
//! 实现，上层只依赖 [`crate::platform`] 的中立类型。

use crate::platform::unsupported_platform;

/// macOS 宿主端口。
#[derive(Debug, Default)]
pub struct MacOs;

impl MacOs {
    pub const fn new() -> Self {
        Self
    }
}

unsupported_platform!(MacOs, "macOS");
