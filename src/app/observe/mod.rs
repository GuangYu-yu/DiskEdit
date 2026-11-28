//! 已解析的用户意图。
//!
//! 只承载命令与参数，不含任何目标事实；观察层在 S1 起把请求与宿主端口合成为
//! 不可变的事实快照，判定层只消费快照。这里的类型在解析之后不再变化。

use std::path::{Path, PathBuf};

use diskedit_core::evidence::Generation;
use diskedit_core::fs::MkfsKind;
use diskedit_core::table::Guid;

pub mod edit;
pub mod filesystem;
pub mod new;
pub mod records;
pub mod resize;
pub mod snapshot;
pub mod table;

pub use edit::{EditSnapshot, TableFacts, capture_edit};
pub use filesystem::{FilesystemSnapshot, PartitionFacts, capture as capture_filesystem};
pub use new::{CapturedRegion, NewSnapshot, PriorRecord, PriorState, capture_new};
pub use resize::{ResizeSnapshot, capture_resize};
pub use snapshot::{
    CaptureError, RecordSiteSnapshot, RecordSnapshot, RecoverSnapshot, Snapshot, TargetFacts,
    capture, capture_record_site, capture_records, capture_recover,
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
    alias: Option<String>,
    /// 用户已给出 `--yes`。
    confirmed: bool,
    command: Command,
}

impl Request {
    pub fn new(target: TargetSpec, command: Command) -> Self {
        Self {
            target: Some(target),
            sector_size: None,
            alias: None,
            confirmed: false,
            command,
        }
    }

    /// 没有目标参数的请求，目前只有 `records list`。
    pub fn without_target(command: Command) -> Self {
        Self {
            target: None,
            sector_size: None,
            alias: None,
            confirmed: false,
            command,
        }
    }

    /// 记录确认旗标；解析期已保证写命令必须带 `--yes`。
    pub fn with_confirmation(mut self, confirmed: bool) -> Self {
        self.confirmed = confirmed;
        self
    }

    pub fn confirmed(&self) -> bool {
        self.confirmed
    }

    /// 指定逻辑扇区字节数；判定与执行都以它对齐。
    pub fn with_sector_size(mut self, sector_size: Option<u64>) -> Self {
        self.sector_size = sector_size;
        self
    }

    /// 用户声明的别名（`--alias`）：仅用于自动身份不可得的目标。
    pub fn with_alias(mut self, alias: Option<String>) -> Self {
        self.alias = alias;
        self
    }

    pub fn alias(&self) -> Option<&str> {
        self.alias.as_deref()
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
    /// 整盘布局；`json` 选择结构化明细输出。
    Layout {
        json: bool,
    },
    RecordsList {
        /// `--root ROOT`：扫描一个记录容器目录，而不是列目标自己的记录。
        root: Option<PathBuf>,
    },
    RecordsVerify {
        token: String,
    },
    RecordsShow,
    Resize(ResizeRequest),
    Move(MoveRequest),
    Copy(CopyRequest),
    Delete {
        partition: PartitionSelector,
    },
    Set(SetRequest),
    Check {
        partition: PartitionSelector,
    },
    Mkfs {
        partition: Option<PartitionSelector>,
        filesystem: MkfsKind,
        label: Option<String>,
    },
    Undo,
    Replay,
    Release {
        discard_replay: bool,
    },
    New {
        table: TableFormat,
        overwrite: bool,
    },
    Add(AddRequest),
}

impl Command {
    /// 入口名，与 [`crate::app::cli::declare::COMMANDS`] 的 `name` 逐字一致。
    ///
    /// 入口名取自命令面声明，这里只把枚举变体映射到那张表；两个方向覆盖的
    /// 入口是否一致由 `tests/inventory.rs` 核对。
    pub fn entry_name(&self) -> &'static str {
        match self {
            Self::Layout { .. } => "layout",
            Self::RecordsList { .. } => "records list",
            Self::RecordsVerify { .. } => "records verify",
            Self::RecordsShow => "records show",
            Self::Resize(_) => "resize",
            Self::Move(_) => "move",
            Self::Copy(_) => "copy",
            Self::Delete { .. } => "delete",
            Self::Set(_) => "set",
            Self::Check { .. } => "check",
            Self::Mkfs { .. } => "mkfs",
            Self::Undo => "undo",
            Self::Replay => "replay",
            Self::Release { .. } => "release",
            Self::New { .. } => "new",
            Self::Add(_) => "add",
        }
    }

