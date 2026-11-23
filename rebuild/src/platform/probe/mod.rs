//! 探测：文件系统签名识别与最小尺寸探针声明。
//!
//! 两者都是"读入已取得的字节或声明用哪个程序"，本身不做 I/O；真正的读取由
//! `source` 端口完成，程序执行由 `tools` 完成。

pub mod filesystem;
pub mod min_size;

pub use filesystem::{MIN_BYTES, identify};
pub use min_size::{MinSizeRule, Unit, has_probe, min_size_rule, shrink_declared};
