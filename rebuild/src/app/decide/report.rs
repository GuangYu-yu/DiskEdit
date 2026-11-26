//! 只读报告的值：判定层产出，报告层渲染。
//!
//! 这些结构只含事实与已算好的结论，不含 argv、路径格式化或系统调用；JSON 字段名
//! 由报告层决定。

use std::path::PathBuf;

use diskedit_core::fs::{ContentKind, Program};

use crate::platform::Occupancy;

use super::step::Step;

/// 一次只读判定的报告。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Report {
    Info(InfoReport),
    Inspect(InspectReport),
    Records(RecordsReport),
    Check(CheckQuery),
    Resize(ResizePreview),
}

/// `resize --dry-run` 的计算预览：只给区间与文件系统动作，不写任何字节。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResizePreview {
    pub partition: u32,
    pub start_lba: u64,
    pub old_end_lba: u64,
    pub new_end_lba: u64,
    pub sector_size: u64,
    /// 文件系统动作；`None` 表示本次只改表（`--no-fs` 或无文件系统）。
    pub filesystem: Option<ContentKind>,
    /// 动作是扩容还是收缩。
    pub grow: bool,
}

/// `info` 的整盘视图。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InfoReport {
    pub label: InfoLabel,
    pub damaged: bool,
    pub sector_size: u64,
    pub size_bytes: u64,
    pub disk_guid: Option<String>,
    pub last_part: Option<u32>,
    pub partitions: Vec<InfoPartition>,
    pub preview: Option<GrowPreview>,
}

/// 表形态的展示名。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InfoLabel {
    Gpt,
    Mbr,
    None,
}

impl InfoLabel {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Gpt => "gpt",
            Self::Mbr => "mbr",
            Self::None => "none",
        }
    }
}

/// 一个分区的展示视图。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InfoPartition {
    pub number: u32,
    pub first_lba: u64,
    pub last_lba: u64,
    pub size_bytes: u64,
    pub type_text: String,
    pub content: ContentKind,
    pub name: String,
}

/// `info --grow` 的扩容预览。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GrowPreview {
    pub partition: u32,
    pub new_end_lba: u64,
    pub sector_size: u64,
    pub moves: Vec<GrowMove>,
}

/// 预览中被右移的分区。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GrowMove {
    pub number: u32,
    pub first_lba: u64,
    pub last_lba: u64,
    pub delta_lba: u64,
    pub bytes: u64,
    pub swap: bool,
}

/// `inspect` 的现场视图。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InspectReport {
    pub evidence: Option<PathBuf>,
    pub record: RecordVerdict,
    pub occupancy: Occupancy,
}

/// 记录文件的存在与可读性。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RecordVerdict {
    /// 目标给不出记录目录的键，没有可检查的记录位置。
    Unresolved {
        why: String,
    },
    Absent,
    Present {
        bytes: u64,
        decoded: String,
    },
}

/// `records` 的列表视图。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecordsReport {
    pub root: Option<PathBuf>,
    pub records: Vec<RecordEntry>,
    pub scanned: Option<PathBuf>,
    pub complete: bool,
}

/// 一条记录的摘要。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecordEntry {
    pub id: String,
    pub path: PathBuf,
    pub bytes: u64,
    pub decoded: String,
}

/// `check` 的执行请求。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CheckQuery {
    pub program: Program,
    pub partition: u32,
    pub content: ContentKind,
    pub target: CheckTarget,
}

/// 检查目标在宿主上的形态。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CheckTarget {
    /// 整盘块设备上的第 N 个分区，执行前解析为分区设备节点。
    PartitionDevice { disk: PathBuf, number: u32 },
    /// 镜像内的一段：执行前绑定为临时块设备。
    ImageRegion {
        path: PathBuf,
        offset: u64,
        len: u64,
    },
}

/// 把一份只读判定包装成 `Report` 决策。
pub fn decision(report: Report, steps: Vec<Step>) -> super::Decision {
    super::Decision::Report {
        plan: super::Plan::read_only(steps),
        report,
    }
}
