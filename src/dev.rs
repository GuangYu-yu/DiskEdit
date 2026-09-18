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

    /// 元数据写入：被覆盖字节的原文先入 undo journal 再写
    pub fn write_at(&mut self, off: u64, buf: &[u8]) -> io::Result<()> {
        if self.journal.is_some() {
            let mut orig = vec![0u8; buf.len()];
            let within = self.size.saturating_sub(off).min(buf.len() as u64) as usize;
            if within > 0 {
                self.read_at(off, &mut orig[..within])?;
            } // EOF 之外视为零，其余保持 0 填充
              // journal 先行落盘：写入发生前，被覆盖字节的原文必须已持久化
            if let Some(journal) = self.journal.as_mut() {
                journal.record(off, &orig)?;
            }
        }
        self.write_raw(off, buf)
    }

    /// 数据块写入（搬移的 chunk 拷贝）：不入 undo journal。
    /// 搬移的设计是"前向恢复、无回滚"（见 movepart 模块注释），undo 不消费数据字节，
    /// 记录它们只会产生与搬移量等大的 journal。数据一致性由 ckpt + 幂等重做保证
    pub fn write_data_at(&mut self, off: u64, buf: &[u8]) -> io::Result<()> {
        self.write_raw(off, buf)
    }

    fn write_raw(&mut self, off: u64, buf: &[u8]) -> io::Result<()> {
        use std::io::{Seek, SeekFrom, Write};
        self.file.seek(SeekFrom::Start(off))?;
        self.file.write_all(buf)
    }

    /// 标记本次操作含数据搬移（其字节不入 journal）：undo 据此拒绝回滚，
    /// 避免"表回滚了、数据没回滚"的不一致
    pub fn mark_relocation(&mut self) -> io::Result<()> {
        if let Some(journal) = self.journal.as_mut() {
            journal.record(Journal::MOVED_MARKER, &[])?;
        }
        Ok(())
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
    // SAFETY: f 有效打开的 fd；内核仅写入 &mut v（输出方向 _IOR），调用期间指针有效
    let r = unsafe { libc::ioctl(f.as_raw_fd() as libc::c_int, BLKGETSIZE64 as libc::Ioctl, &mut v as *mut u64) };
    if r < 0 { Err(io::Error::last_os_error()) } else { Ok(v) }
}

#[cfg(target_os = "linux")]
fn blksszget(f: &File) -> io::Result<u32> {
    // BLKSSZGET = _IO(0x12, 104)（内核 include/uapi/linux/fs.h），getter 返回 u32；
    // libc 按架构导出正确编码（generic 0x1268、mips/powerpc/sparc 0x20001268）
    let mut v: u32 = 0;
    // SAFETY: f 有效打开的 fd；内核仅写入 &mut v，调用期间指针有效
    let r = unsafe { libc::ioctl(f.as_raw_fd() as libc::c_int, libc::BLKSSZGET as libc::Ioctl, &mut v as *mut u32) };
    if r < 0 { Err(io::Error::last_os_error()) } else { Ok(v) }
}

/// 解析 `<target>[:N]` → (路径, 分区号 Option)。
/// 本层只判定合法性、不决定进程怎么退出：数字段溢出 u32 静默当整盘目标会误伤数据，
/// 故作为错误上抛，由调用方（main 的参数层）转成退出码
pub fn parse_target(s: &str) -> Result<(String, Option<u32>), &'static str> {
    match s.rfind(':') {
        Some(pos) if s[pos + 1..].chars().all(|c| c.is_ascii_digit()) && !s[pos + 1..].is_empty() => {
            let n: u32 = s[pos + 1..].parse().map_err(|_| "partition number out of range")?;
            Ok((s[..pos].to_string(), if n > 0 { Some(n) } else { None }))
        }
        _ => Ok((s.to_string(), None)),
    }
}

/// 补救提示里的设备标识：块设备给出可直接粘贴的分区节点（/dev/sdb→/dev/sdb1、
/// /dev/nvme0n1→/dev/nvme0n1p1，末尾数字需 p 分隔）；镜像文件没有分区节点，
/// 给出字节偏移供 `losetup -o` 使用
pub(crate) fn part_dev_hint(src: &FileSource, part: u32, offset_bytes: u64) -> String {
    if src.is_block {
        let base = src.path.to_string_lossy();
        let sep = if base.chars().last().is_some_and(|c| c.is_ascii_digit()) { "p" } else { "" };
        format!("{base}{sep}{part}")
    } else {
        format!("<part {part} of {} at offset {offset_bytes} — e.g. losetup -o {offset_bytes}>", src.path.display())
    }
}

