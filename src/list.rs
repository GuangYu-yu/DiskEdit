//! 文件级浏览（ls/cat）：fstool inspect 文件后端流式读取。
//!
//! - `inspect::Target::parse("img:N")` 原生分区切片（1 起，同 sgdisk/loopXpN）
//! - `with_target_device_read_only` 只读打开文件/块设备，`BlockDevice` 按需 seek+read，
//!   不整分区入内存
//! - `read_file` 返回 `Box<dyn fstool::io::Read>`，cat 逐块流出
//! - 扇区大小由 fstool 自行探测，本工具的 --sector-size 覆盖对 ls/cat 不生效
//! - musl：上游 block/file.rs ioctl 类型 bug 编译不过，browse feature 整体门控（Cargo.toml）

#[cfg(feature = "browse")]
mod browse {
    use fstool::fs::EntryKind;
    use fstool::fs::Filesystem as FsApi;
    use fstool::inspect::{self, Target};
    use std::io;
    use std::path::Path;

    fn io_err<E: std::fmt::Display>(e: E) -> io::Error {
        io::Error::new(io::ErrorKind::InvalidData, format!("fstool: {e}"))
    }

    fn kind_str(k: EntryKind) -> &'static str {
        match k {
            EntryKind::Regular => "f",
            EntryKind::Dir => "d",
            EntryKind::Symlink => "l",
            EntryKind::Char => "c",
            EntryKind::Block => "b",
            EntryKind::Fifo => "p",
            EntryKind::Socket => "s",
            EntryKind::Unknown => "?",
        }
    }

    /// 打开目标的文件系统（流式后端），闭包内拿到 FS 句柄与块设备。
    /// 闭包以 io::Result 返回，这里统一转成 fstool 的 Ok/Err 通道。
    fn with_fs<T>(
        path: &Path,
        part: Option<u32>,
        f: impl FnOnce(&mut dyn FsApi, &mut dyn fstool::block::BlockDevice) -> io::Result<T>,
    ) -> io::Result<T> {
        let spec = match part {
            Some(n) => format!("{}:{n}", path.display()),
            None => path.display().to_string(),
        };
        let target = Target::parse(&spec);
        let mut out: Option<io::Result<T>> = None;
        inspect::with_target_device_read_only(&target, |dev| {
            out = Some(match inspect::open(dev) {
                Ok(mut fs) => f(fs.as_mut(), dev),
                Err(e) => Err(io_err(e)),
            });
            Ok(())
        })
        .map_err(io_err)?;
        out.ok_or_else(|| io::Error::other("fstool inspect closure did not produce output"))?
    }

    pub fn ls(path: &Path, part: Option<u32>, dir: &str) -> io::Result<Vec<(String, String, u64)>> {
        let dirp = Path::new(dir);
        with_fs(path, part, |fs, dev| {
            let entries = FsApi::list(fs, dev, dirp).map_err(io_err)?;
            Ok(entries.into_iter().map(|e| (e.name, kind_str(e.kind).to_string(), e.size)).collect())
        })
    }

    /// 流式 cat：逐块读出写到 out，返回总字节数
    pub fn cat_to(path: &Path, part: Option<u32>, file: &str, out: &mut dyn io::Write) -> io::Result<u64> {
        let fp = Path::new(file);
        with_fs(path, part, |fs, dev| {
            let mut r = FsApi::read_file(fs, dev, fp).map_err(io_err)?;
            let mut total = 0u64;
            let mut buf = vec![0u8; 256 * 1024];
            loop {
                let n = fstool::io::Read::read(&mut r, &mut buf).map_err(io_err)?;
                if n == 0 {
                    return Ok(total);
                }
                out.write_all(&buf[..n])?;
                total += n as u64;
            }
        })
    }
}

/// 无 browse feature（musl 静态构建等）：ls/cat 编译为显式拒绝，不影响其余功能
#[cfg(not(feature = "browse"))]
mod browse {
    use std::io;
    use std::path::Path;

    const DISABLED: &str = "built without the `browse` feature (no FS support; rebuild with --features browse)";
    pub fn ls(_path: &Path, _part: Option<u32>, _dir: &str) -> io::Result<Vec<(String, String, u64)>> {
        Err(io::Error::new(io::ErrorKind::Unsupported, DISABLED))
    }
    pub fn cat_to(_path: &Path, _part: Option<u32>, _file: &str, _out: &mut dyn io::Write) -> io::Result<u64> {
        Err(io::Error::new(io::ErrorKind::Unsupported, DISABLED))
    }
}

pub use browse::{cat_to, ls};