//! FileSource：镜像与块设备统一的按偏移读写句柄；WriteSource：在其上附加独占所有权的写句柄。

use super::*;

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

pub struct FileSource {
    /// 底层句柄。**生产构建里只有本模块能碰**：`&File` 同样实现 `Write`，交出任何一个
    /// 引用都等于在 journal 与越界检查之外重开一条写路，故外部只能经 [`Self::metadata`]
    /// 与 [`Self::reread_partitions`] 取到它被允许的那一点能力。测试要拼装夹具，那一侧
    /// 放宽到 crate 可见
    #[cfg(test)]
    pub(crate) file: File,
    #[cfg(not(test))]
    file: File,
    pub path: PathBuf,
    pub sector_size: u64,
    pub size: u64,
    pub is_block: bool,
    /// 目标身份：打开时解析一次，journal / checkpoint / undo 只消费它
    pub identity: TargetIdentity,
    /// undo journal：只记录本工具 write_at 的直接写入
    pub journal: Option<Journal>,
    /// 目标文件的稳定指纹：恢复现场（journal / checkpoint）用它确认"路径下的文件还是当时
    /// 那个"。镜像与 loop 的 backing 文件取文件元数据；真块设备取**整盘**容量与整盘设备 ID
    /// （dev / ino 留零——节点 inode 不承诺稳定、节点元数据长度也恒 0）。**比对逐字段进行**：
    /// 零值表示该字段没有证据，不参与比对（见 [`scene_mismatch`]）
    pub fingerprint: TargetFingerprint,
    /// loop 映射事实（offset, sizelimit）：目标经由 loop 设备进入时在打开时读一次
    /// sysfs。checkpoint 恢复时与记录值比对，重 attach 参数变化报"映射已变化"
    /// 而非几何不符的泛化拒绝
    pub loop_mapping: Option<(u64, u64)>,
}

/// 写句柄：在 [`FileSource`] 之外**持有**一份目标独占所有权（见 `targetlock`）。
///
/// 写方法（`write_at` / `write_data_at` / `write_raw`，以及 journal 的屏障与语义标注）
/// 只挂在本类型上：只读打开得到的 [`FileSource`] 在类型层面就没有写盘接口，生产写入口
/// 因此绕不过"持锁"这一步。构造只有两条路——[`Self::acquire`] 取锁（生产唯一入口）与
/// 测试专用的 [`Self::unowned`]（`#[cfg(test)]`，不取锁）。字段私有，crate 内没有第三种
/// 拼装方式
pub struct WriteSource {
    src: FileSource,
    /// 独占所有权的持有者。字段不被读取，存在的意义是被持有到写完为止；析构即释放锁
    _lock: Option<crate::targetlock::TargetLock>,
}

impl std::ops::Deref for WriteSource {
    type Target = FileSource;

    fn deref(&self) -> &FileSource {
        &self.src
    }
}

/// 测试专用的可变解引用：夹具常直接改 `size` / `fingerprint` 这类字段来构造几何。
/// **生产构建里没有它**——与字段 `file` 的 `cfg` 放宽同一条边界：crate 内其余代码
/// 拿不到 `&mut FileSource`，也就改不动 journal 与指纹
#[cfg(test)]
impl std::ops::DerefMut for WriteSource {
    fn deref_mut(&mut self) -> &mut FileSource {
        &mut self.src
    }
}

impl WriteSource {
    /// 取目标的独占所有权，把已打开的句柄升格为写句柄。锁落点由身份恒给出
    /// （见 [`TargetIdentity::lock_path`]），取不到即拒绝——没有"无锁继续写"这条路径
    pub(crate) fn acquire(src: FileSource) -> Result<Self, crate::outcome::Fail> {
        let lock = crate::targetlock::TargetLock::acquire(&src.identity, Some(&src.fingerprint))?;
        // ckpt 残骸清扫必须在锁下：未持锁时删除会命中另一个进程正在写的暂存文件
        sweep_stale_ckpt_temps(&src.identity);
        Ok(Self { src, _lock: Some(lock) })
    }

    /// 测试专用：不取锁的写句柄。`#[cfg(test)]` 之外构造不出来，生产代码只能经
    /// [`Self::acquire`] 拿到写权限
    #[cfg(test)]
    pub(crate) fn unowned(src: FileSource) -> Self {
        Self { src, _lock: None }
    }

