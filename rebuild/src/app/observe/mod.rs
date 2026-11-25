//! 已解析的用户意图。
//!
//! 只承载命令与参数，不含任何目标事实；观察层在 S1 起把请求与宿主端口合成为
//! 不可变的事实快照，判定层只消费快照。这里的类型在解析之后不再变化。

use std::path::{Path, PathBuf};

use diskedit_core::evidence::Generation;
use diskedit_core::fs::MkfsKind;
use diskedit_core::table::Guid;

pub mod new;
pub mod records;
pub mod snapshot;
pub mod table;

pub use new::{CapturedRegion, NewSnapshot, PriorRecord, PriorState, capture_new};
pub use snapshot::{
    CaptureError, InspectSnapshot, RecordSnapshot, Snapshot, TargetFacts, capture, capture_inspect,
    capture_records,
};

/// 分区表格式。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TableFormat {
    Gpt,
    Mbr,
}

/// 目标后缀 `:N` 与 `:last` 的解析结果。
///
/// `:last` 的选择发生在判定层：解析器只保留用户的选择，不猜目标上的分区数。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PartitionSelector {
    /// 明确的分区号，1 起。
    Number(u32),
    /// 目标上的最后一个分区。
    Last,
}

/// 一个目标参数：路径与可选的分区选择器。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TargetSpec {
    pub path: PathBuf,
    pub partition: Option<PartitionSelector>,
}

impl TargetSpec {
    pub fn new(path: impl Into<PathBuf>, partition: Option<PartitionSelector>) -> Self {
        Self {
            path: path.into(),
            partition,
        }
    }
}

/// 已解析的用户意图。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Request {
    target: Option<TargetSpec>,
    sector_size: Option<u64>,
    identity: Option<String>,
    command: Command,
}

impl Request {
    pub fn new(target: TargetSpec, command: Command) -> Self {
        Self {
            target: Some(target),
            sector_size: None,
            identity: None,
            command,
        }
    }

    /// 没有目标参数的请求，目前只有 `records list`。
    pub fn without_target(command: Command) -> Self {
        Self {
            target: None,
            sector_size: None,
            identity: None,
            command,
        }
    }

    /// 指定逻辑扇区字节数；判定与执行都以它对齐。
    pub fn with_sector_size(mut self, sector_size: Option<u64>) -> Self {
        self.sector_size = sector_size;
        self
    }

    /// 用户声明的身份名（`--identity`）：仅用于自动身份不可得的目标。
    pub fn with_identity(mut self, identity: Option<String>) -> Self {
        self.identity = identity;
        self
    }

    pub fn identity(&self) -> Option<&str> {
        self.identity.as_deref()
    }

    pub fn sector_size(&self) -> Option<u64> {
        self.sector_size
    }

    pub fn target_spec(&self) -> Option<&TargetSpec> {
        self.target.as_ref()
    }

    pub fn target(&self) -> Option<&Path> {
        self.target.as_ref().map(|spec| spec.path.as_path())
    }

    /// 目标的分区选择器；没有目标或未指定分区时为 `None`。
    pub fn partition_selector(&self) -> Option<PartitionSelector> {
        self.target.as_ref().and_then(|spec| spec.partition)
    }

    pub fn command(&self) -> &Command {
        &self.command
    }
}

/// 16 个入口及各自的参数。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Command {
    /// 整盘信息；`grow` 为真的形态只接受分区目标并给出扩容预览。
    Info {
        grow: bool,
    },
    Inspect,
    RecordsList,
    RecordsVerify {
        token: String,
    },
    RecordsScan,
    Resize(ResizeRequest),
    Move(MoveRequest),
    Copy(CopyRequest),
    Delete {
        partition: PartitionSelector,
    },
    Set {
        partition: PartitionSelector,
        action: SetAction,
    },
    Check {
        partition: PartitionSelector,
    },
    Mkfs {
        partition: PartitionSelector,
        filesystem: MkfsKind,
        label: Option<String>,
    },
    Resizefs {
        partition: Option<PartitionSelector>,
        bytes: Option<u64>,
    },
    Recover(RecoverMode),
    New {
        table: TableFormat,
        overwrite: bool,
    },
    Add(AddRequest),
}

/// `resize` 的 SIZE 写法。
///
/// 增量与百分比保留原始形态，判定层在锁下用当前分区字节数解析，锁前算出的绝对值
/// 可能已经作废。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SizeSpec {
    /// `grow`：吃满右侧空闲。
    Grow,
    /// 绝对字节数。
    Absolute(u64),
    /// `+N` / `-N`：相对当前分区大小。
    Delta { grow: bool, bytes: u64 },
    /// `+N%` / `-N%`：相对当前分区大小的百分比增量。
    Percent { grow: bool, num: u64, denom: u64 },
}

/// `resize` 的参数。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResizeRequest {
    pub partition: Option<PartitionSelector>,
    /// 位置参数 SIZE 的原始形态。
    pub size: Option<SizeSpec>,
    /// `--end` 指定的新终点 LBA。
    pub end: Option<u64>,
    pub grow_to_end: bool,
    pub align: Option<u64>,
    pub allow_move: bool,
    pub chunk: Option<u64>,
    pub grow_lv: bool,
    pub lv: Option<String>,
    pub no_fs: bool,
    pub dry_run: bool,
}

/// `move` 的参数。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MoveRequest {
    pub partition: PartitionSelector,
    pub start: StartSpec,
    pub align: Option<u64>,
    pub chunk: Option<u64>,
}

/// `copy` 的参数。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CopyRequest {
    pub partition: PartitionSelector,
    pub start: StartSpec,
    pub align: Option<u64>,
    pub chunk: Option<u64>,
    pub name: Option<String>,
    pub keep_partuuid: bool,
}

/// `add` 的参数；起点、对齐与类型都留待判定层补默认。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AddRequest {
    /// 要完整容纳的分区尺寸，字节。
    pub size: u64,
    pub start: Option<u64>,
    pub align: Option<u64>,
    pub part_number: Option<u32>,
    pub name: Option<String>,
    pub type_guid: Option<Guid>,
}

/// `--start` 的三种取值。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StartSpec {
    /// 明确的起点 LBA。
    Lba(u64),
    /// 紧贴可用区间末尾。
    End,
    /// 从可用区间起点按对齐放置。
    Align,
}

/// `set` 的属性形态。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SetAction {
    Name(String),
    Label(String),
    Uuid { value: Option<String>, random: bool },
    Flag { flag: String, on: bool },
    Type(String),
}

/// `recover` 的三种形态。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecoverMode {
    /// 回滚未完成的记录。
    Default,
    /// 只回放表区原像。
    ReplayTableOnly,
    /// 放弃回滚；`discard_replay` 表示同时放弃可重放区间。
    Release { discard_replay: bool },
}

/// `set` 可修改的分区属性。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SetProperty {
    Name,
    Label,
    Uuid,
    Flag,
    Type,
}

/// 首次观察使用的代次。执行期重探所得代次不得早于此值。
pub const FIRST_GENERATION: Generation = Generation::new(1);