    /// 这一形态是否只预览、不写盘。
    ///
    /// 只有显式 `--dry-run` 的形态是预览；其余写命令即便没有 `--yes` 也仍会写，
    /// 不能因此被当成只读。
    pub fn is_dry_run(&self) -> bool {
        match self {
            Self::Resize(request) => request.dry_run,
            Self::Add(request) => request.dry_run,
            Self::Layout { .. }
            | Self::RecordsList { .. }
            | Self::RecordsVerify { .. }
            | Self::RecordsShow
            | Self::Move(_)
            | Self::Copy(_)
            | Self::Delete { .. }
            | Self::Set(_)
            | Self::Check { .. }
            | Self::Mkfs { .. }
            | Self::Undo
            | Self::Replay
            | Self::Release { .. }
            | Self::New { .. } => false,
        }
    }
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
    /// 位置参数 SIZE 的原始形态；`--fs-only` 的挂载点形态用它携带可选的 BYTES。
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
    /// `--fs-only`：只动文件系统（离线长满分区，或挂载点在线扩容）。
    pub fs_only: bool,
    pub dry_run: bool,
}

/// `move` 的参数。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MoveRequest {
    pub partition: PartitionSelector,
    pub start: StartSpec,
    pub align: Alignment,
    /// 单块字节数；`None` 表示未给出，按默认 4 MiB。
    pub chunk: Option<u64>,
}

/// `copy` 的参数。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CopyRequest {
    pub partition: PartitionSelector,
    pub start: StartSpec,
    pub align: Alignment,
    /// 单块字节数；`None` 表示未给出，按默认 4 MiB。
    pub chunk: Option<u64>,
    pub name: Option<String>,
    pub keep_partuuid: bool,
}

/// `add` 的参数；起点、对齐与类型都留待判定层补默认。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AddRequest {
    /// 要完整容纳的分区尺寸，字节。
    pub size: u64,
    pub start: Option<StartSpec>,
    pub align: Alignment,
    pub part_number: Option<u32>,
    pub name: Option<String>,
    pub type_guid: Option<TypeSelector>,
    /// `--dry-run`：只给出落位预览，不写盘。
    pub dry_run: bool,
}

/// 自动放置的对齐策略。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Alignment {
    /// 未给出 `--align`：按 1 MiB 自动放置。
    Default,
    /// `--align none`：按给定 LBA 落位。
    None,
    /// `--align mib`：按 1 MiB 边界。
    Mib,
}

/// `--type-guid` 的两种写法；表型由判定层判断。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TypeSelector {
    Guid(Guid),
    Mbr(u8),
}

/// `set` 的属性名。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SetKey {
    Name,
    Label,
    Uuid,
    Flag,
    Type,
}

impl SetKey {
    pub fn from_text(text: &str) -> Option<Self> {
        match text {
            "name" => Some(Self::Name),
            "label" => Some(Self::Label),
            "uuid" => Some(Self::Uuid),
            "flag" => Some(Self::Flag),
            "type" => Some(Self::Type),
            _ => None,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Name => "name",
            Self::Label => "label",
            Self::Uuid => "uuid",
            Self::Flag => "flag",
            Self::Type => "type",
        }
    }
}

/// `set` 的参数。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SetRequest {
    pub partition: PartitionSelector,
    pub key: SetKey,
    /// 属性值；`uuid` 可省略。
    pub value: String,
    /// `flag` 的 `on|off`；其余属性必须为空。
    pub state: String,
    /// `uuid` 的 `--random`。
    pub random: bool,
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

/// 恢复步骤的三种形态：回滚、只回放表区原像、放弃。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecoverMode {
    /// 回滚未完成的记录（`undo`）。
    Undo,
    /// 只回放表区原像（`replay`）。
    ReplayTableOnly,
    /// 放弃回滚；`discard_replay` 表示同时放弃可重放区间（`release`）。
    Release { discard_replay: bool },
}

/// `set` 写入的文件系统内容属性。
///
/// 分区表属性（name / type / flag）由整表写入完成，不经过这里；内容属性只改分区
/// 内部的卷标或 UUID，因此两者是两种形态。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContentProperty {
    Label,
    Uuid,
}

/// 首次观察使用的代次。执行期重探所得代次不得早于此值。
pub const FIRST_GENERATION: Generation = Generation::new(1);
