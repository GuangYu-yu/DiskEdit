//! 平台端口：中立类型、语义化错误与工厂。
//!
//! 本文件不出现具体操作系统接口、系统路径或外部程序名。`decide` 只读这里的
//! [`Occupancy`] 与 [`Partition`]，`execute` 只经 [`TargetHandle`] 与 [`Platform`]
//! 落盘，`report` 只读 [`PlatformError::io_kind`] 的语义分类；任何"这里该跑哪个
//! 命令、该读哪个虚拟文件"的判断都留在 `linux` 子树里。
//!
//! 实现按操作系统分目录：当前只有 `linux`；新增平台时补同形状的子树并扩展
//! [`open`]，上层不需要改动。

use std::fmt;
use std::path::{Path, PathBuf};

use diskedit_core::fingerprint::Fingerprint;
use diskedit_core::fs::Program;

#[cfg(target_os = "linux")]
pub mod linux;

/// 文件交互失败的语义分类。
///
/// 上层按这些类别决定文案与退出码，不接触具体错误码。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IoKind {
    NotFound,
    Permission,
    Busy,
    Unsupported,
    Other,
}

/// 平台层错误。
#[derive(Debug)]
pub enum PlatformError {
    /// 与文件、设备或虚拟文件系统的交互失败。
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    /// 当前平台或目标形态不具备该能力，调用方不应把它当作失败重试。
    NotApplicable { why: String },
    /// 目标存在，但本实现拒绝执行该动作。
    Unsupported { why: String },
    /// 目标被占用或被锁住。
    Busy { why: String },
    /// 权限不足。
    Denied { why: String },
    /// 读到的文本或结构不符合预期。
    Parse { what: String },
    /// 外部程序缺失、启动失败或拒绝执行。
    Tools { program: String, why: String },
}

impl PlatformError {
    pub fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Self::Io {
            path: path.into(),
            source,
        }
    }

    pub fn not_applicable(why: impl Into<String>) -> Self {
        Self::NotApplicable { why: why.into() }
    }

    pub fn unsupported(why: impl Into<String>) -> Self {
        Self::Unsupported { why: why.into() }
    }

    pub fn busy(why: impl Into<String>) -> Self {
        Self::Busy { why: why.into() }
    }

    pub fn denied(why: impl Into<String>) -> Self {
        Self::Denied { why: why.into() }
    }

    pub fn parse(what: impl Into<String>) -> Self {
        Self::Parse { what: what.into() }
    }

    pub fn tools(program: impl Into<String>, why: impl Into<String>) -> Self {
        Self::Tools {
            program: program.into(),
            why: why.into(),
        }
    }

    /// 文件错误的语义分类；非文件错误为 `None`。
    pub fn io_kind(&self) -> Option<IoKind> {
        match self {
            Self::Io { source, .. } => Some(classify_io(source.kind())),
            Self::NotApplicable { .. }
            | Self::Unsupported { .. }
            | Self::Busy { .. }
            | Self::Denied { .. }
            | Self::Parse { .. }
            | Self::Tools { .. } => None,
        }
    }
}

fn classify_io(kind: std::io::ErrorKind) -> IoKind {
    match kind {
        std::io::ErrorKind::NotFound => IoKind::NotFound,
        std::io::ErrorKind::PermissionDenied => IoKind::Permission,
        std::io::ErrorKind::WouldBlock => IoKind::Busy,
        std::io::ErrorKind::Unsupported => IoKind::Unsupported,
        _ => IoKind::Other,
    }
}

impl fmt::Display for PlatformError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { path, source } => write!(f, "读写 {} 失败：{source}", path.display()),
            Self::NotApplicable { why } => write!(f, "当前环境不适用：{why}"),
            Self::Unsupported { why } => write!(f, "能力不支持：{why}"),
            Self::Busy { why } => write!(f, "目标被占用：{why}"),
            Self::Denied { why } => write!(f, "权限不足：{why}"),
            Self::Parse { what } => write!(f, "解析失败：{what}"),
            Self::Tools { program, why } => write!(f, "外部程序 {program} 执行失败：{why}"),
        }
    }
}

impl std::error::Error for PlatformError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::NotApplicable { .. }
            | Self::Unsupported { .. }
            | Self::Busy { .. }
            | Self::Denied { .. }
            | Self::Parse { .. }
            | Self::Tools { .. } => None,
        }
    }
}

pub type PlatformResult<T> = Result<T, PlatformError>;

