//! 宿主端口：上层只通过本模块的中立接口触碰操作系统。
//!
//! 平台选择与具体系统调用只出现在本层；`app` 与纯核不引用任何平台名。
//! S0 固定端口与句柄的形状，Linux 实现按纵切逐个接入。

use std::fmt;
use std::path::Path;

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

/// 目标的打开方式由具体平台决定，上层只拿到这个按偏移读写的句柄。
pub trait TargetHandle {
    /// 打开时使用的路径。
    fn path(&self) -> &Path;

    /// 打开时读到的字节长度。
    fn len(&self) -> u64;

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
}

/// 宿主端口。
pub trait Platform: Send + Sync {
    /// 以读写方式打开目标。
    fn open_target(&self, path: &Path) -> PlatformResult<Box<dyn TargetHandle>>;

    /// 以只读方式打开目标。
    fn open_target_read_only(&self, path: &Path) -> PlatformResult<Box<dyn TargetHandle>>;
}
