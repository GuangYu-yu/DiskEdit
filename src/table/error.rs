//! 表解析错误的分类及其与 io::Error 的转换。

use super::*;

/// 解析层错误：结构非法的表在类型上不可继续当作正常 GPT 使用（load_gpt 只返回它）。
/// 只提供 `From<io::Error>` 这一个方向——**不提供 `From<GptError> for io::Error`**，
/// 因此"把结构化错误压平"必须经 into_io_error() 在每个调用点显式发生，不会被 `?` 静默吞掉。
#[derive(Debug)]
pub enum GptError {
    /// 底层读写失败
    Io(io::Error),
    /// 头部结构/几何不合法：签名、头尺寸、CRC、MyLBA、usable 上下界关系、条目数组几何
    InvalidHeader(String),
    /// 头部字段越过容器末端（镜像被截断/损坏）
    BeyondContainer {
        field: &'static str,
        value: u64,
        file_last_lba: u64,
    },
    /// 条目自身矛盾：starting_lba > ending_lba（index 为 1-based 分区序号）
    InvalidEntry {
        index: usize,
        start: u64,
        end: u64,
    },
    /// 条目越出 [FirstUsableLBA, LastUsableLBA]（UEFI 2.10 §5.3.1：已定义条目必须落在 usable range 内）
    BeyondUsable {
        index: usize,
        start: u64,
        end: u64,
        first: u64,
        last: u64,
    },
    /// 某一副本的条目数组字节与其头部自述的 CRC 不符：**该副本的数组**是数据损伤。
    /// 单列出来是因为它的可恢复性与前几类不同——另一份副本的数组是独立写入的，
    /// 可能完好，UEFI 2.10 §5.3.2 的主备互备正是为此；是否回退由 load_gpt 决定
    EntryArrayCorrupt { copy: GptCopyKind },
    /// 某一副本的头部字节与自身 CRC 不符（签名在而头不可用）：**该副本的头**是数据损伤。
    /// 与 EntryArrayCorrupt 分列是刻意的：损伤位置不同、对外措辞不同（头坏 vs 数组坏），
    /// 而可恢复性相同——另一份副本的头是独立写入的，可能完好，回退与否仍由 load_gpt 决定
    HeaderCorrupt { copy: GptCopyKind, detail: &'static str },
}

impl From<io::Error> for GptError {
    fn from(e: io::Error) -> Self {
        GptError::Io(e)
    }
}

impl std::fmt::Display for GptError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GptError::Io(e) => write!(f, "{e}"),
            GptError::InvalidHeader(s) => f.write_str(s),
            GptError::BeyondContainer { field, value, file_last_lba } => {
                write!(f, "{field} {value} beyond container end (last LBA {file_last_lba}) — truncated image")
            }
            GptError::InvalidEntry { index, start, end } => {
                write!(f, "partition {index}: starting_lba {start} > ending_lba {end} — invalid entry")
            }
            GptError::BeyondUsable { index, start, end, first, last } => {
                write!(f, "partition {index}: {start}..{end} outside usable range {first}..{last}")
            }
            GptError::EntryArrayCorrupt { copy } => {
                write!(f, "{} GPT entry array CRC mismatch", copy.label())
            }
            GptError::HeaderCorrupt { copy, detail } => {
                write!(f, "{} GPT header damaged: {detail}", copy.label())
            }
        }
    }
}

impl std::error::Error for GptError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            GptError::Io(e) => Some(e),
            _ => None,
        }
    }
}

/// 把 GptError 压平为 io::Error：只给"调用方只需知道这次被拒绝"的写入/查询层用。
/// cmd_info 不得使用——它必须直接匹配 GptError 变体做结构化诊断。
pub fn into_io_error(e: GptError) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, e.to_string())
}