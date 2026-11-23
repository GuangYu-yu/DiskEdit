//! 平台层：目标打开与独占、内核视图、外部工具与探测。
//!
//! 本层负责一切与操作系统、进程和外部程序打交道的工作，向上只暴露端口；
//! 依赖方向为 `platform → core`，本层不引用 `app`。
//!
//! 端口的划分：按偏移读写以 [`ReadAt`] 与 [`WriteAt`] 收口，目标独占以 [`Lease`] 收口，
//! 识别结论以 `diskedit_core::evidence` 的类型返回。文件描述符、ioctl 编号、sysfs
//! 路径与子进程参数拼装只在 `platform` 内可见，层外只能经本文件导出的类型与函数使用。
//! 平台无法完成的能力显式返回 [`PlatformError::NotApplicable`] 或
//! [`PlatformError::Unsupported`]，不返回空结果冒充成功。

use std::fmt;
use std::path::{Path, PathBuf};

pub mod dm;
pub mod kernel;
pub mod loopdev;
pub mod mount;
pub mod occupancy;
pub mod probe;
pub mod source;
pub mod target;
pub mod tools;

pub use dm::DmStatus;
pub use kernel::reread::reread;
pub use loopdev::LoopDevice;
pub use mount::is_mounted_at;
pub use occupancy::Occupancy;
pub use source::{BlockSource, FileSource, Lease, ReadAt, WriteAt};
pub use target::{Partition, Target, TargetKind};
pub use tools::Registry;

/// 平台层错误。
#[derive(Debug)]
pub enum PlatformError {
    /// 与文件、设备或 sysfs 的交互失败；`path` 指向出错的落点。
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    /// 当前平台或目标形态不具备该能力，调用方不应把它当作失败重试。
    NotApplicable { why: String },
    /// 能力被明确拒绝：目标存在，但本实现不执行该动作。
    Unsupported { why: String },
    /// 读到的文本或结构不符合预期；`what` 说明解析对象。
    Parse { what: String },
    /// 外部程序缺失、启动失败或拒绝执行；`program` 为可执行名。
    Tools { program: String, why: String },
}

impl PlatformError {
    /// 文件交互失败。
    pub fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Self::Io {
            path: path.into(),
            source,
        }
    }

    /// 当前平台或目标形态不具备该能力。
    pub fn not_applicable(why: impl Into<String>) -> Self {
        Self::NotApplicable { why: why.into() }
    }

    /// 目标存在但本实现拒绝执行。
    pub fn unsupported(why: impl Into<String>) -> Self {
        Self::Unsupported { why: why.into() }
    }

    /// 解析失败。
    pub fn parse(what: impl Into<String>) -> Self {
        Self::Parse { what: what.into() }
    }

    /// 外部程序失败。
    pub fn tools(program: impl Into<String>, why: impl Into<String>) -> Self {
        Self::Tools {
            program: program.into(),
            why: why.into(),
        }
    }
}

impl fmt::Display for PlatformError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { path, source } => {
                write!(f, "读写 {} 失败：{source}", path.display())
            }
            Self::NotApplicable { why } => write!(f, "当前环境不适用：{why}"),
            Self::Unsupported { why } => write!(f, "能力不支持：{why}"),
            Self::Parse { what } => write!(f, "解析失败：{what}"),
            Self::Tools { program, why } => {
                write!(f, "外部程序 {program} 执行失败：{why}")
            }
        }
    }
}

impl std::error::Error for PlatformError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::NotApplicable { .. }
            | Self::Unsupported { .. }
            | Self::Parse { .. }
            | Self::Tools { .. } => None,
        }
    }
}

/// 平台层函数的返回类型。
pub type PlatformResult<T> = Result<T, PlatformError>;

/// sysfs 根目录。生产取 `/sys`，测试经环境变量指向构造的目录树。
pub fn sysfs_root() -> PathBuf {
    root_from_env("DISKEDIT_SYSFS_ROOT", "/sys")
}

/// procfs 根目录。生产取 `/proc`，测试经环境变量指向构造的目录树。
pub fn proc_root() -> PathBuf {
    root_from_env("DISKEDIT_PROC_ROOT", "/proc")
}

fn root_from_env(name: &str, fallback: &str) -> PathBuf {
    match std::env::var_os(name) {
        Some(value) if !value.is_empty() => PathBuf::from(value),
        Some(_) => PathBuf::from(fallback),
        None => PathBuf::from(fallback),
    }
}

/// 读取 sysfs 或 procfs 的文本属性。
///
/// 文件不存在表示"该属性没有提供"（`Ok(None)`）；其余读取失败如实返回错误。
/// 存在但内容为空返回 `Ok(Some(""))`，调用方据空串判断"提供了但没有值"。
pub(crate) fn read_optional_attr(path: &Path) -> PlatformResult<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text.trim().to_string())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(PlatformError::io(path, error)),
    }
}

/// 读取一个必须存在的属性；不存在按解析失败报告，由调用方决定处置。
pub(crate) fn read_required_attr(path: &Path) -> PlatformResult<String> {
    read_optional_attr(path)?.ok_or_else(|| PlatformError::parse(format!("{} 不存在", path.display())))
}

/// 已打开的 fd 当前指向的路径。
///
/// 仅用于错误消息定位；取不到时给出 fd 目录名，不据此做任何判定。
#[cfg(unix)]
pub(crate) fn fd_path(file: &std::fs::File) -> PathBuf {
    use std::os::fd::AsRawFd;
    let link = format!("/proc/self/fd/{}", file.as_raw_fd());
    match std::fs::read_link(&link) {
        Ok(path) => path,
        Err(_) => PathBuf::from(link),
    }
}

/// 非 Unix 平台没有 `/proc/self/fd`，错误消息只给出固定落点名。
#[cfg(not(unix))]
pub(crate) fn fd_path(_file: &std::fs::File) -> PathBuf {
    PathBuf::from("<open file>")
}
