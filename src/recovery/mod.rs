//! 恢复引擎：记录状态机、证据分类与清理许可。
//!
//! 记录整份替换，证据的四种失败形态（格式、身份、长度、CRC）各自成态；
//! 删除只接受 [`CommittedRecord`] 与 [`CleanupPermit`]。

// 非 Linux 构建只保留恢复记录的类型与拒绝路径，未用到的再导出与项都属预期；
// 收在平台条件里，Linux 上任何一条多余的再导出仍会被 lint 抓到
#![cfg_attr(not(target_os = "linux"), allow(dead_code, unused_imports))]

mod cleanup;
mod engine;
mod evidence;
mod journal;
mod record;
mod scene;
pub(crate) mod sink;

#[cfg(test)]
mod tests;

pub(crate) use cleanup::*;
pub(crate) use engine::*;
pub(crate) use evidence::*;
pub(crate) use journal::*;
pub(crate) use record::*;
pub(crate) use scene::*;
