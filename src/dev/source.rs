//! FileSource：镜像与块设备统一的按偏移读写句柄；WriteSource：在其上附加独占所有权的写句柄。

use super::*;
use crate::outcome::WriteStep;
use sha2::{Digest, Sha256};

/// 一段区间的写前期望：偏移、长度与区间内字节的 sha256。
///
/// `digest` 为 `None` 表示该区间没有可比对的期望内容（数据搬移的目的区间不承诺
/// 原来是什么），写入口因此只做写后回读，不做写前比对。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RegionStamp {
    offset: u64,
    len: u64,
    digest: Option<[u8; 32]>,
}

impl RegionStamp {
    /// 由已读到的区间原文构造（计划期快照、调用点刚读的扇区）
    pub(crate) fn of_bytes(offset: u64, bytes: &[u8]) -> Self {
        Self { offset, len: bytes.len() as u64, digest: Some(Sha256::digest(bytes).into()) }
    }

    /// 没有期望内容的区间：写入只受写后回读约束
    pub(crate) fn unconstrained(offset: u64, len: u64) -> Self {
        Self { offset, len, digest: None }
    }

    pub(crate) fn offset(&self) -> u64 {
        self.offset
    }

    pub(crate) fn len(&self) -> u64 {
        self.len
    }

    /// 区间字节是否就是这份期望；没有期望内容时恒为真
    pub(crate) fn accepts(&self, bytes: &[u8]) -> bool {
        match self.digest {
            Some(digest) => <[u8; 32]>::from(Sha256::digest(bytes)) == digest,
            None => true,
        }
    }
}

/// 受守卫写入的拒绝理由：写前重读与期望不等，或重读/写入本身失败。
/// 两种情形都意味着"这一刻没有按计划写出"，调用方据此决定不推进进度
#[derive(Debug)]
pub(crate) struct WriteConflict {
    reason: String,
    kind: io::ErrorKind,
}

impl WriteConflict {
    fn changed(offset: u64, len: usize) -> Self {
        Self {
            reason: format!(
                "the target bytes at [{offset}..{}) no longer match what was read before writing — \
                 refusing to overwrite them",
                offset.saturating_add(len as u64)
            ),
            kind: io::ErrorKind::InvalidData,
        }
    }

    fn io(offset: u64, len: usize, what: &str, e: io::Error) -> Self {
        Self {
            reason: format!("{what} at byte {offset} ({len} byte(s)): {e}"),
            kind: e.kind(),
        }
    }

    pub(crate) fn into_io_error(self) -> io::Error {
        io::Error::new(self.kind, self.reason)
    }
}

impl std::fmt::Display for WriteConflict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.reason)
    }
}

/// 本进程内已提交给恢复记录的写次数。崩溃注入按"第 N 次写入后"触发，用来留下
/// 含多条区间的记录（只按第一次触发时记录里永远只有一条）
static RECORD_WRITES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

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
    /// 目标身份：打开时解析一次，record / undo 只消费它
    pub identity: TargetIdentity,
    /// 单文件恢复记录：每条写命令只有这一种凭据
    pub record_journal: Option<RecordJournal>,
    /// 目标文件的稳定指纹：恢复现场用它确认"路径下的文件还是当时
    /// 那个"。镜像与 loop 的 backing 文件取文件元数据；真块设备取**整盘**容量与整盘设备 ID
    /// （dev / ino 留零——节点 inode 不承诺稳定、节点元数据长度也恒 0）。**比对逐字段进行**：
    /// 零值表示该字段没有证据，不参与比对（见 [`TargetFingerprint::has_inode_evidence`]）
    pub fingerprint: TargetFingerprint,
    /// loop 映射事实（offset, sizelimit）：目标经由 loop 设备进入时在打开时读一次
    /// sysfs。checkpoint 恢复时与记录值比对，重 attach 参数变化报"映射已变化"
    /// 而非几何不符的泛化拒绝
    pub loop_mapping: Option<(u64, u64)>,
    /// 目标文件的所有者 (uid, gid)，取自打开句柄；块设备为 `None`。镜像侧恢复记录
    /// 用它保证与目标同主
    pub owner: Option<(u32, u32)>,
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
    /// 即将落盘的那一笔写入的归属。写入口在调用写通道前设置、返回后清除；
    /// `write_raw` 据此把首写步骤记入本进程的写事实
    pending_step: Option<WriteStep>,
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

