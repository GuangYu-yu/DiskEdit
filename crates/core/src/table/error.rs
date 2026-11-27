//! 分区表错误：解析、几何校验与编解码的失败原因。
//!
//! 每个变体只描述一件已判定的事实；调用方据此决定拒绝、修复还是回退，
//! 错误本身不携带策略。

use std::fmt;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TableError {
    /// 头部签名不是 `EFI PART`。
    MissingSignature,
    /// 头部长度小于固定字段数或不大于扇区长度。
    InvalidHeaderSize(u32),
    /// 头部 CRC 与计算值不符。
    HeaderCrcMismatch,
    /// 条目数组 CRC 与头内记录不符。
    EntryArrayCrcMismatch,
    /// 扇区或缓冲区不足以容纳所需字节。
    BufferTooSmall { needed: usize, actual: usize },
    /// 条目大小小于 128 字节。
    InvalidEntrySize(u32),
    /// 条目数超过实现上限。
    TooManyEntries { count: u32, max: u32 },
    /// 头字段落到容器之外。
    BeyondContainer {
        field: &'static str,
        value: u64,
        last_lba: u64,
    },
    /// 头字段彼此矛盾。
    InvalidGeometry(&'static str),
    /// 已定义条目的起止颠倒。
    InvalidEntry { index: usize, start: u64, end: u64 },
    /// 已定义条目越出可用区间。
    BeyondUsable {
        index: usize,
        start: u64,
        end: u64,
        first: u64,
        last: u64,
    },
    /// MBR 引导签名缺失。
    MbrSignatureMissing,
    /// 扩展链在给定上限内未收敛。
    ExtendedChainTooLong { max: usize },
    /// 扩展链出现环或回指自身。
    ExtendedChainLoop { lba: u64 },
}

impl fmt::Display for TableError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingSignature => write!(f, "GPT signature is absent"),
            Self::InvalidHeaderSize(size) => write!(f, "invalid GPT header size: {size}"),
            Self::HeaderCrcMismatch => write!(f, "GPT header CRC mismatch"),
            Self::EntryArrayCrcMismatch => write!(f, "GPT entry array CRC mismatch"),
            Self::BufferTooSmall { needed, actual } => {
                write!(f, "buffer too small: need {needed} bytes, got {actual}")
            }
            Self::InvalidEntrySize(size) => write!(f, "invalid partition entry size: {size}"),
            Self::TooManyEntries { count, max } => {
                write!(f, "too many partition entries: {count} > {max}")
            }
            Self::BeyondContainer {
                field,
                value,
                last_lba,
            } => write!(
                f,
                "{field} = {value} is beyond the container last LBA {last_lba}"
            ),
            Self::InvalidGeometry(detail) => write!(f, "invalid GPT geometry: {detail}"),
            Self::InvalidEntry { index, start, end } => {
                write!(f, "entry {index} has start {start} > end {end}")
            }
            Self::BeyondUsable {
                index,
                start,
                end,
                first,
                last,
            } => write!(
                f,
                "entry {index} [{start}, {end}] is outside the usable range [{first}, {last}]"
            ),
            Self::MbrSignatureMissing => write!(f, "MBR boot signature is absent"),
            Self::ExtendedChainTooLong { max } => {
                write!(f, "extended partition chain exceeds {max} entries")
            }
            Self::ExtendedChainLoop { lba } => {
                write!(f, "extended partition chain revisits LBA {lba}")
            }
        }
    }
}

impl std::error::Error for TableError {}
