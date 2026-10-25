//! FS 层外部工具链调用：offset loop 映射 + e2fsck/resize2fs/mkfs。
//!
//! 严格 Linux。非 Linux 平台编译为显式拒绝存根。

mod error;
mod env;
pub(crate) mod mount;
mod scope;
mod probe;
mod swap;
mod mkfs;
mod capabilities;
mod grow;
mod shrink;
mod attrs;

#[cfg(test)]
mod tests;

pub(crate) use error::*;
pub(crate) use env::*;
pub(crate) use mount::*;
pub(crate) use scope::*;
pub(crate) use probe::*;
pub(crate) use swap::*;
pub(crate) use mkfs::*;
pub(crate) use capabilities::*;
pub(crate) use grow::*;
pub(crate) use shrink::*;
pub(crate) use attrs::*;

use crate::dev::FileSource;
use crate::fsid::{fs_name, FsKind};
use std::ffi::{OsStr, OsString};
use std::io;
#[cfg(target_os = "linux")]
use std::path::Path;
use std::path::PathBuf;
use std::process::{Command, Stdio};