/// 记录目录准备失败的出口：块设备侧缺 root 或仓库形状不符是能力缺口（6），
/// 镜像侧目标旁的目录形状不符属前置拒绝（3），I/O 失败按未改动落 3
fn prepare_dir_fail(src: &FileSource, e: crate::dev::HostRepoError) -> crate::outcome::RunOutcome {
    use crate::dev::HostRepoError;
    match e {
        HostRepoError::NotRoot => crate::outcome::RunOutcome::unsupported(
            "the host recovery repository must be owned by uid 0; table writes to a block device \
             are refused when this process is not root",
        ),
        HostRepoError::Insecure(detail) if src.is_block => crate::outcome::RunOutcome::unsupported(format!(
            "the host recovery repository is refused: {detail}"
        )),
        HostRepoError::Insecure(detail) => {
            crate::outcome::RunOutcome::refused(format!("the recovery directory is refused: {detail}"))
        }
        HostRepoError::Io(e) => {
            crate::outcome::RunOutcome::refused(format!("cannot prepare the recovery directory: {e}"))
        }
    }
}

impl WriteSource {
    /// 取目标的独占所有权，把已打开的句柄升格为写句柄。锁落点由身份恒给出
    /// （见 [`TargetIdentity::lock_path`]），取不到即拒绝——没有"无锁继续写"这条路径
    pub(crate) fn acquire(src: FileSource) -> Result<Self, crate::outcome::RunOutcome> {
        // 运行边界：新的写句柄即新的运行，写事实从这里重新起算
        crate::outcome::reset_run_facts();
        // 锁文件落在记录目录内，目录必须先于取锁就位
        crate::dev::prepare_record_dir(&src.identity, src.owner).map_err(|e| prepare_dir_fail(&src, e))?;
        let lock = crate::targetlock::TargetLock::acquire(&src.identity, Some(&src.fingerprint))?;
        // 锁已到手，先按路径重探身份再继续；锁前解析的身份不能直接沿用
        src.reprobe_under_lock()?;
        Ok(Self { src, _lock: Some(lock), pending_step: None })
    }

    /// 测试专用：不取锁的写句柄。`#[cfg(test)]` 之外构造不出来，生产代码只能经
    /// [`Self::acquire`] 拿到写权限
    #[cfg(test)]
    pub(crate) fn unowned(src: FileSource) -> Self {
        // 运行边界：每个用例自建写句柄，写事实从构造点重新起算
        crate::outcome::reset_run_facts();
        Self { src, _lock: None, pending_step: None }
    }

    /// 受守卫写入：先重读区间与 `expect` 比对，相符才写。
    ///
    /// 写通道按步骤分派：`Relocate` 是数据块，走不入记录的 `write_data_at`；
    /// 其余是元数据，走先登记原文的 `write_at`。步骤因此只由施加者给出一次，
    /// 既决定归属也决定通道，调用点不再各自挑
    pub(crate) fn write_guarded(
        &mut self,
        off: u64,
        buf: &[u8],
        expect: RegionStamp,
        step: WriteStep,
    ) -> Result<(), WriteConflict> {
        if let Some(digest) = expect.digest {
            if expect.offset != off || expect.len != buf.len() as u64 {
                return Err(WriteConflict::changed(off, buf.len()));
            }
            let mut current = vec![0u8; buf.len()];
            self.read_at(off, &mut current)
                .map_err(|e| WriteConflict::io(off, buf.len(), "cannot re-read the target before writing", e))?;
            if <[u8; 32]>::from(Sha256::digest(&current)) != digest {
                return Err(WriteConflict::changed(off, buf.len()));
            }
        }
        self.pending_step = Some(step);
        let result = if matches!(step, WriteStep::Relocate { .. }) {
            self.write_data_at(off, buf)
        } else {
            self.write_at(off, buf)
        };
        self.pending_step = None;
        result.map_err(|e| WriteConflict::io(off, buf.len(), "cannot write the target", e))
    }

