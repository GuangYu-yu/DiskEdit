//! 普通文件的按偏移读写。
//!
//! 文件长度在打开时取自元数据；[`ReadAt::read_at`] 与 [`WriteAt::write_at`] 都只
//! 作用于给定区间，越界读取返回错误而不补零。[`WriteAt::barrier`] 调用 `sync_all`。

use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};

use super::{PlatformError, PlatformResult, ReadAt, WriteAt};

/// 打开的普通文件及其长度。
#[derive(Debug)]
pub struct FileSource {
    file: File,
    path: PathBuf,
    len: u64,
}

impl FileSource {
    /// 以读写方式打开。
    pub fn open(path: &Path) -> PlatformResult<Self> {
        Self::open_with(path, true)
    }

    /// 以只读方式打开。
    pub fn open_read_only(path: &Path) -> PlatformResult<Self> {
        Self::open_with(path, false)
    }

    fn open_with(path: &Path, writable: bool) -> PlatformResult<Self> {
        let mut options = OpenOptions::new();
        options.read(true).write(writable);
        let file = options
            .open(path)
            .map_err(|error| PlatformError::io(path, error))?;
        let len = file
            .metadata()
            .map_err(|error| PlatformError::io(path, error))?
            .len();
        Ok(Self {
            file,
            path: path.to_path_buf(),
            len,
        })
    }

    /// 打开时读到的字节长度。
    pub fn len_bytes(&self) -> u64 {
        self.len
    }

    /// 打开时使用的路径。
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 底层的文件句柄，供本层内需要 fd 的调用点使用。
    pub(crate) fn file(&self) -> &File {
        &self.file
    }
}

#[cfg(unix)]
mod unix {
    use super::*;
    use std::os::unix::fs::FileExt;

    impl ReadAt for FileSource {
        fn len(&self) -> PlatformResult<u64> {
            Ok(self.len_bytes())
        }

        fn read_at(&self, offset: u64, buf: &mut [u8]) -> PlatformResult<usize> {
            if offset >= self.len_bytes() && !buf.is_empty() {
                return Err(PlatformError::io(
                    self.path(),
                    std::io::Error::new(
                        std::io::ErrorKind::UnexpectedEof,
                        format!("读取起点 {offset} 已到文件末尾"),
                    ),
                ));
            }
            let available = self.len_bytes().saturating_sub(offset);
            let want = buf.len().min(available as usize);
            self.file()
                .read_at(&mut buf[..want], offset)
                .map_err(|error| PlatformError::io(self.path(), error))
        }
    }

    impl WriteAt for FileSource {
        fn write_at(&mut self, offset: u64, buf: &[u8]) -> PlatformResult<usize> {
            let mut written = 0usize;
            while written < buf.len() {
                let at = offset + written as u64;
                let n = self
                    .file()
                    .write_at(&buf[written..], at)
                    .map_err(|error| PlatformError::io(self.path(), error))?;
                if n == 0 {
                    return Err(PlatformError::io(
                        self.path(),
                        std::io::Error::new(std::io::ErrorKind::WriteZero, "写入返回 0 字节"),
                    ));
                }
                written += n;
            }
            Ok(written)
        }

        fn barrier(&mut self) -> PlatformResult<()> {
            self.file()
                .sync_all()
                .map_err(|error| PlatformError::io(self.path(), error))
        }
    }
}

#[cfg(not(unix))]
mod fallback {
    use super::*;

    impl ReadAt for FileSource {
        fn len(&self) -> PlatformResult<u64> {
            Ok(self.len_bytes())
        }

        fn read_at(&self, _offset: u64, _buf: &mut [u8]) -> PlatformResult<usize> {
            Err(PlatformError::not_applicable(
                "当前平台没有按偏移读取的接口",
            ))
        }
    }

    impl WriteAt for FileSource {
        fn write_at(&mut self, _offset: u64, _buf: &[u8]) -> PlatformResult<usize> {
            Err(PlatformError::not_applicable(
                "当前平台没有按偏移写入的接口",
            ))
        }

        fn barrier(&mut self) -> PlatformResult<()> {
            self.file()
                .sync_all()
                .map_err(|error| PlatformError::io(self.path(), error))
        }
    }
}
