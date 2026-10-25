//! FileSource：镜像与块设备统一的按偏移读写句柄。

use super::*;

pub struct FileSource {
    pub(crate) file: File,
    pub path: PathBuf,
    pub sector_size: u64,
    pub size: u64,
    pub is_block: bool,
    /// 目标身份：打开时解析一次，journal / checkpoint / undo 只消费它
    pub identity: TargetIdentity,
    /// undo journal：只记录本工具 write_at 的直接写入
    pub journal: Option<Journal>,
    /// 目标的独占所有权（见 `targetlock`）：写命令持有它，只读打开为 None。
    /// 与这次打开同生命周期，因此不必由调用方各自绑定，也就不会在写盘前被提前丢掉
    pub(crate) ownership: Option<crate::targetlock::TargetLock>,
    /// 目标文件的稳定指纹：恢复现场（journal / checkpoint）用它确认"路径下的文件还是当时
    /// 那个"。镜像与 loop 的 backing 文件取文件元数据；真块设备的节点 inode 不承诺稳定
    /// （udev 重建节点即变）、节点元数据长度也恒 0，故那里三个字段全零。**比对逐字段进行**：
    /// 零值表示该字段没有证据，不参与比对（见 [`scene_mismatch`]）
    pub fingerprint: TargetFingerprint,
    /// loop 映射事实（offset, sizelimit）：目标经由 loop 设备进入时在打开时读一次
    /// sysfs。checkpoint 恢复时与记录值比对，重 attach 参数变化报"映射已变化"
    /// 而非几何不符的泛化拒绝
    pub loop_mapping: Option<(u64, u64)>,
}

// 测试期的定点读失败注入（模拟坏扇区），按线程生效。
//
// 回退逻辑的关键情形是"某个位置读不出来、别处正常"——常规文件构造不出这种形态
// （让 file 短于 size 会连盘尾一并读失败），故留一个最小的注入点。
// 测试各自跑在自己的线程上，用 RAII 守卫设置与复位
#[cfg(test)]
thread_local! {
    static READ_FAULT: std::cell::Cell<Option<u64>> = const { std::cell::Cell::new(None) };
}

/// 注入守卫：命中该偏移的 `read_at` 报错，守卫析构即复位
#[cfg(test)]
pub(crate) struct ReadFaultGuard;

#[cfg(test)]
impl ReadFaultGuard {
    pub(crate) fn at(off: u64) -> Self {
        READ_FAULT.with(|f| f.set(Some(off)));
        Self
    }
}

#[cfg(test)]
impl Drop for ReadFaultGuard {
    fn drop(&mut self) {
        READ_FAULT.with(|f| f.set(None));
    }
}

impl FileSource {
    /// 块设备打开的公共附加物：指纹 + loop 映射。
    /// loop 的身份是 backing 文件：指纹取它；真块设备节点 inode 不承诺稳定，不取。
    /// `node` 必须与身份解析用的是同一个 sysfs 节点（打开路径按 fd 的 rdev 定位），
    /// 否则路径在窗口内被重建时映射事实会来自另一台设备
    #[cfg(target_os = "linux")]
    pub(crate) fn block_open_extras(identity: &TargetIdentity, node: &Path) -> io::Result<(TargetFingerprint, Option<(u64, u64)>)> {
        let fp = if identity.kind == TargetKind::Image {
            // 指纹按**打开后的数据路径**取（loop 的 backing 名字由已打开句柄解析），
            // 不按 sysfs 字符串：那个名字是读取者的视角，不是对象本身
            TargetFingerprint::from_meta(&std::fs::metadata(identity.data_path())?)
        } else {
            TargetFingerprint::default()
        };
        Ok((fp, loop_mapping_at(node)?))
    }

