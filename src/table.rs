//! 分区表：GPT/MBR 解析 + 崩溃安全写入。
//!
//! 读取经 gptman（主备头自动回退、扇区大小自动探测）。
//! 写入自行序列化（gptman write_into 无 sync、顺序不受控）：
//! 头 92 字节起（实际长度见规范 HeaderSize，CRC 覆盖该长度的字节）+ 条目 128 字节，
//! 均为 UEFI 规范布局；CRC（ISO-HDLC）对实际写盘的字节计算。
//!
//! 写原语的错误类型分两类，判据是调用方要不要区分**未写盘的事前拒绝（退出码 10）**与
//! **写盘后失败（30，须提示"盘可能已改变"）**：
//! - 需要区分 → 返回 `outcome::Fail`：写盘前的校验/形状拒绝写 `Fail::refused`，
//!   I/O 失败交给 `?`（`From<io::Error> for Fail` 落到 `Failed`，即安全缺省）
//! - 不需要（调用方一律按 30 处理）→ 保持 `io::Result`，避免无收益的类型搬运。
//!   例：commit_gpt 的调用方是 table 自己的写原语与写入路径（apply_repair / execute_apply /
//!   execute_resize）

mod entry_array;
mod error;
mod gpt_parse;
mod gpt_write;
mod mbr;
mod create;

#[cfg(test)]
mod tests;

pub(crate) use entry_array::*;
pub(crate) use error::*;
pub(crate) use gpt_parse::*;
pub(crate) use gpt_write::*;
pub(crate) use mbr::*;
pub(crate) use create::*;

use crate::dev::FileSource;
use crate::outcome::Fail;
use gptman::GPTPartitionEntry;
use std::io;