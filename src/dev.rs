//! 输入源：镜像文件与块设备统一为按偏移读写的字节存储。

use std::fs::{File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};

#[cfg(target_os = "linux")]
use std::os::fd::AsRawFd;
#[cfg(target_os = "linux")]
use std::os::unix::fs::{FileTypeExt, OpenOptionsExt};

pub struct FileSource {
    pub(crate) file: File,
    pub path: PathBuf,
    pub sector_size: u64,
    pub size: u64,
    pub is_block: bool,
    /// undo journal：只记录本工具 write_at 的直接写入
    pub journal: Option<Journal>,
}

impl FileSource {
    /// `sector_size_override`：镜像默认 512（镜像不携带扇区信息），块设备经 BLKSSZGET 查询并忽略覆盖值。
    pub fn open(path: &Path, sector_size_override: Option<u64>) -> io::Result<Self> {
        let meta = std::fs::metadata(path)?;
        // 块设备判定：块设备文件（其 metadata().len() 恒 0，容量需 ioctl 取）
        #[cfg(target_os = "linux")]
        if meta.file_type().is_block_device() {
            // 读写 + O_EXCL（man open(2)）：设备被 claim 时内核拒绝打开，
            // 分区被挂载或占用会连同整盘一起被 claim
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .custom_flags(libc::O_EXCL)
                .open(path)?;
            let size = blkgetsize64(&file)?;
            let sector_size = blksszget(&file)? as u64;
            return Ok(FileSource { file, path: path.to_path_buf(), sector_size, size, is_block: true, journal: None });
        }
        let file = OpenOptions::new().read(true).write(true).open(path)?;
        let size = meta.len();
        let sector_size = sector_size_override.unwrap_or(512);
        // 镜像扇区大小须为 2 的幂且落在 512..=65536；这是本工具的自定约束，非规范要求
        if !(512..=65536).contains(&sector_size) || !sector_size.is_power_of_two() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("invalid sector size {sector_size}"),
            ));
        }
        Ok(FileSource { file, path: path.to_path_buf(), sector_size, size, is_block: false, journal: None })
    }

    /// 只读打开块设备（在线路径识别 FS 用：读写 + O_EXCL 在设备被 claim 时会失败）
    #[cfg(target_os = "linux")]
    pub(crate) fn open_read_only(path: &Path) -> io::Result<Self> {
        let file = OpenOptions::new().read(true).open(path)?;
        let size = blkgetsize64(&file)?;
        let sector_size = blksszget(&file)? as u64;
        Ok(FileSource { file, path: path.to_path_buf(), sector_size, size, is_block: true, journal: None })
    }

    /// pread 语义（不移动文件游标，&self 可调用）；不足 buf 长度报错
    pub fn read_at(&self, off: u64, buf: &mut [u8]) -> io::Result<()> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::FileExt;
            self.file.read_exact_at(buf, off)
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::FileExt;
            let mut done = 0usize;
            while done < buf.len() {
                let n = self.file.seek_read(&mut buf[done..], off + done as u64)?;
                if n == 0 {
                    return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "short read"));
                }
                done += n;
            }
            Ok(())
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = (off, buf);
            Err(io::Error::new(io::ErrorKind::Unsupported, "no read_at on this platform"))
        }
    }

    pub fn write_at(&mut self, off: u64, buf: &[u8]) -> io::Result<()> {
        // journal 先行落盘：写入发生前，被覆盖字节的原文必须已持久化
        let mut orig = vec![0u8; buf.len()];
        let within = self.size.saturating_sub(off).min(buf.len() as u64) as usize;
        if within > 0 {
            self.read_at(off, &mut orig[..within])?;
        } // EOF 之外视为零，其余保持 0 填充
        if let Some(journal) = self.journal.as_mut() {
            journal.record(off, &orig)?;
        }
        use std::io::{Seek, SeekFrom, Write};
        self.file.seek(SeekFrom::Start(off))?;
        self.file.write_all(buf)
    }

    /// 每步落盘。容量有变化的场景用 sync_all，纯数据覆盖用 sync_data。
    pub fn sync_all(&self) -> io::Result<()> {
        self.file.sync_all()
    }

    pub fn sync_data(&self) -> io::Result<()> {
        self.file.sync_data()
    }
}

#[cfg(target_os = "linux")]
fn blkgetsize64(f: &File) -> io::Result<u64> {
    // BLKGETSIZE64 = _IOR(0x12, 114, u64)（内核 include/uapi/linux/fs.h，内容 u64）。
    // libc 未导出该常量（0.2.139/0.2.186/0.2.189 实测），取 UAPI 定义自持；
    // 值为 asm-generic 编码（x86_64/aarch64/arm/riscv 共用），MIPS/PowerPC/sparc
    // 的 _IOC 位域偏移不同、编码不同，不受支持
    const BLKGETSIZE64: u64 = 0x8008_1272;
    let mut v: u64 = 0;
    let r = unsafe { libc::ioctl(f.as_raw_fd() as libc::c_int, BLKGETSIZE64 as libc::Ioctl, &mut v as *mut u64) };
    // SAFETY: f 有效打开的 fd；内核仅写入 &mut v（输出方向 _IOR），调用期间指针有效
    if r < 0 { Err(io::Error::last_os_error()) } else { Ok(v) }
}