    /// 受守卫写入 + 写后立刻回读比对。回读不等按 `InvalidData` 返回，
    /// 调用方不得推进进度
    pub(crate) fn write_converged(
        &mut self,
        off: u64,
        buf: &[u8],
        expect: RegionStamp,
        step: WriteStep,
    ) -> io::Result<()> {
        self.write_guarded(off, buf, expect, step).map_err(WriteConflict::into_io_error)?;
        self.read_back(off, buf)
    }

    /// 回读一段并与期望字节比对。批量落盘的数据搬移按批次调用它，
    /// 回读粒度因此跟住 durable 批次而不是单块
    pub(crate) fn read_back(&self, off: u64, expected: &[u8]) -> io::Result<()> {
        let mut now = vec![0u8; expected.len()];
        self.read_at(off, &mut now).map_err(|e| {
            io::Error::new(e.kind(), format!("cannot read back the bytes written at byte {off}: {e}"))
        })?;
        if now != expected {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "the bytes read back at byte {off} differ from what was written ({} byte(s)) — the write may not have landed",
                    expected.len()
                ),
            ));
        }
        Ok(())
    }

    /// 元数据写入：被覆盖字节的原文先入恢复记录再写。
    pub fn write_at(&mut self, off: u64, buf: &[u8]) -> io::Result<()> {
        self.refuse_past_end(off, buf.len())?;
        if self.src.record_journal.is_some() && !buf.is_empty() {
            // 单文件记录的粒度是扇区：把写入区间按扇区边界扩到整扇区，
            // 记录这些扇区的完整原文（最后一扇区可以止于文件末尾）
            let sector_size = self.sector_size;
            let first = off - off % sector_size;
            let end = ((off + buf.len() as u64).div_ceil(sector_size))
                .saturating_mul(sector_size)
                .min(self.size);
            let mut orig = vec![0u8; (end - first) as usize];
            self.read_at(first, &mut orig)?;
            // 写入后的整扇区样子：原文叠上本次写入的字节。undo 用它核对"盘面仍是记录
            // 写后的样子"，只存原文无法把外部改动与本次写入区分开
            let mut after = orig.clone();
            let at = (off - first) as usize;
            after[at..at + buf.len()].copy_from_slice(buf);
            if let Some(journal) = self.src.record_journal.as_mut() {
                journal.record_span(first / sector_size, orig, after)?;
            }
            crate::movepart::fault_record_persisted();
        }
        self.write_raw(off, buf)?;
        if self.src.record_journal.is_some() {
            let nth = RECORD_WRITES.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
            crate::movepart::fault_record_before_commit(nth);
        }
        Ok(())
    }

    /// 数据块写入（搬移的 chunk 拷贝）：不入 undo journal。
    /// 搬移的设计是"前向恢复、无回滚"（见 movepart 模块注释），undo 不消费数据字节，
    /// 记录它们只会产生与搬移量等大的 journal。数据一致性由 ckpt + 幂等重做保证
    ///
    /// 屏障由这里自己落，不靠调用点先记得 mark_non_reversible：数据一旦写下去，
    /// 回滚表项就会与盘上内容自相矛盾。同一 mutation 下屏障只落一条
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
        match self.pending_step {
            Some(step) => crate::outcome::note_target_write_step(step),
            None => crate::outcome::note_target_write(),
        }
        self.src.file.write_all(buf)
    }

    /// 记下"本次操作含有不可回滚的写入"，undo 见到它即拒绝回滚。
    ///
    /// 判据是**可逆性**，不是"是不是外部进程"：数据搬移的字节（`write_data_at`）是本进程
    /// 写的，但按设计不入记录；外部 FS 工具的写入（resize2fs/mkswap/mkfs…）
    /// 与内核侧的分区表写入同样无法回滚。凡"回滚表项会与盘上内容自相矛盾"的写入都属于这一类
    pub fn mark_non_reversible(&mut self) -> io::Result<()> {
        if let Some(journal) = self.src.record_journal.as_mut() {
            journal.barrier()?;
            crate::outcome::note_record_past_irreversible_point();
        }
        Ok(())
    }

    pub(crate) fn adopt_record_journal(&mut self, journal: RecordJournal) {
        let record = journal.record();
        crate::outcome::note_record_present(record.committed());
        if record.restore_blocked || record.committed() {
            crate::outcome::note_record_past_irreversible_point();
        }
        self.src.record_journal = Some(journal);
    }

    /// 把单文件恢复记录取回来（成功收尾后提交/清理）
    pub(crate) fn take_record_journal(&mut self) -> Option<RecordJournal> {
        self.src.record_journal.take()
    }

    /// 只借出记录，用于判断"记录是否已经落盘"
    pub(crate) fn record_journal(&self) -> Option<&RecordJournal> {
        self.src.record_journal.as_ref()
    }

    /// 借出可写的记录：搬移类命令在写盘之间推进游标/FS 相位，记录就在写句柄里
    pub(crate) fn record_journal_mut(&mut self) -> Option<&mut RecordJournal> {
        self.src.record_journal.as_mut()
    }
}

