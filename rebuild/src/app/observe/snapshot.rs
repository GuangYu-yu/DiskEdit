//! 事实快照：一次打开、一次身份与记录探测；完整快照另含分区表与占用。
//!
//! 判定层只消费快照；快照之后目标如何变化与判定无关。读失败如实向上报，不折叠
//! 成"没有表"或"没有记录"。不同命令只采集自己需要的事实：`records *` 不读分区表，
//! `inspect` 不读分区表，只有 `info`/`check` 需要完整视图。

use std::path::PathBuf;

use diskedit_core::fingerprint::{Fingerprint, Strength};

use crate::platform::{Occupancy, Platform, PlatformError, TargetKind};

use super::records::{self, RecordState};
use super::table::{self, TableView};
use super::{Command, Request};

/// 快照采集失败。
#[derive(Debug)]
pub enum CaptureError {
    /// 宿主端口失败。
    Platform(PlatformError),
    /// 请求没有目标。
    NoTarget,
    /// 目标装不下一个 512 字节扇区，无法开始读分区表。
    TooSmall { bytes: u64 },
    /// 读取目标字节失败。
    Unreadable { why: String },
    /// 身份无法解析。
    Identity(PlatformError),
    /// 用户声明的身份名不适用于这个目标：它自身已能证明身份，或不是块设备。
    DeclaredIdentityNotApplicable,
}

impl From<PlatformError> for CaptureError {
    fn from(error: PlatformError) -> Self {
        Self::Platform(error)
    }
}

impl std::fmt::Display for CaptureError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Platform(error) => write!(f, "{error}"),
            Self::NoTarget => f.write_str("the command needs a target"),
            Self::TooSmall { bytes } => {
                write!(
                    f,
                    "the target holds {bytes} bytes, less than one 512-byte sector"
                )
            }
            Self::Unreadable { why } => f.write_str(why),
            Self::Identity(error) => write!(f, "{error}"),
            Self::DeclaredIdentityNotApplicable => f.write_str(
                "--identity applies only to block devices with no readable WWN, EUI or repeatable \
                 serial; images and identified devices are addressed by their own identity",
            ),
        }
    }
}

/// 目标的基本事实。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TargetFacts {
    /// 用户给出的路径原文。
    pub display: String,
    pub path: PathBuf,
    pub bytes: u64,
    pub kind: TargetKind,
    /// 容器逻辑块大小；普通文件没有内核值时为 512。
    pub sector_size: u64,
    /// 目标自身证明的稳定身份；读不到或等级不足时为 `None`。
    pub identity: Option<Fingerprint>,
}

/// 一次完整观察的结果：`info` 与 `check` 消费。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Snapshot {
    pub target: TargetFacts,
    pub table: TableView,
    pub record: RecordState,
    pub occupancy: Occupancy,
}

impl Snapshot {
    /// 快照的逻辑块大小：表命中时用表的值，否则用容器的值。
    pub fn sector_size(&self) -> u64 {
        self.table.sector_size().unwrap_or(self.target.sector_size)
    }
}

/// `inspect` 消费的现场：目标、记录与占用，不含分区表。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InspectSnapshot {
    pub target: TargetFacts,
    pub record: RecordState,
    pub occupancy: Occupancy,
}

/// `records *` 消费的现场：目标与记录。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecordSnapshot {
    pub target: TargetFacts,
    pub record: RecordState,
}

/// 采集完整快照（`info`、`check`）。
pub fn capture(platform: &dyn Platform, request: &Request) -> Result<Snapshot, CaptureError> {
    let target = open_target(platform, request)?;
    let handle = platform.open_target_read_only(&target.path)?;
    let sector_size = handle
        .sector_size()
        .unwrap_or(request.sector_size().unwrap_or(512));
    if handle.len() < 512 {
        return Err(CaptureError::TooSmall {
            bytes: handle.len(),
        });
    }
    let table = table::observe(handle.as_ref(), request.sector_size(), handle.sector_size())
        .map_err(|error| CaptureError::Unreadable {
            why: error.to_string(),
        })?;
    let record = record_state(platform, &target)?;
    let occupancy = platform.occupancy(&target.path)?;
    Ok(Snapshot {
        target: TargetFacts {
            sector_size,
            ..target
        },
        table,
        record,
        occupancy,
    })
}

/// 采集 `inspect` 的现场：目标、记录与占用。
pub fn capture_inspect(
    platform: &dyn Platform,
    request: &Request,
) -> Result<InspectSnapshot, CaptureError> {
    let target = open_target(platform, request)?;
    let record = record_state(platform, &target)?;
    let occupancy = platform.occupancy(&target.path)?;
    Ok(InspectSnapshot {
        target,
        record,
        occupancy,
    })
}

/// 采集 `records *` 的现场：目标与记录。
pub fn capture_records(
    platform: &dyn Platform,
    request: &Request,
) -> Result<RecordSnapshot, CaptureError> {
    let target = open_target(platform, request)?;
    let record = record_state(platform, &target)?;
    Ok(RecordSnapshot { target, record })
}

/// 打开目标并解析身份；`--identity` 的适用性在这里判定一次。
pub(crate) fn open_target(
    platform: &dyn Platform,
    request: &Request,
) -> Result<TargetFacts, CaptureError> {
    let spec = request.target_spec().ok_or(CaptureError::NoTarget)?;
    let path = &spec.path;
    let declared = request.identity();
    // 只有 Strong 等级构成身份；不可用等级不参与记录定位，也不能与声明名争用。
    let strong = platform
        .identity(path)
        .ok()
        .filter(|fingerprint| fingerprint.strength() == Strength::Strong);
    // `--identity` 只命名自身证明不了身份的目标。`records list`/`verify` 与基线一样
    // 在打开目标之前先判这一条，因此目标不存在时也报用法错误而不是拒绝。
    let declares =
        |kind: TargetKind| declared.is_some() && !(kind.is_block_device() && strong.is_none());
    let early = matches!(
        request.command(),
        Command::RecordsList | Command::RecordsVerify { .. }
    );
    if early && declares(platform.path_kind(path)) {
        return Err(CaptureError::DeclaredIdentityNotApplicable);
    }
    let handle = platform.open_target_read_only(path)?;
    let kind = handle.kind();
    if declares(kind) {
        return Err(CaptureError::DeclaredIdentityNotApplicable);
    }
    // 声明名只在自动身份不可得时生效；它与自动身份走同一个记录键空间。
    let identity = declared.map(Fingerprint::declared).or(strong);
    let sector_size = handle.sector_size().unwrap_or(512);
    Ok(TargetFacts {
        display: path.display().to_string(),
        path: path.clone(),
        bytes: handle.len(),
        kind,
        sector_size,
        identity,
    })
}

/// 按目标身份或声明名定位记录。
pub(crate) fn record_state(
    platform: &dyn Platform,
    target: &TargetFacts,
) -> Result<RecordState, CaptureError> {
    records::locate(platform, target.identity).map_err(CaptureError::from)
}
