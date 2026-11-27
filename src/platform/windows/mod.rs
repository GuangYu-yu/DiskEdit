//! Windows 后端子树：预留端口。当前每个端口都显式返回"不支持"，
//! 接入能力时只在本子树内逐个实现，不改上层。

use crate::platform::unsupported_platform;

/// Windows 宿主端口。
pub struct WindowsPlatform;

unsupported_platform!(WindowsPlatform, "Windows");