    /// 采纳既有 journal（事务层取锁后调用）。这是 `journal` 字段唯一的对外写入口：
    /// 其余路径只能经 pre-image 记录与屏障影响它
    pub(crate) fn adopt_journal(&mut self, journal: Journal) {
        self.src.journal = Some(journal);
    }

    /// 元数据写入：被覆盖字节的原文先入 undo journal 再写
    pub fn write_at(&mut self, off: u64, buf: &[u8]) -> io::Result<()> {
        self.refuse_past_end(off, buf.len())?;
        if self.journal.is_some() {
            // 越界已在上面拒掉，这里读到的总是目标范围内的真实原文，不需要按 EOF 截断
            let mut orig = vec![0u8; buf.len()];
            self.read_at(off, &mut orig)?;
            // journal 先行落盘：写入发生前，被覆盖字节的原文必须已持久化
            if let Some(journal) = self.src.journal.as_mut() {
                journal.record(off, &orig, buf)?;
            }
        }
        self.write_raw(off, buf)
    }

    /// 数据块写入（搬移的 chunk 拷贝）：不入 undo journal。
    /// 搬移的设计是"前向恢复、无回滚"（见 movepart 模块注释），undo 不消费数据字节，
    /// 记录它们只会产生与搬移量等大的 journal。数据一致性由 ckpt + 幂等重做保证
    ///
    /// 屏障由这里自己落，**不靠调用点先记得 mark_non_reversible**：数据一旦写下去，
    /// 回滚表项就会与盘上内容自相矛盾。同一 mutation 下屏障只落一条（见 [`Journal::barrier`]），
    /// 逐 chunk 调用因此不产生额外记录
    pub fn write_data_at(&mut self, off: u64, buf: &[u8]) -> io::Result<()> {
        // 越界判定先于屏障：屏障一旦落地 undo 即永久失效，而越界写一个字节也不会落盘
        self.refuse_past_end(off, buf.len())?;
        self.mark_non_reversible()?;
        self.write_raw(off, buf)
    }

    fn write_raw(&mut self, off: u64, buf: &[u8]) -> io::Result<()> {
        self.refuse_past_end(off, buf.len())?;
        use std::io::{Seek, SeekFrom, Write};
        self.src.file.seek(SeekFrom::Start(off))?;
        self.src.file.write_all(buf)
    }

    /// 记下"本次操作含有不可回滚的写入"，undo 见到它即拒绝回滚。
    ///
    /// 判据是**可逆性**，不是"是不是外部进程"：数据搬移的字节（`write_data_at`）是本进程
    /// 写的，但按设计故意不入 journal；外部 FS 工具的写入（resize2fs/mkswap/mkfs…）
    /// 与内核侧的分区表写入同样无法回滚。凡"回滚表项会与盘上内容自相矛盾"的写入都属于这一类，
    /// 它们共同落一条屏障，屏障上的 mutation 说明是哪一类越过了这条线
    pub fn mark_non_reversible(&mut self) -> io::Result<()> {
        if let Some(journal) = self.src.journal.as_mut() {
            journal.barrier()?;
        }
        Ok(())
    }

    /// 声明"接下来这类改动是什么"，作用于随后的 pre-image 与屏障记录。
    /// 语义由知道自己在做什么的那一层给出——journal 只负责记，不负责猜
    pub fn set_mutation(&mut self, m: Mutation) {
        if let Some(journal) = self.src.journal.as_mut() {
            journal.set_mutation(m);
        }
    }
}

/// 扇区大小须为 2 的幂且落在 512..=65536；这是本工具的自定约束，非规范要求。
/// 换算按 LBA 的调用点以它为分母，0 或非 2 的幂会把尺寸静默压成偏移 0
fn check_sector_size(sector_size: u64) -> io::Result<()> {
    if !(512..=65536).contains(&sector_size) || !sector_size.is_power_of_two() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, format!("invalid sector size {sector_size}")));
    }
    Ok(())
}

impl FileSource {
    /// 底层文件元数据。**只给这一点能力，不交出句柄**：`&File` 也实现 `Write`，
    /// 交出去就等于在 journal 与越界检查之外重开一条写路（见字段 `file` 的注释）
    ///
    /// 只有 Linux 路径要它（sysfs 反查与 loop 占用探测），故与那些调用点同 cfg
    #[cfg(target_os = "linux")]
    pub(crate) fn metadata(&self) -> io::Result<std::fs::Metadata> {
        self.file.metadata()
    }

