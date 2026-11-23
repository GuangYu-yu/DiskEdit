//! 按偏移读写的端口与目标独占。
//!
//! [`ReadAt`] 与 [`WriteAt`] 是平台层向上暴露的唯一读写形状：偏移以字节计，
//! 一次调用只作用在给定区间内。普通文件与块设备各自实现这两个 trait，调用方
//! 不需要知道自己拿到的是哪一种。
//!
//! 具体实现（fd 的打开方式、ioctl 编号、flock 参数）只在本模块内可见。

mod block;
mod file;
pub mod guarded;
pub mod lease;

use super::{PlatformError, PlatformResult};

pub use block::BlockSource;
pub use file::FileSource;
pub use lease::Lease;

// 读写形状由中立端口定义；本模块只提供实现。
pub use crate::platform::{ReadAt, WriteAt};