#[cfg(target_os = "linux")]
fn blksszget(f: &File) -> io::Result<u32> {
    // BLKSSZGET = _IO(0x12, 104)（内核 include/uapi/linux/fs.h），getter 返回 u32；
    // libc 按架构导出正确编码（generic 0x1268、mips/powerpc/sparc 0x20001268）
    let mut v: u32 = 0;
    let r = unsafe { libc::ioctl(f.as_raw_fd() as libc::c_int, libc::BLKSSZGET as libc::Ioctl, &mut v as *mut u32) };
    // SAFETY: f 有效打开的 fd；内核仅写入 &mut v，调用期间指针有效
    if r < 0 { Err(io::Error::last_os_error()) } else { Ok(v) }
}

/// 解析 `<target>[:N]` → (路径, 分区号 Option)
pub fn parse_target(s: &str) -> (String, Option<u32>) {
    match s.rfind(':') {
        Some(pos) if s[pos + 1..].chars().all(|c| c.is_ascii_digit()) && !s[pos + 1..].is_empty() => {
            // 数字段溢出 u32 属非法输入：静默当整盘目标会误伤数据
            let n: u32 = s[pos + 1..].parse().unwrap_or_else(|_| {
                eprintln!("refused: partition number out of range");
                std::process::exit(crate::EXIT_REFUSED as i32);
            });
            (s[..pos].to_string(), if n > 0 { Some(n) } else { None })
        }
        _ => (s.to_string(), None),
    }
}

/// undo journal：追加式 [len u32][off u64][crc32 u32][data] 记录流。
/// 每条记录先于实际写入持久化，故任意落点断电后 undo 都能还原已发生的写入。
/// 仅覆盖本工具 write_at 的写入，外部 FS 工具（mkfs/resizefs 等）的写入不在此列；
/// 回放时整卷日志全量载入内存。
pub struct Journal {
    file: File,
}

impl Journal {
    const MAGIC: &[u8; 5] = b"DEJL\x01";

    pub fn create(path: &Path) -> io::Result<Self> {
        let mut f = OpenOptions::new().create(true).append(true).read(true).open(path)?;
        if f.metadata()?.len() == 0 {
            use std::io::Write;
            f.write_all(Self::MAGIC)?;
            f.sync_all()?;
            // 新建文件的目录项也必须落盘：否则断电后 journal 整体消失，而写入已经发生
            #[cfg(unix)]
            if let Some(dir) = path.parent()
                && let Ok(d) = std::fs::File::open(dir)
            {
                let _ = d.sync_all();
            }
        } else {
            // 已有文件必须是我们自己写的 journal，拒绝把陌生文件当 journal 追加
            use std::io::Read;
            let mut hdr = [0u8; 5];
            f.read_exact(&mut hdr).map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidData, "existing journal too short for magic")
            })?;
            if &hdr != Self::MAGIC {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "existing file is not a diskedit journal (magic mismatch)"));
            }
        }
        Ok(Journal { file: f })
    }

    pub fn record(&mut self, off: u64, bytes: &[u8]) -> io::Result<()> {
        use std::io::Write;
        self.file.write_all(&(bytes.len() as u32).to_le_bytes())?;
        self.file.write_all(&off.to_le_bytes())?;
        self.file.write_all(&crate::table::crc32(bytes).to_le_bytes())?;
        self.file.write_all(bytes)?;
        self.file.sync_data()
    }

    /// 全量读取并逐条校验 CRC（损坏即报错，不做部分回放）
    pub fn read_entries(path: &Path) -> io::Result<Vec<(u64, Vec<u8>)>> {
        let data = std::fs::read(path)?;
        if data.len() < Self::MAGIC.len() || &data[..Self::MAGIC.len()] != Self::MAGIC {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "bad journal header"));
        }
        let mut out = Vec::new();
        let mut pos = Self::MAGIC.len();
        while pos < data.len() {
            if pos + 16 > data.len() {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "truncated journal entry"));
            }
            let len = u32::from_le_bytes(data[pos..pos + 4].try_into().unwrap()) as usize;
            let off = u64::from_le_bytes(data[pos + 4..pos + 12].try_into().unwrap());
            let crc = u32::from_le_bytes(data[pos + 12..pos + 16].try_into().unwrap());
            pos += 16;
            if pos + len > data.len() {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "truncated journal entry"));
            }
            let bytes = data[pos..pos + len].to_vec();
            if crate::table::crc32(&bytes) != crc {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "journal entry CRC mismatch"));
            }
            pos += len;
            out.push((off, bytes));
        }
        Ok(out)
    }
}