    /// 让内核重读分区表。同样只此一条：调用方要的是"重读"这个动作，不是句柄
    #[cfg(target_os = "linux")]
    pub(crate) fn reread_partitions(&self) -> io::Result<()> {
        crate::ioctl::blkrrpart(&self.file)
    }

    /// 现在这个目标能给出的身份证据种类。与指纹的构造一一对应：镜像恒取文件 inode；
    /// 块设备上整盘属性可读 ⇒ `DeviceId`，loop（指纹取自 backing 文件）⇒ `Inode`，
    /// 两者都没有 ⇒ `Unavailable`
    pub(crate) fn proof(&self) -> LiveProof {
        if !self.is_block {
            return LiveProof::Inode;
        }
        if self.fingerprint.has_id_evidence() {
            LiveProof::DeviceId
        } else if self.fingerprint.has_inode_evidence() {
            LiveProof::Inode
        } else {
            LiveProof::Unavailable
        }
    }

    /// 现场比对用的"当前身份"：与 journal / checkpoint 头里存的那三样同构
    pub(crate) fn scene(&self) -> LiveScene {
        LiveScene { fp: self.fingerprint, map: self.loop_mapping, proof: self.proof() }
    }

    /// 块设备打开的公共附加物：指纹 + loop 映射。
    /// loop 的身份是 backing 文件：指纹取它；真块设备取整盘容量与整盘设备 ID——两者都按
    /// 整设备节点算，`/dev/sdb` 与 `/dev/sdb2` 因此共享同一份指纹（现场按盘共享）。
    /// `node` 必须与身份解析用的是同一个 sysfs 节点（打开路径按 fd 的 rdev 定位），
    /// 否则路径在窗口内被重建时映射事实会来自另一台设备
    #[cfg(target_os = "linux")]
    pub(crate) fn block_open_extras(identity: &TargetIdentity, node: &Path) -> io::Result<(TargetFingerprint, Option<(u64, u64)>)> {
        let fp = if identity.kind == TargetKind::Image {
            // 指纹取自身份解析时核对过的那个 backing 对象（见 TargetIdentity::loop_backed）；
            // 解析时没有留下指纹（非 loop 路径）才按打开后的数据路径取
            match identity.backing_fp {
                Some(fp) => fp,
                None => TargetFingerprint::from_meta(&std::fs::metadata(identity.data_path())?),
            }
        } else {
            let (size, id) = disk_identity(node)?;
            TargetFingerprint { dev: 0, ino: 0, size, id: id.as_deref().map_or(0, |s| fnv1a64(s.as_bytes())) }
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
                check_sector_size(sector_size)?;
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
        check_sector_size(sector_size)?;
        // 身份也取自已打开的句柄：路径在 open 与 canonicalize 之间被替换时，身份与写句柄
        // 会指向两个不同的文件（见 `identity_of_opened_image`）
        #[cfg(target_os = "linux")]
        let identity = Self::identity_of_opened_image(path, &file, &meta)?;
        // 非 Linux 没有从 fd 取路径的标准接口：维持按路径解析（README 的"并发边界"
        // 已声明该平台上的别名路径各有一份现场与一把锁）
        #[cfg(not(target_os = "linux"))]
        let identity = TargetIdentity::resolve_image(path);
        let fingerprint = TargetFingerprint::from_file(&file)?;
        Ok(FileSource {
            identity,
            file,
            path: path.to_path_buf(),
            sector_size,
            size,
            is_block: false,
            journal: None,
            fingerprint,
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
        check_sector_size(sector_size)?;
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
        let fingerprint = TargetFingerprint::from_file(&file)?;
        Ok(FileSource {
            identity: TargetIdentity::resolve_image(path),
            file,
            path: path.to_path_buf(),
            sector_size: 512,
            size,
            is_block: false,
            journal: None,
            fingerprint,
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
        // 镜像的边界取文件**此刻**的长度：`self.size` 是打开时的快照，镜像在本进程运行
        // 期间被外部截短时，按快照放行会让 seek + write_all 把文件稀疏扩回旧长度——
        // 正是上面那条上界要防的静默扩张。块设备的容量不受这种改写影响，故只查镜像
        if !self.is_block {
            let now = self.file.metadata()?.len();
            if end > now {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "write of {len} byte(s) at offset {off} ends at {end}, but the image is now {now} byte(s) \
                         (opened as {}) — refusing to extend the target",
                        self.size
                    ),
                ));
            }
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
