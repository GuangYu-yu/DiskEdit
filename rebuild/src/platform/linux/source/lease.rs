//! 目标独占租约。
//!
//! 租约对已打开的文件描述符加 `flock(LOCK_EX | LOCK_NB)`：取不到就说明另一个进程
//! 已独占该目标，此时返回 [`PlatformError::Unsupported`]，不等待。租约在析构时释放。
//! 所有写类动作都要求先取得租约。

use std::fs::File;
use std::path::{Path, PathBuf};

use super::super::{PlatformError, PlatformResult};

/// 一份独占租约；持有期间对应文件保持加锁。
#[derive(Debug)]
pub struct Lease {
    file: File,
    path: PathBuf,
}

impl Lease {
    /// 打开路径并尝试取得独占租约。
    #[cfg(unix)]
    pub fn acquire(path: &Path) -> PlatformResult<Self> {
        use std::os::fd::AsRawFd;
        let file = File::open(path).map_err(|error| PlatformError::io(path, error))?;
        // SAFETY: fd 来自已打开的文件；flock 只作用在该 fd 上，不触碰内存。
        let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if result == 0 {
            return Ok(Self {
                file,
                path: path.to_path_buf(),
            });
        }
        let error = std::io::Error::last_os_error();
        match error.raw_os_error() {
            Some(code) if code == libc::EWOULDBLOCK || code == libc::EAGAIN => {
                Err(PlatformError::unsupported(format!(
                    "{} is exclusively held by another process; cannot acquire a lease",
                    path.display()
                )))
            }
            Some(_) | None => Err(PlatformError::io(path, error)),
        }
    }

    /// 非 Unix 平台没有 flock，显式报告不适用。
    #[cfg(not(unix))]
    pub fn acquire(path: &Path) -> PlatformResult<Self> {
        Err(PlatformError::not_applicable(format!(
            "an exclusive lease on {} is not supported on this platform",
            path.display()
        )))
    }

    /// 取得租约时使用的路径。
    pub fn path(&self) -> &Path {
        &self.path
    }
}

#[cfg(unix)]
impl Drop for Lease {
    fn drop(&mut self) {
        use std::os::fd::AsRawFd;
        // SAFETY: fd 仍然有效；释放锁不影响任何内存，失败时进程退出也会由内核释放。
        unsafe {
            libc::flock(self.file.as_raw_fd(), libc::LOCK_UN);
        }
    }
}
