//! 按偏移读写的端口与目标独占。
//!
//! [`ReadAt`] 与 [`WriteAt`] 是平台层向上暴露的唯一读写形状：偏移以字节计，
//! 一次调用只作用在给定区间内。普通文件与块设备各自实现这两个 trait，调用方
//! 不需要知道自己拿到的是哪一种。
//!
//! 具体实现（fd 的打开方式、ioctl 编号、flock 参数）只在本模块内可见。

use std::path::Path;

use super::{PlatformError, PlatformResult};

mod block;
mod file;
pub mod guarded;
pub mod lease;

pub use block::BlockSource;
pub use file::FileSource;
pub use lease::Lease;

/// 按偏移读取。
pub trait ReadAt {
    /// 目标的总字节数。
    fn len(&self) -> PlatformResult<u64>;

    /// 从 `offset` 起读取至多 `buf.len()` 字节，返回实际读到的字节数。
    ///
    /// 区间越过末尾时返回错误，不返回凭空补零的短读。
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> PlatformResult<usize>;
}

/// 按偏移写入，并在需要时提交到设备。
pub trait WriteAt: ReadAt {
    /// 从 `offset` 起写入 `buf`，返回实际写出的字节数；写出不足时继续写直到完成。
    fn write_at(&mut self, offset: u64, buf: &[u8]) -> PlatformResult<usize>;

    /// 把已写出的字节提交到持久介质。
    fn barrier(&mut self) -> PlatformResult<()>;
}

/// 打开目标路径供读写。
///
/// 只接受普通文件与（Linux 上的）块设备；其余形态返回能力不支持。
pub fn open(path: &Path) -> PlatformResult<TargetHandle> {
    let metadata = std::fs::metadata(path).map_err(|error| PlatformError::io(path, error))?;
    let file_type = metadata.file_type();
    if file_type.is_file() {
        return Ok(TargetHandle::File(FileSource::open(path)?));
    }
    #[cfg(unix)]
    if std::os::unix::fs::FileTypeExt::is_block_device(&file_type) {
        return Ok(TargetHandle::Block(BlockSource::open(path)?));
    }
    Err(PlatformError::unsupported(format!(
        "{} 既不是普通文件也不是块设备",
        path.display()
    )))
}

/// 打开后的读写句柄。
pub enum TargetHandle {
    File(FileSource),
    Block(BlockSource),
}

impl TargetHandle {
    pub fn len(&self) -> u64 {
        match self {
            Self::File(source) => source.len_bytes(),
            Self::Block(source) => source.len_bytes(),
        }
    }

    pub fn read_at(&self, offset: u64, buf: &mut [u8]) -> PlatformResult<usize> {
        match self {
            Self::File(source) => ReadAt::read_at(source, offset, buf),
            Self::Block(source) => ReadAt::read_at(source, offset, buf),
        }
    }
}
