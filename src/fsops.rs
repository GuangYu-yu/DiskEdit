//! FS 层外部工具链调用：offset loop 映射 + e2fsck/resize2fs/mkfs。
//!
//! 严格 Linux。非 Linux 平台编译为显式拒绝存根。

mod error;
mod env;
mod ext;
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
pub(crate) use ext::*;
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
use crate::fsid::{content_name, ContentKind, FsKind};
use std::ffi::{OsStr, OsString};
use std::io;
#[cfg(target_os = "linux")]
use std::path::Path;
use std::path::PathBuf;

/// 扩容 / 缩容结果共用的后置条件装配：未证实的原因与补救、没卸掉的挂载点。
/// 顺序即报告顺序——先未证实（带补救），后清理待办
pub(crate) fn run_pending(
    part: u32,
    step: &'static str,
    unverified: Option<(String, String)>,
    leak: Option<MountLeak>,
) -> Vec<crate::outcome::Pending> {
    let mut pending = Vec::new();
    if let Some((why, hint)) = unverified {
        pending.push(crate::outcome::Pending::new(
            part,
            crate::outcome::PendingKind::Other(step),
            why,
            hint,
        ));
    }
    if let Some(leak) = leak {
        pending.push(leak.into_pending(part));
    }
    pending
}
use std::process::{Command, Stdio};