/// 目标的种类。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TargetKind {
    /// 普通文件，含镜像与 loop 的后备文件。
    File,
    /// 块设备。
    Block,
}

/// 一个分区的起点与长度，单位 512 字节扇区。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Partition {
    pub number: u32,
    pub start_lba: u64,
    pub sectors: u64,
}

/// 一条挂载记录。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MountEntry {
    pub major: u32,
    pub minor: u32,
    pub mount_point: PathBuf,
}

impl MountEntry {
    pub fn device(&self) -> String {
        format!("{}:{}", self.major, self.minor)
    }
}

/// 一个已启用的交换区。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SwapEntry {
    pub path: PathBuf,
    pub kind: String,
}

/// 目标的占用视图。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Occupancy {
    pub mounts: Vec<MountEntry>,
    pub swaps: Vec<SwapEntry>,
    pub holders: Vec<String>,
    pub array_member: bool,
    pub loops: Vec<String>,
}

impl Occupancy {
    /// 任一证据表明目标被占用。
    pub fn is_occupied(&self) -> bool {
        !self.mounts.is_empty()
            || !self.swaps.is_empty()
            || !self.holders.is_empty()
            || self.array_member
            || !self.loops.is_empty()
    }
}

/// 外部程序的退出结果。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExitKind {
    Succeeded,
    Failed(i32),
    Signaled(i32),
    Unknown,
}

/// 外部程序的输出。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Output {
    pub exit: ExitKind,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

impl Default for ExitKind {
    fn default() -> Self {
        Self::Unknown
    }
}

impl Output {
    pub fn succeeded(&self) -> bool {
        self.exit == ExitKind::Succeeded
    }

    pub fn stdout_text(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }

    pub fn stderr_text(&self) -> String {
        String::from_utf8_lossy(&self.stderr).into_owned()
    }
}

/// 已打开目标的读写句柄。
pub trait TargetHandle {
    fn path(&self) -> &Path;
    fn kind(&self) -> TargetKind;
    fn len(&self) -> u64;
    fn sector_size(&self) -> Option<u64>;
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> PlatformResult<usize>;
    fn write_at(&mut self, offset: u64, buf: &[u8]) -> PlatformResult<usize>;
    fn barrier(&mut self) -> PlatformResult<()>;
    /// 取得整盘独占租约；已被占用时返回 [`PlatformError::Busy`]。
    fn lock_exclusive(&mut self) -> PlatformResult<()>;
}

/// 按偏移读取。
pub trait ReadAt {
    fn len(&self) -> PlatformResult<u64>;
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> PlatformResult<usize>;
}

/// 按偏移写入并提交。
pub trait WriteAt: ReadAt {
    fn write_at(&mut self, offset: u64, buf: &[u8]) -> PlatformResult<usize>;
    fn barrier(&mut self) -> PlatformResult<()>;
}

/// 宿主能力端口。
pub trait Platform {
    fn open_target(&self, path: &Path) -> PlatformResult<Box<dyn TargetHandle>>;
    fn fingerprint(&self, path: &Path) -> PlatformResult<Fingerprint>;
    fn partitions(&self, path: &Path) -> PlatformResult<Vec<Partition>>;
    fn occupancy(&self, path: &Path) -> PlatformResult<Occupancy>;
    fn reread(&self, path: &Path) -> PlatformResult<()>;
    fn update_table(&self, path: &Path) -> PlatformResult<()>;
    fn btrfs_fsid(&self, path: &Path) -> PlatformResult<[u8; 16]>;
    fn loop_attach(&self, backing: &Path, offset: u64, size: u64) -> PlatformResult<PathBuf>;
    fn loop_detach(&self, device: &Path) -> PlatformResult<()>;
    fn mount(
        &self,
        source: &Path,
        mount_point: &Path,
        fstype: Option<&str>,
        options: &[&str],
    ) -> PlatformResult<()>;
    fn umount(&self, mount_point: &Path) -> PlatformResult<()>;
    fn require(&self, program: Program) -> PlatformResult<PathBuf>;
    fn run(&self, program: Program, args: &[&str]) -> PlatformResult<Output>;
}

/// 打开本平台的端口实现。
pub fn open() -> PlatformResult<Box<dyn Platform>> {
    #[cfg(target_os = "linux")]
    {
        Ok(Box::new(linux::Linux::new()))
    }
    #[cfg(not(target_os = "linux"))]
    {
        Err(PlatformError::not_applicable("当前平台尚无端口实现"))
    }
}
