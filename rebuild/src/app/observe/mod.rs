//! 观察层：把一次请求与宿主端口转成不可变的事实快照。
//!
//! 快照里每条事实都是 [`Observation`]，带来源与观测代次；读不到、判不出记
//! `Unknown`，不折叠成默认值或空集合。本层只读目标，不写盘、不做判定；文件
//! 系统尺寸需要运行只读探测程序取得，探测本身不改动目标。

pub mod disk;
pub mod filesystem;
pub mod identity;
pub mod snapshot;
pub mod table;
pub mod tools;

use std::path::{Path, PathBuf};

use diskedit_core::evidence::Generation;
use diskedit_core::fs::MkfsKind;
use diskedit_core::table::Guid;

use crate::platform::TargetHandle;

pub use snapshot::{Snapshot, observe};

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
///
/// 只承载命令与参数，不含任何目标事实；打开、识别与判定都发生在后续层。
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

    /// 替换目标：挂载点形态在平台反查后改写为整盘目标加分区选择器。
    pub fn with_target(mut self, target: TargetSpec) -> Self {
        self.target = Some(target);
        self
    }

    /// 替换命令：与 [`Request::with_target`] 在同一处改写，保持二者一致。
    pub fn with_command(mut self, command: Command) -> Self {
        self.command = command;
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

    pub fn command(&self) -> &Command {
        &self.command
    }
}

/// 16 个入口及各自的参数。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Command {
    Info,
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
        /// 由挂载点反查得到；离线形态为 `false`。
        mount: bool,
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

/// `set` 的六种属性形态。
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

/// 从目标按偏移读满一段。
///
/// 短读与 I/O 失败都折算成不可判定的原因文本：调用方据此产出 `Unknown`，
/// 不把残缺字节当成正常内容继续解释。
pub(super) fn read_exact_at(
    handle: &dyn TargetHandle,
    offset: u64,
    len: usize,
) -> Result<Vec<u8>, String> {
    let mut buffer = vec![0u8; len];
    let mut filled = 0;
    while filled < buffer.len() {
        let position = offset
            .checked_add(filled as u64)
            .ok_or_else(|| "read offset exceeds addressable bound".to_owned())?;
        match handle.read_at(position, &mut buffer[filled..]) {
            Ok(0) => return Err(format!("target ended early at offset {position}")),
            Ok(read) => filled += read,
            Err(error) => return Err(format!("failed to read at offset {position}: {error}")),
        }
    }
    Ok(buffer)
}
