//! 分区表：GPT/MBR 解析 + 崩溃安全写入。
//!
//! 读取自实现（主备头回退与扇区大小候选探测见 `gpt_parse`）；`gptman` 只作类型引用。
//! 写入自行序列化（gptman write_into 无 sync、顺序不受控）：
//! 头 92 字节起（实际长度见规范 HeaderSize，CRC 覆盖该长度的字节）+ 条目 128 字节，
//! 均为 UEFI 规范布局；CRC（ISO-HDLC）对实际写盘的字节计算。
//!
//! 写原语的错误类型分两类，判据是调用方要不要区分**未写盘的事前拒绝（退出码 3）**与
//! **写盘后失败（5，须提示"盘可能已改变"）**：
//! - 需要区分 → 返回 `outcome::RunOutcome`：写盘前的校验/形状拒绝写 `RunOutcome::refused`，
//!   I/O 失败交给 `?`（`From<io::Error> for RunOutcome` 落到 `Partial`，即安全缺省）
//! - 不需要（调用方一律按 5 处理）→ 保持 `io::Result`，避免无收益的类型搬运。
//!
//! 落盘分区表的写原语另有写前重读复核（[`table_stamp`] / [`verify_table_stamp`]）：
//! 首次写盘前重读三扇区摘要与记录值比对，不等即不写，故这些原语返回 `RunOutcome`
//! 以保留 3 / 4 / 5 的分界。

mod entry_array;
mod error;
mod gpt_parse;
mod gpt_write;
mod mbr;
mod create;
mod stamp;
mod step;

#[cfg(test)]
mod tests;

pub(crate) use entry_array::*;
pub(crate) use error::*;
pub(crate) use gpt_parse::*;
pub(crate) use gpt_write::*;
pub(crate) use mbr::*;
pub(crate) use create::*;
pub(crate) use stamp::*;
pub(crate) use step::*;

use crate::dev::{FileSource, RegionStamp};
#[cfg(test)]
use crate::dev::WriteSource;
use crate::outcome::RunOutcome;
use gptman::GPTPartitionEntry;
use std::io;

/// UEFI 2.10 §5.3.2 Table 5.5 头部固定字段的字节数：HeaderSize 的下界，也是 CRC
/// 计算范围与新建表写出的长度。解析、写出与建表三处都从这一个值取
pub(crate) const GPT_MIN_HEADER_SIZE: u32 = 92;

/// 主 GPT 头的 LBA 与主条目数组的起始 LBA（UEFI 2.10 §5.3.2：主头在 LBA1，
/// 数组紧随其后从 LBA2 起）。解析、建表与回放共用这两个值
pub(crate) const PRIMARY_HEADER_LBA: u64 = 1;
pub(crate) const PRIMARY_ARRAY_LBA: u64 = 2;