/// 扇区大小经 [`SectorSize`](crate::domain::SectorSize) 的单点校验：2 的幂、不低于
/// 512 字节，且不超过本工具 65536 字节的实现边界（字段宽度不是兼容性上限）。
/// 换算按 LBA 的调用点以它为分母，0 或非 2 的幂会把尺寸静默压成偏移 0
fn check_sector_size(sector_size: u64) -> io::Result<()> {
    crate::domain::SectorSize::try_new(sector_size)
        .map(|_| ())
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e.to_string()))
}

impl FileSource {
    /// 目标是否是内核标记为只读的块设备（BLKROGET）。只读设备上的任何写都会在
    /// 内核处被拒，而那个拒绝以 pwrite 的错误出现在计划步上——记录已落盘，退出码
    /// 落进"盘可能已改变"的一档。写命令准入前在这里判掉，只读目标保持未写盘
    #[cfg(target_os = "linux")]
    pub(crate) fn read_only_block(&self) -> bool {
        self.is_block && crate::ioctl::blkroget(&self.file).unwrap_or(false)
    }

    /// 底层文件元数据。**只给这一点能力，不交出句柄**：`&File` 也实现 `Write`，
    /// 交出去就等于在 journal 与越界检查之外重开一条写路（见字段 `file` 的注释）。
    /// 占用探测与指纹构造取它；只有 Linux 路径要它（sysfs 反查与 loop 占用探测）
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

