//! 宿主端口：上层只通过本模块的中立接口触碰操作系统。
//!
//! 平台选择与具体系统调用只出现在本层；`app` 与纯核不引用任何平台名。
//! 端口只暴露事实与动作：句柄、扇区大小、身份、占用、外部程序与记录根目录。

pub mod linux;
pub mod probe;
pub mod tools;

use std::fmt;
use std::path::{Path, PathBuf};

use diskedit_core::fingerprint::Fingerprint;
use diskedit_core::fs::Program;

pub use linux::LinuxPlatform;

/// 当前宿主的端口实现。平台选择只在这里发生。
pub fn host() -> LinuxPlatform {
    LinuxPlatform
}

/// 端口操作失败。
#[derive(Debug)]
pub enum PlatformError {
    /// 目标或系统调用返回的 I/O 错误。
    Io { path: String, message: String },
    /// 本平台不提供该能力。
    NotSupported(String),
    /// 无法解释系统返回的数据。
    Parse(String),
    /// 当前对象不适用该操作。
    NotApplicable(String),
}

impl PlatformError {
    /// 以路径与底层错误构造 I/O 失败。
    pub fn io(path: &Path, error: std::io::Error) -> Self {
        Self::Io {
            path: path.display().to_string(),
            message: error.to_string(),
        }
    }

    /// 本平台不提供该能力。
    pub fn not_supported(why: impl Into<String>) -> Self {
        Self::NotSupported(why.into())
    }

    /// 无法解释系统返回的数据。
    pub fn parse(why: impl Into<String>) -> Self {
        Self::Parse(why.into())
    }

    /// 当前对象不适用该操作。
    pub fn not_applicable(why: impl Into<String>) -> Self {
        Self::NotApplicable(why.into())
    }
}

impl fmt::Display for PlatformError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { path, message } => write!(f, "{path}: {message}"),
            Self::NotSupported(why) | Self::Parse(why) | Self::NotApplicable(why) => {
                f.write_str(why)
            }
        }
    }
}

impl std::error::Error for PlatformError {}

/// 端口调用的结果。
pub type PlatformResult<T> = Result<T, PlatformError>;

/// 目标在宿主上的存在形态。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TargetKind {
    /// 普通文件（含镜像）。
    File,
    /// 块设备（整盘或单个分区）。
    BlockDevice,
    /// 其他对象：目录、管道、字符设备等。
    Other,
}

impl TargetKind {
    pub const fn is_block_device(self) -> bool {
        matches!(self, Self::BlockDevice)
    }
}

/// 目标在宿主上的占用结论。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Occupancy {
    /// 没有任何占用者或占用来源。
    Idle,
    /// 已被占用，附带来源说明。
    Used { evidence: String },
    /// 无法判定，附带原因。
    Unknown { why: String },
}

/// 外部程序的一次运行结果。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolRun {
    /// 进程退出码；被信号终止时为 `None`。
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

/// 临时绑定到目标的块设备视图；销毁时解除绑定。
#[derive(Debug)]
pub struct LoopBinding {
    device: PathBuf,
}

impl LoopBinding {
    pub fn device(&self) -> &Path {
        &self.device
    }

    pub(crate) fn new(device: PathBuf) -> Self {
        Self { device }
    }
}

/// 目标的打开方式由具体平台决定，上层只拿到这个按偏移读写的句柄。
pub trait TargetHandle {
    /// 打开时使用的路径。
    fn path(&self) -> &Path;

    /// 打开时读到的字节长度。
    fn len(&self) -> u64;

    /// 目标的宿主形态。
    fn kind(&self) -> TargetKind;

    /// 块设备的逻辑扇区字节数；普通文件没有这一事实。
    fn sector_size(&self) -> Option<u64>;

    /// 目标是否为空。
    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// 按偏移读取，返回实际读到的字节数。
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> PlatformResult<usize>;

    /// 按偏移写入，返回实际写入的字节数。
    fn write_at(&mut self, offset: u64, buf: &[u8]) -> PlatformResult<usize>;

    /// 提交目标缓存。
    fn barrier(&mut self) -> PlatformResult<()>;

    /// 让内核重读分区表与块设备尺寸；普通文件无事可做。
    fn refresh_partitions(&mut self) -> PlatformResult<()>;
}

/// 记录目录上的独占锁；销毁时释放。
pub trait StateLock {}

/// 宿主端口。
pub trait Platform: Send + Sync {
    /// 以读写方式打开目标。
    fn open_target(&self, path: &Path) -> PlatformResult<Box<dyn TargetHandle>>;

    /// 以只读方式打开目标。
    fn open_target_read_only(&self, path: &Path) -> PlatformResult<Box<dyn TargetHandle>>;

    /// 目标的宿主形态；只读元数据，不打开文件。
    fn path_kind(&self, path: &Path) -> TargetKind;

    /// 目标本身是不是块设备上的分区节点（`/dev/sda1`），而不是整盘。
    fn is_partition_node(&self, path: &Path) -> bool;

    /// 目标的稳定身份；镜像取文件对象，块设备取设备本身。
    fn identity(&self, path: &Path) -> PlatformResult<Fingerprint>;

    /// 目标的占用结论。
    fn occupancy(&self, path: &Path) -> PlatformResult<Occupancy>;

    /// 记录根目录；宿主没有可写状态目录时返回 `None`。
    fn state_root(&self) -> Option<PathBuf>;

    /// 路径是否为目录。
    fn is_directory(&self, path: &Path) -> bool;

    /// 递归列出目录下的普通文件；根不存在或不可读时返回错误。
    fn list_files(&self, root: &Path) -> PlatformResult<Vec<PathBuf>>;

    /// 读取一个普通文件；不存在时返回 `None`。
    fn read_file(&self, path: &Path) -> PlatformResult<Option<Vec<u8>>>;

    /// 运行一个外部程序。程序身份到可执行名与参数的映射留在本层。
    fn run(&self, program: Program, args: &[String]) -> PlatformResult<ToolRun>;

    /// 仅当 `program` 在宿主上可执行时返回真。
    fn tool_available(&self, program: Program) -> bool;

    /// 程序在宿主上的可执行名，用于提示文案。
    fn tool_name(&self, program: Program) -> &'static str;

    /// 把文件的字节区间绑定为块设备视图。
    fn bind_loop(&self, path: &Path, offset: u64, len: u64) -> PlatformResult<LoopBinding>;

    /// 整盘块设备上第 N 个分区的设备节点。
    fn partition_device(&self, disk: &Path, number: u32) -> PlatformResult<PathBuf>;

    /// 宿主随机字节，用于新表的 GUID。
    fn entropy(&self, len: usize) -> PlatformResult<Vec<u8>>;

    /// 递归建立目录。
    fn create_directory_all(&self, path: &Path) -> PlatformResult<()>;

    /// 以覆盖方式写一个状态文件并落盘。
    fn write_state_file(&self, path: &Path, bytes: &[u8]) -> PlatformResult<()>;

    /// 追加写一个状态文件并落盘。
    fn append_state_file(&self, path: &Path, bytes: &[u8]) -> PlatformResult<()>;

    /// 删除一个状态文件；不存在时视为成功。
    fn remove_state_file(&self, path: &Path) -> PlatformResult<()>;

    /// 把目录项落盘。
    fn sync_directory(&self, path: &Path) -> PlatformResult<()>;

    /// 对状态文件取独占锁；离开作用域即释放。
    fn lock_state(&self, path: &Path) -> PlatformResult<Box<dyn StateLock>>;
}