    /// `sector_size_override`：镜像默认 512（镜像不携带扇区信息），块设备经 BLKSSZGET 查询并忽略覆盖值。
    pub fn open(path: &Path, sector_size_override: Option<u64>) -> io::Result<Self> {
        // 块设备判定：块设备文件（其 metadata().len() 恒 0，容量需 ioctl 取）
        #[cfg(target_os = "linux")]
        {
            let meta = std::fs::metadata(path)?;
            if meta.file_type().is_block_device() {
                // 读写 + O_EXCL（man open(2)）：设备被 claim 时内核拒绝打开，
                // 分区被挂载或占用会连同整盘一起被 claim
                let file = OpenOptions::new()
                    .read(true)
                    .write(true)
                    .custom_flags(libc::O_EXCL)
                    .open(path)?;
                let size = ioctl::blkgetsize64(&file)?;
                let sector_size = ioctl::blksszget(&file)? as u64;
                // fd 已拿到：身份与映射都按它的 rdev 反查 sysfs，不再按路径二次 stat——
                // /dev 节点在窗口内被 udev 重建时，fd 与身份描述会指向不同设备
                let rdev = {
                    use std::os::unix::fs::MetadataExt;
                    file.metadata()?.rdev()
                };
                let node = sysfs_node_from_rdev(rdev)?;
                // 块设备身份解析失败（fail-closed）⇒ 打开失败：错误信息带设备名
                let identity = TargetIdentity::resolve_block_at(&node, path).map_err(io::Error::other)?;
                let (fingerprint, loop_mapping) = Self::block_open_extras(&identity, &node)?;
                return Ok(FileSource {
                    identity,
                    file,
                    path: path.to_path_buf(),
                    sector_size,
                    size,
                    is_block: true,
                    journal: None,
                    ownership: None,
                    fingerprint,
                    loop_mapping,
                });
            }
        }
        Self::open_image(path, sector_size_override, true)
    }

    /// 镜像的只读打开：info/plan/resize 的只读阶段经此打开——调用面已核清
    /// 无一处经该 FileSource 写盘，真只读让只读文件、只读介质或被他进程独占的镜像
    /// 同样可用，且"只读命令不申请写权限"与块设备侧同一口径
    pub(crate) fn open_read_only_image(path: &Path, sector_size_override: Option<u64>) -> io::Result<Self> {
        Self::open_image(path, sector_size_override, false)
    }

    /// 已打开镜像的身份。canonical 取自**已打开的句柄**（Linux：`/proc/self/fd/<n>`），
    /// 不再按命令行路径 canonicalize：open 与 canonicalize 之间有一个窗口，路径被改名或
    /// 替换后，canonicalize 给出的是另一个文件的名字，而写入仍落在 fd 指向的文件上，
    /// journal / checkpoint / lock 因此会挂到另一份数据旁。
    ///
    /// 字面路径仍用于日志命名与历史候选，但前提是它**此刻仍指向同一个 inode**：改名后
    /// 旧路径可能已经属于别人，把它的兄弟文件当成本目标的历史候选，提交清理时就会删到
    /// 别人的 journal。两条检查任一不成立即拒绝（fail-closed），不退到路径身份
    #[cfg(target_os = "linux")]
    pub(crate) fn identity_of_opened_image(
        path: &Path,
        file: &File,
        meta: &std::fs::Metadata,
    ) -> io::Result<TargetIdentity> {
        use std::os::fd::AsRawFd;
        use std::os::unix::fs::MetadataExt;
        let link = PathBuf::from(format!("/proc/self/fd/{}", file.as_raw_fd()));
        let canonical = std::fs::read_link(&link).map_err(|e| {
            io::Error::other(format!(
                "cannot resolve the path of the opened target through {}: {e} — refusing to \
                 derive the target identity from the command-line path alone",
                link.display()
            ))
        })?;
        // 打开之后被 unlink：内核在链接尾部标 " (deleted)"，这个名字已不再指向任何可达
        // 文件，伴随文件无处安放
        if canonical.as_os_str().to_string_lossy().ends_with(" (deleted)") {
            return Err(io::Error::other(format!(
                "the target {} was removed after it was opened — refusing to write through an unnamed file",
                path.display()
            )));
        }
        let literal = std::fs::metadata(path).map_err(|e| {
            io::Error::other(format!(
                "cannot verify that {} still names the opened target: {e}",
                path.display()
            ))
        })?;
        if (literal.dev(), literal.ino()) != (meta.dev(), meta.ino()) {
            return Err(io::Error::other(format!(
                "{} no longer names the file that was opened (path dev:{} ino:{}, opened dev:{} ino:{}) \
                 — refusing to place the undo journal next to another file",
                path.display(),
                literal.dev(),
                literal.ino(),
                meta.dev(),
                meta.ino(),
            )));
        }
        Ok(TargetIdentity::image_at(&canonical, path))
    }