    /// 已打开块设备的事实收集：容量、逻辑扇区、身份与 loop 映射。读写与只读两个入口
    /// 共用这一处——身份与映射一律按 fd 的 rdev 反查 sysfs，不按路径二次 stat；
    /// /dev 节点在窗口内被 udev 重建时，fd 与身份描述会指向不同设备
    #[cfg(target_os = "linux")]
    fn block_source(file: File, path: &Path) -> io::Result<Self> {
        let size = ioctl::blkgetsize64(&file)?;
        let sector_size = ioctl::blksszget(&file)? as u64;
        check_sector_size(sector_size)?;
        let rdev = {
            use std::os::unix::fs::MetadataExt;
            file.metadata()?.rdev()
        };
        let node = sysfs_node_from_rdev(rdev)?;
        // 块设备身份解析失败（fail-closed）⇒ 打开失败：错误信息带设备名
        let identity = TargetIdentity::resolve_block_at(&node, path).map_err(io::Error::other)?;
        let (fingerprint, loop_mapping) = Self::block_open_extras(&identity, &node)?;
        Ok(FileSource {
            identity,
            file,
            path: path.to_path_buf(),
            sector_size,
            size,
            is_block: true,
            record_journal: None,
            fingerprint,
            loop_mapping,
            owner: None,
        })
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
                return Self::block_source(file, path);
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
    /// 字面路径只用于日志命名，且前提是它**此刻仍指向同一个 inode**：改名后，改名前的
    /// 路径可能已经属于别人，把它的兄弟文件当成本目标的伴随文件，提交清理时就会删到
    /// 别人的 journal。检查不成立即拒绝（fail-closed），不退到路径身份
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
        // 非 Linux 没有从 fd 取路径的标准接口：维持按路径解析；该平台上别名路径各有一份现场与一把锁
        #[cfg(not(target_os = "linux"))]
        let identity = TargetIdentity::resolve_image(path);
        let fingerprint = TargetFingerprint::from_file(&file)?;
        #[cfg(target_os = "linux")]
        let owner = {
            use std::os::unix::fs::MetadataExt;
            Some((meta.uid(), meta.gid()))
        };
        #[cfg(not(target_os = "linux"))]
        let owner = None;
        Ok(FileSource {
            identity,
            file,
            path: path.to_path_buf(),
            sector_size,
            size,
            is_block: false,
            record_journal: None,
            fingerprint,
            loop_mapping: None,
            owner,
        })
    }

    /// 锁内重探身份：取锁后重新从命令行路径解析目标，与打开时解析的
    /// 身份比对。块设备比设备身份派生的 journal / checkpoint / record 落点；镜像
    /// 比 canonical 路径与已打开句柄的 (dev, ino)
    pub(crate) fn reprobe_under_lock(&self) -> Result<(), crate::outcome::RunOutcome> {
        use crate::outcome::RunOutcome;
        if self.is_block {
            #[cfg(target_os = "linux")]
            {
                let now = TargetIdentity::resolve_block(&self.path).map_err(|e| {
                    RunOutcome::refused(format!(
                        "the target cannot be re-read after taking the lock: {e}"
                    ))
                })?;
                if now.record_path() != self.identity.record_path()
                    || now.journal_path() != self.identity.journal_path()
                    || now.lock_path() != self.identity.lock_path()
                {
                    return Err(RunOutcome::refused(
                        "the device node now resolves to a different target than the one opened \
                         before the lock",
                    ));
                }
            }
            return Ok(());
        }
        let now_identity = TargetIdentity::resolve_image(&self.path);
        if now_identity.canonical != self.identity.canonical {
            return Err(RunOutcome::refused(
                "the path now resolves to a different file than the one opened before the lock",
            ));
        }
        let file = std::fs::File::open(&self.path).map_err(|e| {
            RunOutcome::refused(format!(
                "the target cannot be reopened after taking the lock: {e}"
            ))
        })?;
        let now = TargetFingerprint::from_file(&file).map_err(|e| {
            RunOutcome::refused(format!(
                "cannot read the target identity after taking the lock: {e}"
            ))
        })?;
        if self.fingerprint.has_inode_evidence()
            && (!now.has_inode_evidence()
                || (now.dev, now.ino) != (self.fingerprint.dev, self.fingerprint.ino))
        {
            return Err(RunOutcome::refused(
                "the path no longer names the file that was opened before the lock",
            ));
        }
        Ok(())
    }

    /// 只读打开块设备（在线路径识别 FS 用：读写 + O_EXCL 在设备被 claim 时会失败）。
    /// 身份解析失败（fail-closed）⇒ 打开失败
    #[cfg(target_os = "linux")]
    pub(crate) fn open_read_only(path: &Path) -> io::Result<Self> {
        Self::block_source(OpenOptions::new().read(true).open(path)?, path)
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
            record_journal: None,
            fingerprint,
            loop_mapping: None,
            owner: None,
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