/// 删除持久化元数据（journal / checkpoint）失败：残留不是"无害垃圾"——
/// journal 残留会让下次 undo 重复回滚已回滚的内容，checkpoint 残留会让下次运行
/// 被旧计划阻塞或误续传。属用户必须知道的状态，故告警（不改变本次的成功结论）。
/// 文件本就不存在不算失败：删除的后置条件是"不残留"，此时已然成立
pub(crate) fn warn_if_remove_failed(path: &std::path::Path) {
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => eprintln!(
            "warning: cannot remove {}: {e} — a later run may re-apply this journal or be blocked by a stale checkpoint",
            path.display()
        ),
    }
}

/// 尽力清理临时资源（临时挂载点等）：失败只影响资源占用，且调用点通常已有主错误/告警
#[allow(clippy::let_underscore_must_use)]
pub(crate) fn best_effort_rmdir(dir: &std::path::Path) {
    let _ = std::fs::remove_dir(dir);
}

/// 尽力而为的目录 fsync：把新建/重命名后的目录项推入持久存储。
/// 失败**不**影响安全性——只影响"崩溃后还能看到多新的元数据"：
/// - ckpt 目录项丢失 → 恢复点回退到上一个 checkpoint（重做已完成部分，安全，不会超前）
/// - journal 目录项丢失 → 该 journal 可能整份消失（undo 能力丢失，前向恢复不受影响）
///
/// 两者都不会产生"超前于数据"的持久化状态，故此处有意忽略失败
#[cfg(unix)]
#[allow(clippy::let_underscore_must_use)] // 有意忽略：失败只使元数据回退，见上
fn best_effort_dir_fsync(path: &std::path::Path) {
    if let Some(dir) = path.parent()
        && let Ok(d) = std::fs::File::open(dir)
    {
        let _ = d.sync_all();
    }
}

#[cfg(not(unix))]
fn best_effort_dir_fsync(_path: &std::path::Path) {}

/// journal 整份读取的结论：区分"读完了"与"尾部有一笔未完成的事务"。
/// 后者是 append-only 语义下的正常产物（最后一次追加中途断电），不是损坏——两者若共用一个
/// 表示，"崩溃后能不能回滚"就变成靠错误文案猜的事
pub enum JournalRead {
    /// 全部记录完整且 CRC 校验通过
    Complete(Vec<(u64, Vec<u8>)>),
    /// 尾部存在一条未完成的记录，已丢弃；前面已完整的记录前缀原样返回
    TruncatedTail(Vec<(u64, Vec<u8>)>),
}

/// undo journal：追加式 [len u32][off u64][crc32 u32][data] 记录流。
/// 每条记录先于实际写入持久化，故任意落点断电后 undo 都能还原已发生的写入。
/// 只覆盖本工具**元数据**写入（write_at）；分区搬移的**数据块**走 write_data_at，
/// 不入 journal 而只留一条 MOVED_MARKER —— 搬移是"前向恢复、无回滚"，
/// 记数据字节既无人消费又与搬移量等大。外部 FS 工具（mkfs/resizefs 等）的写入
/// 同样不在此列；回放时整卷日志全量载入内存。
pub struct Journal {
    file: File,
}

impl Journal {
    const MAGIC: &[u8; 5] = b"DEJL\x01";

    /// 搬移标记哨兵 offset：undo 见到它即拒绝回滚（数据未被 journal 覆盖，
    /// 仅回滚表项会留下与数据不一致的布局）
    pub const MOVED_MARKER: u64 = u64::MAX;

