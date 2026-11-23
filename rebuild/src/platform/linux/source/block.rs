//! 块设备的按偏移读写与逻辑扇区读取。
//!
//! 长度取自 `BLKGETSIZE64`（字节），逻辑扇区取自 `BLKSSZGET`（字节）。两者都是
//! 只读查询，被信号打断时重发。读写与普通文件相同，走按偏移的系统调用。

use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};

use super::{PlatformError, PlatformResult, ReadAt, WriteAt};

/// 打开的块设备、字节长度与逻辑扇区。
#[derive(Debug)]
pub struct BlockSource {
    file: File,
    path: PathBuf,
    len: u64,
    sector_size: u64,
}

impl BlockSource {
    /// 以读写方式打开块设备，并读取长度与逻辑扇区。
    #[cfg(target_os = "linux")]
    pub fn open(path: &Path) -> PlatformResult<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .map_err(|error| PlatformError::io(path, error))?;
        let len = ioctl_get_u64(&file, path, BLKGETSIZE64)?;
        let sector_size = u64::from(ioctl_get_u32(
            &file,
            path,
            libc::BLKSSZGET as libc::c_ulong,
        )?);
        if sector_size == 0 {
            return Err(PlatformError::parse(format!(
                "{} 报告的逻辑扇区为 0",
                path.display()
            )));
        }
        Ok(Self {
            file,
            path: path.to_path_buf(),
            len,
            sector_size,
        })
    }

    /// 非 Linux 平台没有统一的块设备 ioctl，显式报告不适用。
    #[cfg(not(target_os = "linux"))]
    pub fn open(path: &Path) -> PlatformResult<Self> {
        Err(PlatformError::not_applicable(format!(
            "当前平台不支持按块设备读取 {}",
            path.display()
        )))
    }

    /// 打开时读到的字节长度。
    pub fn len_bytes(&self) -> u64 {
        self.len
    }

    /// 逻辑扇区大小，字节。
    pub fn sector_size(&self) -> u64 {
        self.sector_size
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

// 设备容量（`BLKGETSIZE64`）：`_IOR(0x12, 114, size_t)`，内容为 u64。libc 未导出该
// 常量，按 asm-generic 的 _IOC 位域自持；size 字段取 size_t 宽度，故按指针宽度分写。
#[cfg(target_os = "linux")]
#[cfg(target_pointer_width = "64")]
const BLKGETSIZE64: libc::c_ulong = 0x8008_1272;
#[cfg(target_os = "linux")]
#[cfg(target_pointer_width = "32")]
const BLKGETSIZE64: libc::c_ulong = 0x8004_1272;

#[cfg(target_os = "linux")]
fn ioctl_get_u64(file: &File, path: &Path, request: libc::c_ulong) -> PlatformResult<u64> {
    let mut value: u64 = 0;
    // SAFETY: fd 来自已打开的文件；内核只写入 &mut value，调用期间指针有效。
    ioctl_retry(|| unsafe {
        libc::ioctl(
            std::os::fd::AsRawFd::as_raw_fd(file),
            request,
            &mut value as *mut u64,
        )
    })
    .map_err(|error| PlatformError::io(path, error))?;
    Ok(value)
}

#[cfg(target_os = "linux")]
fn ioctl_get_u32(file: &File, path: &Path, request: libc::c_ulong) -> PlatformResult<u32> {
    let mut value: u32 = 0;
    // SAFETY: fd 来自已打开的文件；内核只写入 &mut value，调用期间指针有效。
    ioctl_retry(|| unsafe {
        libc::ioctl(
            std::os::fd::AsRawFd::as_raw_fd(file),
            request,
            &mut value as *mut u32,
        )
    })
    .map_err(|error| PlatformError::io(path, error))?;
    Ok(value)
}

/// 调用只读类 ioctl，被信号打断时重发。
#[cfg(target_os = "linux")]
fn ioctl_retry(mut call: impl FnMut() -> libc::c_int) -> std::io::Result<()> {
    loop {
        if call() == 0 {
            return Ok(());
        }
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::EINTR) {
            return Err(error);
        }
    }
}

#[cfg(unix)]
mod unix {
    use super::*;
    use std::os::unix::fs::FileExt;

    impl ReadAt for BlockSource {
        fn len(&self) -> PlatformResult<u64> {
            Ok(self.len_bytes())
        }

        fn read_at(&self, offset: u64, buf: &mut [u8]) -> PlatformResult<usize> {
            if offset >= self.len_bytes() && !buf.is_empty() {
                return Err(PlatformError::io(
                    self.path(),
                    std::io::Error::new(
                        std::io::ErrorKind::UnexpectedEof,
                        format!("读取起点 {offset} 已到设备末尾"),
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

    impl WriteAt for BlockSource {
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
            // 块设备的 sync_all 会触发设备缓存提交；失败如实上报。
            self.file()
                .sync_all()
                .map_err(|error| PlatformError::io(self.path(), error))
        }
    }
}

#[cfg(not(unix))]
mod fallback {
    use super::*;

    impl ReadAt for BlockSource {
        fn len(&self) -> PlatformResult<u64> {
            Ok(self.len_bytes())
        }

        fn read_at(&self, _offset: u64, _buf: &mut [u8]) -> PlatformResult<usize> {
            Err(PlatformError::not_applicable(
                "当前平台没有按偏移读取的接口",
            ))
        }
    }

    impl WriteAt for BlockSource {
        fn write_at(&mut self, _offset: u64, _buf: &[u8]) -> PlatformResult<usize> {
            Err(PlatformError::not_applicable(
                "当前平台没有按偏移写入的接口",
            ))
        }

        fn barrier(&mut self) -> PlatformResult<()> {
            Err(PlatformError::not_applicable("当前平台没有块设备提交接口"))
        }
    }
}