    /// 镜像打开的唯一实现：可写性是显式参数，读写与只读两个入口不各自拼一份
    pub(crate) fn open_image(path: &Path, sector_size_override: Option<u64>, writable: bool) -> io::Result<Self> {
        let mut opts = OpenOptions::new();
        opts.read(true);
        if writable {
            opts.write(true);
        }
        let file = opts.open(path)?;
        // 元数据取自已打开的句柄：先 stat 路径再 open 会在两步之间留下替换窗口——
        // 写句柄指向 B 而越界上界与 journal 身份来自 A
        let meta = file.metadata()?;
        let size = meta.len();
        let sector_size = sector_size_override.unwrap_or(512);
        // 镜像扇区大小须为 2 的幂且落在 512..=65536；这是本工具的自定约束，非规范要求
        if !(512..=65536).contains(&sector_size) || !sector_size.is_power_of_two() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("invalid sector size {sector_size}"),
            ));
        }
        // 身份也取自已打开的句柄：路径在 open 与 canonicalize 之间被替换时，身份与写句柄
        // 会指向两个不同的文件（见 `identity_of_opened_image`）
        #[cfg(target_os = "linux")]
        let identity = Self::identity_of_opened_image(path, &file, &meta)?;
        // 非 Linux 没有从 fd 取路径的标准接口：维持按路径解析（README 的"并发边界"
        // 已声明该平台上的别名路径各有一份现场与一把锁）
        #[cfg(not(target_os = "linux"))]
        let identity = TargetIdentity::resolve_image(path);
        Ok(FileSource {
            identity,
            file,
            path: path.to_path_buf(),
            sector_size,
            size,
            is_block: false,
            journal: None,
            ownership: None,
            fingerprint: TargetFingerprint::from_meta(&meta),
            loop_mapping: None,
        })
    }

    /// 只读打开块设备（在线路径识别 FS 用：读写 + O_EXCL 在设备被 claim 时会失败）。
    /// 身份解析失败（fail-closed）⇒ 打开失败
    #[cfg(target_os = "linux")]
    pub(crate) fn open_read_only(path: &Path) -> io::Result<Self> {
        let file = OpenOptions::new().read(true).open(path)?;
        let size = ioctl::blkgetsize64(&file)?;
        let sector_size = ioctl::blksszget(&file)? as u64;
        // 与可写打开同口径：身份与映射都从 fd 的 rdev 派生
        let rdev = {
            use std::os::unix::fs::MetadataExt;
            file.metadata()?.rdev()
        };
        let node = sysfs_node_from_rdev(rdev)?;
        let identity = TargetIdentity::resolve_block_at(&node, path).map_err(io::Error::other)?;
        let (fingerprint, loop_mapping) = Self::block_open_extras(&identity, &node)?;
        Ok(FileSource {
            identity,
            file,
            path: path.to_path_buf(),
            sector_size,
            size,
            is_block: true,
            journal: None,
            ownership: None,
            fingerprint,
            loop_mapping,
        })
    }

    /// 非 Linux 平台的占位实现：块设备判定与 ioctl 都不可用，只读路径按普通文件打开。
    /// 该平台上身份解析恒为镜像（`is_block()` 恒 false），此函数不会被块设备路径走到
    #[cfg(not(target_os = "linux"))]
    pub(crate) fn open_read_only(path: &Path) -> io::Result<Self> {
        let file = File::open(path)?;
        let meta = file.metadata()?;
        let size = meta.len();
        Ok(FileSource {
            identity: TargetIdentity::resolve_image(path),
            file,
            path: path.to_path_buf(),
            sector_size: 512,
            size,
            is_block: false,
            journal: None,
            ownership: None,
            fingerprint: TargetFingerprint::from_meta(&meta),
            loop_mapping: None,
        })
    }

    /// pread 语义（不移动文件游标，&self 可调用）；不足 buf 长度报错
    pub fn read_at(&self, off: u64, buf: &mut [u8]) -> io::Result<()> {
        #[cfg(test)]
        if READ_FAULT.with(|f| f.get()) == Some(off) {
            return Err(io::Error::other("injected read fault"));
        }
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
                let at = off.checked_add(done as u64).ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidInput, "read offset overflows")
                })?;
                let n = self.file.seek_read(&mut buf[done..], at)?;
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
        self.refuse_past_end(off, buf.len())?;
        if self.journal.is_some() {
            // 越界已在上面拒掉，这里读到的总是目标范围内的真实原文，不需要按 EOF 截断
            let mut orig = vec![0u8; buf.len()];
            self.read_at(off, &mut orig)?;
            // journal 先行落盘：写入发生前，被覆盖字节的原文必须已持久化
            if let Some(journal) = self.journal.as_mut() {
                journal.record(off, &orig, buf)?;
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

    /// 越界写拒绝。镜像上越界写会静默把文件稀疏扩出来——分区表几何一旦算错，
    /// 那就是一次无声的目标扩张；块设备上则由内核以 EIO 拒绝。两侧同一条判据
    pub(crate) fn refuse_past_end(&self, off: u64, len: usize) -> io::Result<()> {
        let end = off.checked_add(len as u64).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("write of {len} byte(s) at offset {off} overflows the offset range"),
            )
        })?;
        if end > self.size {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "write of {len} byte(s) at offset {off} ends at {end}, past the target's {} byte(s) — \
                     refusing to extend the target",
                    self.size
                ),
            ));
        }
        Ok(())
    }

    pub(crate) fn write_raw(&mut self, off: u64, buf: &[u8]) -> io::Result<()> {
        self.refuse_past_end(off, buf.len())?;
        use std::io::{Seek, SeekFrom, Write};
        self.file.seek(SeekFrom::Start(off))?;
        self.file.write_all(buf)
    }

    /// 记下"本次操作含有不可回滚的写入"，undo 见到它即拒绝回滚。
    ///
    /// 判据是**可逆性**，不是"是不是外部进程"：数据搬移的字节（`write_data_at`）是本进程
    /// 写的，但按设计故意不入 journal；外部 FS 工具的写入（resize2fs/mkswap/mkfs…）
    /// 与内核侧的分区表写入同样无法回滚。凡"回滚表项会与盘上内容自相矛盾"的写入都属于这一类，
    /// 它们共同落一条屏障，屏障上的 mutation 说明是哪一类越过了这条线
    pub fn mark_non_reversible(&mut self) -> io::Result<()> {
        if let Some(journal) = self.journal.as_mut() {
            journal.barrier()?;
        }
        Ok(())
    }

    /// 声明"接下来这类改动是什么"，作用于随后的 pre-image 与屏障记录。
    /// 语义由知道自己在做什么的那一层给出——journal 只负责记，不负责猜
    pub fn set_mutation(&mut self, m: Mutation) {
        if let Some(journal) = self.journal.as_mut() {
            journal.set_mutation(m);
        }
    }

    /// 每步落盘。容量有变化的场景用 sync_all，纯数据覆盖用 sync_data。
    pub fn sync_all(&self) -> io::Result<()> {
        self.file.sync_all()
    }

    pub fn sync_data(&self) -> io::Result<()> {
        self.file.sync_data()
    }
}