    pub fn create(path: &Path) -> io::Result<Self> {
        let mut f = OpenOptions::new().create(true).append(true).read(true).open(path)?;
        if f.metadata()?.len() == 0 {
            use std::io::Write;
            f.write_all(Self::MAGIC)?;
            f.sync_all()?;
            // 新建文件的目录项也必须落盘：否则断电后 journal 整体消失，而写入已经发生
            best_effort_dir_fsync(path);
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

    /// 逐条校验并读取。**契约不变**：已形成的记录必须 CRC 正确，任一条损坏即整体拒绝，
    /// 不做"跳过坏记录继续回放"式的部分回放。
    ///
    /// 额外单列的是"**未完成的尾部记录**"——它与"损坏"不是一回事：`record` 先写头再写数据、
    /// 最后才 sync，而调用方在 record 返回之后才真正写盘，因此尾部撕裂只可能来自一次没走完的
    /// 追加，那条记录对应的数据写入**根本没有发生**，丢弃它是安全的。中途（非尾部）CRC 不符
    /// 才是真实损坏，仍整体拒绝
    ///
    /// 注意：记录头里的 len 不参与任何 CRC，故"len 被写坏成大值"与"数据只写了一半"在文件里
    /// 无法区分，两者都落在 TruncatedTail。这不影响安全性——返回的前缀每条都通过了 CRC，
    /// 且各自对应的写入确实发生过；代价只是该点之后的记录无法回放，措辞里已如实点明
    pub fn read_entries(path: &Path) -> io::Result<JournalRead> {
        let data = std::fs::read(path)?;
        if data.len() < Self::MAGIC.len() || &data[..Self::MAGIC.len()] != Self::MAGIC {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "bad journal header"));
        }
        let mut out = Vec::new();
        let mut pos = Self::MAGIC.len();
        while pos < data.len() {
            // 记录头尚未写全 → 尾部未完成的事务
            if pos + 16 > data.len() {
                return Ok(JournalRead::TruncatedTail(out));
            }
            let len = u32::from_le_bytes(data[pos..pos + 4].try_into().unwrap()) as usize;
            let off = u64::from_le_bytes(data[pos + 4..pos + 12].try_into().unwrap());
            let crc = u32::from_le_bytes(data[pos + 12..pos + 16].try_into().unwrap());
            pos += 16;
            // 头写全了但数据没写全 → 尾部未完成的事务
            if pos + len > data.len() {
                return Ok(JournalRead::TruncatedTail(out));
            }
            let bytes = data[pos..pos + len].to_vec();
            if crate::table::crc32(&bytes) != crc {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "journal entry CRC mismatch"));
            }
            pos += len;
            out.push((off, bytes));
        }
        Ok(JournalRead::Complete(out))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::let_underscore_must_use)] // 清理临时文件有意忽略失败
    use super::*;

    fn journal_path(tag: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("diskedit_jr_{tag}_{}.log", std::process::id()));
        let _ = std::fs::remove_file(&p);
        p
    }

    /// 尾部撕裂是"未完成的事务"，不是损坏：丢弃那一条、回放完整前缀；
    /// 中途 CRC 不符才是损坏，仍整体拒绝（"不做部分回放"的契约只对后者生效）
    #[test]
    fn journal_tail_truncation_is_recovered_but_corruption_is_not() {
        let p = journal_path("tail");
        const DATA: usize = 512;
        const REC: usize = 16 + DATA; // [len u32][off u64][crc u32][data]
        {
            let mut j = Journal::create(&p).unwrap();
            for i in 0..3u64 {
                j.record(1024 * i, &[0xAA; DATA]).unwrap();
            }
        }
        assert!(matches!(
            Journal::read_entries(&p).unwrap(),
            JournalRead::Complete(v) if v.len() == 3
        ));

        // 把最后一条记录截掉任意一段非空的长度 → 都应是"2 条完整 + 尾部未完成"。
        // 上界取 REC（不含）：截满 REC 等于整条消失，那就不再是撕裂而是"到此为止"
        let full = std::fs::read(&p).unwrap();
        for cut in 1..REC {
            std::fs::write(&p, &full[..full.len() - cut]).unwrap();
            match Journal::read_entries(&p).unwrap() {
                JournalRead::TruncatedTail(v) => assert_eq!(v.len(), 2, "cut {cut}"),
                JournalRead::Complete(v) => {
                    panic!("cut {cut}: expected a truncated tail, got {} complete record(s)", v.len())
                }
            }
        }
        // 截满一条 = 该记录整条没写进去，此时应报 Complete(2)，不是撕裂
        std::fs::write(&p, &full[..full.len() - REC]).unwrap();
        assert!(matches!(
            Journal::read_entries(&p).unwrap(),
            JournalRead::Complete(v) if v.len() == 2
        ));

        // 中途（非尾部）CRC 损坏 → 整体拒绝
        let mut corrupt = full.clone();
        corrupt[5 + 16 + 100] ^= 0xFF; // 第 1 条记录的数据中段
        std::fs::write(&p, &corrupt).unwrap();
        assert!(Journal::read_entries(&p).is_err(), "mid-file corruption must be refused");

        let _ = std::fs::remove_file(&p);
    }
}