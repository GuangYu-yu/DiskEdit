//! 恢复记录：记录目录内 `record` 的惰性创建、整份原子替换、读取与收尾。
//!
//! 记录在**第一次写盘前**落盘：`record_span` 先把被覆盖扇区的原文写进记录并持久化，
//! 调用方随后才写目标字节。整份记录用同目录临时文件 + 改名 + 目录 fsync 替换；首次创建
//! 前先确认落点没有同名条目，不覆盖任何已存在的记录。
//!
//! 回滚载荷是"被覆盖扇区的原始字节 + LBA 区间 + 扇区大小"，不含表语义差分；回滚只把
//! 字节写回原位，不重新解析。写入总是按扇区边界取整记录：一个只覆盖部分扇区的写入，
//! 记录该扇区的全部原文。
//!
//! 删除只经 [`RecoveryEngine::commit`] 签发提交记录与清理许可后调用
//! [`RecoveryEngine::cleanup`]。非 Linux 平台是预留的 fail-closed 端口：没有目录相对
//! 原语与目录持久化，构造与读写一律拒绝。

use std::path::{Path, PathBuf};

#[cfg(target_os = "linux")]
use std::os::unix::ffi::OsStrExt;

#[cfg(target_os = "linux")]
use crate::platform::evidence_store::HostEvidenceStore;
#[cfg(target_os = "linux")]
use crate::platform::{
    DirDurability, DirFd, EvidenceFile, EvidenceIo, EvidenceStore, PathError, RelativePath,
};

use super::evidence::Evidence;
#[cfg(target_os = "linux")]
use super::cleanup::{CleanupError, RecoveryEngine};
use super::record::{OpKind, Record};
#[cfg(target_os = "linux")]
use super::record::{
    CommitState, CopySource, LvmPost, PlanDigest, PlanId, PlanVersion, Progress, RecoveryAction,
    Relocation, RestoreEntry, TxId,
};
use super::scene::SceneIdentity;
use crate::dev::DiskGuidState;

#[cfg(target_os = "linux")]
use super::evidence::Cause;

/// 记录落点由目标身份给出（`<image>.diskedit/record` 或
/// `<state_root>/recovery/<identity-hash>/record`）；这里只把它拆成锚点目录与文件名
fn companion_parts(record: &Path) -> Result<(PathBuf, PathBuf), RecordJournalError> {
    let name = record
        .file_name()
        .map(PathBuf::from)
        .ok_or_else(|| RecordJournalError::Unsupported(format!("{} has no file name", record.display())))?;
    let dir = match record.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
        _ => PathBuf::from("."),
    };
    Ok((dir, name))
}

/// 同目录临时名：`.<正式名>.<pid>.<128 位随机>.tmp`。改名前后都在
/// 同一目录，改名是单目录操作
#[cfg(target_os = "linux")]
fn temp_name(name: &Path) -> Result<PathBuf, RecordJournalError> {
    let mut random = [0u8; 16];
    getrandom::fill(&mut random)
        .map_err(|e| RecordJournalError::Unsupported(format!("no random source: {e}")))?;
    let hex = crate::support::hex_lower(&random);
    let mut temp = std::ffi::OsString::from(".");
    temp.push(name.as_os_str());
    temp.push(format!(".{}.{hex}.tmp", std::process::id()));
    Ok(PathBuf::from(temp))
}

/// 清掉本目标落点的残留临时文件（第 26、41 条）：只认
/// `.<正式名>.<pid>.<32 位 hex>.tmp`，范围限于该落点的父目录。
///
/// 目录按句柄打开（末级不跟随符号链接），逐条 `fstatat(AT_SYMLINK_NOFOLLOW)`
/// 确认是常规文件后才 `unlinkat` 删除：不递归、不删目录、不碰正式文件与锁文件
#[cfg(target_os = "linux")]
pub(crate) fn sweep_record_temps(record_path: &Path) {
    let Ok((dir, name)) = companion_parts(record_path) else { return };
    let Ok(dir_c) = std::ffi::CString::new(dir.as_os_str().as_bytes()) else { return };
    let raw = unsafe {
        libc::openat(
            libc::AT_FDCWD,
            dir_c.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if raw < 0 {
        return;
    }
    let stream = unsafe { libc::fdopendir(raw) };
    if stream.is_null() {
        unsafe { libc::close(raw) };
        return;
    }
    let stream = DirStream(stream);
    let prefix = format!(".{}.", name.to_string_lossy());
    while let Some(file_name) = stream.next_name() {
        let Some(text) = file_name.to_str() else { continue };
        if !is_stale_temp_name(text, &prefix) {
            continue;
        }
        let Ok(entry) = std::ffi::CString::new(file_name.as_bytes()) else { continue };
        let fd = unsafe { libc::dirfd(stream.0) };
        let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
        let rc = unsafe {
            libc::fstatat(fd, entry.as_ptr(), stat.as_mut_ptr(), libc::AT_SYMLINK_NOFOLLOW)
        };
        if rc != 0 {
            continue;
        }
        let stat = unsafe { stat.assume_init() };
        if stat.st_mode & libc::S_IFMT != libc::S_IFREG {
            continue;
        }
        if unsafe { libc::unlinkat(fd, entry.as_ptr(), 0) } != 0 {
            let err = std::io::Error::last_os_error();
            eprintln!(
                "warning: cannot remove stale temporary {}: {err}",
                dir.join(&file_name).display()
            );
        }
    }
}

/// 目录句柄包装：`closedir` 同时关闭底层 fd
#[cfg(target_os = "linux")]
struct DirStream(*mut libc::DIR);

#[cfg(target_os = "linux")]
impl DirStream {
    fn next_name(&self) -> Option<std::ffi::OsString> {
        loop {
            let entry = unsafe { libc::readdir(self.0) };
            if entry.is_null() {
                return None;
            }
            let name = unsafe { std::ffi::CStr::from_ptr((*entry).d_name.as_ptr()) };
            let bytes = name.to_bytes();
            if bytes == b"." || bytes == b".." {
                continue;
            }
            return Some(std::ffi::OsStr::from_bytes(bytes).to_os_string());
        }
    }
}

#[cfg(target_os = "linux")]
impl Drop for DirStream {
    fn drop(&mut self) {
        unsafe { libc::closedir(self.0) };
    }
}

/// 固定临时名：`<前缀><pid>.<32 位 hex>.tmp`
#[cfg(target_os = "linux")]
fn is_stale_temp_name(text: &str, prefix: &str) -> bool {
    let Some(rest) = text.strip_prefix(prefix) else { return false };
    let Some(middle) = rest.strip_suffix(".tmp") else { return false };
    let mut parts = middle.split('.');
    let (Some(pid), Some(random), None) = (parts.next(), parts.next(), parts.next()) else {
        return false;
    };
    pid.parse::<u32>().is_ok() && random.len() == 32 && random.bytes().all(|b| b.is_ascii_hexdigit())
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn sweep_record_temps(_record_path: &Path) {}

/// 记录读写失败
#[derive(Debug)]
pub(crate) enum RecordJournalError {
    Io(std::io::Error),
    Unsupported(String),
    /// 删除前回读与待删记录不符：记录保留。已写目标字节时落 5，未写时落 3
    ReadbackMismatch(String),
}

impl std::fmt::Display for RecordJournalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "{e}"),
            Self::Unsupported(message) | Self::ReadbackMismatch(message) => f.write_str(message),
        }
    }
}

impl From<std::io::Error> for RecordJournalError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

#[cfg(target_os = "linux")]
fn path_error(e: PathError) -> RecordJournalError {
    RecordJournalError::Unsupported(format!("the record path is refused: {e:?}"))
}

#[cfg(target_os = "linux")]
fn evidence_io(e: EvidenceIo) -> RecordJournalError {
    match e {
        EvidenceIo::Io(e) => RecordJournalError::Io(e),
        EvidenceIo::Unsupported(message) => RecordJournalError::Unsupported(message),
    }
}

/// 闸口看到的记录现场：只回答"落点上是什么"，不做身份比较
#[derive(Clone, Debug)]
// 记录是固定布局的落盘形态（488 字节）；装箱只把同一份字节搬到堆上，
// 而每个匹配臂都要多一次解引用。差异来自"这就是那份字节"，不是设计遗漏
#[allow(clippy::large_enum_variant)]
pub(crate) enum CompanionProbe {
    /// 落点没有条目
    Absent,
    /// 读得出完整记录（身份由调用方按当前目标再比）
    Decoded(Record),
    /// 本工具的记录，但撕裂 / CRC 不符
    Damaged(String),
    /// 布局不属于本构建
    Foreign,
    /// 落点存在但不是普通文件（符号链接 / 目录 / 其他条目）
    NotOurs(String),
    /// 读不出来：权限、I/O 或平台没有读取原语
    Unreadable(String),
}

/// 探测记录落点
#[cfg(target_os = "linux")]
pub(crate) fn probe_companion(record: &Path) -> CompanionProbe {
    let (dir, name) = match companion_parts(record) {
        Ok(parts) => parts,
        Err(e) => return CompanionProbe::Unreadable(e.to_string()),
    };
    let relative = match RelativePath::try_new(name) {
        Ok(relative) => relative,
        Err(e) => return CompanionProbe::Unreadable(format!("the record path is refused: {e:?}")),
    };
    let anchor = match DirFd::open_anchor(&dir) {
        Ok(anchor) => anchor,
        Err(e) => return CompanionProbe::Unreadable(format!("cannot open {}: {e}", dir.display())),
    };
    let store = HostEvidenceStore;
    let file = match store.open_at(&anchor, &relative) {
        Ok(file) => file,
        Err(PathError::NotFound) => return CompanionProbe::Absent,
        Err(PathError::Symlink) => return CompanionProbe::NotOurs("a symbolic link".to_string()),
        Err(PathError::IsDirectory) => return CompanionProbe::NotOurs("a directory".to_string()),
        Err(PathError::NotDirectory | PathError::NotAFile) => {
            return CompanionProbe::NotOurs("not a regular file".to_string())
        }
        Err(e) => return CompanionProbe::Unreadable(format!("the record path is refused: {e:?}")),
    };
    match store.read(&file) {
        Ok(bytes) => classify_bytes(&bytes),
        Err(EvidenceIo::Io(e)) => CompanionProbe::Unreadable(e.to_string()),
        Err(EvidenceIo::Unsupported(message)) => CompanionProbe::Unreadable(message),
    }
}

/// 非 Linux 预留端口：没有目录相对原语。条目存在与否只按路径元数据回答，内容一律不读
#[cfg(not(target_os = "linux"))]
pub(crate) fn probe_companion(record: &Path) -> CompanionProbe {
    match std::fs::symlink_metadata(record) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => CompanionProbe::Absent,
        Err(e) => CompanionProbe::Unreadable(format!("cannot inspect {}: {e}", record.display())),
        Ok(meta) if meta.is_file() => CompanionProbe::Unreadable(
            "this platform has no directory-relative read primitive; the record cannot be read here"
                .to_string(),
        ),
        Ok(meta) if meta.file_type().is_symlink() => CompanionProbe::NotOurs("a symbolic link".to_string()),
        Ok(meta) if meta.is_dir() => CompanionProbe::NotOurs("a directory".to_string()),
        Ok(_) => CompanionProbe::NotOurs("not a regular file".to_string()),
    }
}

fn classify_bytes(bytes: &[u8]) -> CompanionProbe {
    match super::record::decode(bytes) {
        Ok(record) => CompanionProbe::Decoded(record),
        Err(super::record::DecodeFailure::Format { .. }) => CompanionProbe::Foreign,
        Err(super::record::DecodeFailure::Incomplete { observed, expected }) => CompanionProbe::Damaged(format!(
            "the record is incomplete ({observed} of {expected} byte(s))"
        )),
        Err(super::record::DecodeFailure::Crc { region }) => {
            CompanionProbe::Damaged(format!("the record's {region:?} checksum does not match"))
        }
    }
}

/// 读取记录并按当前目标分类。只有 `Recognized` 才携带记录
#[cfg(target_os = "linux")]
pub(crate) fn read_companion(record: &Path, current: &SceneIdentity) -> Result<Evidence, RecordJournalError> {
    let (dir, name) = companion_parts(record)?;
    let relative = RelativePath::try_new(name).map_err(path_error)?;
    let anchor = DirFd::open_anchor(&dir)?;
    let store = HostEvidenceStore;
    match store.open_at(&anchor, &relative) {
        Ok(file) => match store.read(&file) {
            Ok(bytes) => Ok(Evidence::classify(&bytes, current)),
            Err(EvidenceIo::Io(e)) => Ok(Evidence::unreadable(Cause::Io(e.to_string()))),
            Err(EvidenceIo::Unsupported(message)) => Ok(Evidence::unreadable(Cause::Unsupported(message))),
        },
        Err(e) => Ok(Evidence::from_path_error(e)),
    }
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn read_companion(_record: &Path, _current: &SceneIdentity) -> Result<Evidence, RecordJournalError> {
    Err(RecordJournalError::Unsupported(
        "this platform has no directory-relative read primitive; recovery records are refuse-only".to_string(),
    ))
}

/// 整份载荷已无法验证时，取回仍可逐条验证的完整前缀（见 [`super::record::decode_prefix`]）。
/// 读不到文件本身按 I/O 错误上抛：那不是"没有前缀"，不能当成可拒绝的请求
#[cfg(target_os = "linux")]
pub(crate) fn read_companion_prefix(
    record: &Path,
) -> Result<Option<super::record::Prefix>, RecordJournalError> {
    let (dir, name) = companion_parts(record)?;
    let relative = RelativePath::try_new(name).map_err(path_error)?;
    let anchor = DirFd::open_anchor(&dir)?;
    let store = HostEvidenceStore;
    let file = match store.open_at(&anchor, &relative) {
        Ok(file) => file,
        Err(PathError::NotFound) => return Ok(None),
        Err(e) => {
            return Err(RecordJournalError::Unsupported(format!(
                "cannot open the recovery record at {}: {e:?}",
                record.display()
            )))
        }
    };
    match store.read(&file) {
        Ok(bytes) => Ok(super::record::decode_prefix(&bytes)),
        Err(EvidenceIo::Io(e)) => Err(RecordJournalError::Io(e)),
        Err(EvidenceIo::Unsupported(message)) => {
            Err(RecordJournalError::Unsupported(message))
        }
    }
}

/// 把已回滚的记录标记为已提交并删除。
///
/// 回滚字节先写回目标，再走这里：先把 `Committed` 落盘，再经 [`RecoveryEngine`] 的
/// 提交与清理许可删除。两次之间中断只会留下"已提交、待清理"的记录，再跑一次 `undo`
/// 即收尾
#[cfg(target_os = "linux")]
pub(crate) fn settle_companion(record_path: &Path, record: &Record, persist_commit: bool) -> Result<(), RecordJournalError> {
    let (dir, name) = companion_parts(record_path)?;
    let relative = RelativePath::try_new(name.clone()).map_err(path_error)?;
    let temp = RelativePath::try_new(temp_name(&name)?).map_err(path_error)?;
    let anchor = DirFd::open_anchor(&dir)?;
    let store = HostEvidenceStore;
    let mut file = store
        .open_at(&anchor, &relative)
        .map_err(|e| RecordJournalError::Unsupported(format!("the record path is refused: {e:?}")))?;
    // 删除前复核盘上事实：传入的 record 是调用方此前读出的快照，从读到删之间盘上
    // 记录可能已被改写，persist_commit=true 更会把快照整份替换回盘上。与盘上当前
    // 字节整份比对一致才动手，不一致则不动盘、保留记录——
    // 「读回不符则不删」的落点
    let current_bytes = store.read(&file).map_err(evidence_io)?;
    let current = crate::recovery::record::decode(&current_bytes).map_err(|why| {
        RecordJournalError::ReadbackMismatch(format!("the record on disk no longer decodes: {why:?}"))
    })?;
    if current != *record {
        return Err(RecordJournalError::ReadbackMismatch(
            "the record on disk differs from the snapshot this call was given; nothing was written or removed"
                .to_string(),
        ));
    }
    let mut settled = record.clone();
    settled.commit = CommitState::Committed;
    if persist_commit {
        let bytes = settled.encode().map_err(RecordJournalError::Io)?;
        store
            .replace_atomic(&anchor, &file, &temp, &bytes)
            .map_err(evidence_io)?;
        // 整份替换把名字指向了新 inode，旧句柄此后读回的是被覆盖掉的那份旧记录，
        // 而删除按父目录 + 文件名 unlink。删除前回读必须验的是即将被删的这份，
        // 所以替换之后重新解析路径；重开失败就不删，保留记录
        file = store.open_at(&anchor, &relative).map_err(|e| {
            RecordJournalError::Unsupported(format!(
                "cannot reopen the record after replacing it; the record is kept: {e:?}"
            ))
        })?;
    }
    let outcome = RecoveryEngine::commit(Evidence::Recognized(settled), false)
        .map_err(|refusal| RecordJournalError::Unsupported(format!("the record cannot be committed: {refusal:?}")))?;
    let (committed, permit, withheld) = outcome.into_parts();
    let permit = permit.ok_or_else(|| {
        RecordJournalError::Unsupported(format!("cleanup is withheld for this record: {withheld:?}"))
    })?;
    RecoveryEngine::cleanup(&store, &anchor, file, &committed, permit).map_err(|e| match e {
        CleanupError::Io(e) => evidence_io(e),
        CleanupError::ReadbackMismatch(why) => RecordJournalError::ReadbackMismatch(why),
    })?;
    Ok(())
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn settle_companion(
    _record_path: &Path,
    _record: &Record,
    _persist_commit: bool,
) -> Result<(), RecordJournalError> {
    Err(RecordJournalError::Unsupported(
        "this platform has no directory-relative primitive; the record cannot be removed here".to_string(),
    ))
}

#[cfg(target_os = "linux")]
pub(crate) struct RecordJournal {
    anchor: DirFd,
    relative: RelativePath,
    temp: RelativePath,
    file: Option<EvidenceFile>,
    record: Record,
    sector_size: u64,
    last_error: Option<String>,
    /// 目标文件的所有者；镜像侧记录必须与目标同主
    owner: Option<(u32, u32)>,
    /// persist 的整份替换/创建已成功但句柄重开失败：盘上记录还在，本进程却再无
    /// 有效句柄。commit_and_discard 见到它就拒绝当「无事可做」处理
    handle_lost: bool,
}

/// [`RecordJournal::persist`] 的失败分类。整份替换或首次创建成功之后，盘上事实
/// 已经是新记录：此时回滚内存会让「记录内容、盘面事实、报错文案」三者互相矛盾
/// ——调用方中止后盘上是新态，下一次命令读到的是新态，而回滚后的内存与本次报错
/// 描述的却是旧态。`Written` 分支要求调用方保持内存为新态、只中止。
#[cfg(target_os = "linux")]
pub(crate) enum PersistFailure {
    /// 盘上未变：内存回滚是安全的
    NotWritten(std::io::Error),
    /// 盘上已是新字节：内存不得回滚
    Written(std::io::Error),
}

#[cfg(target_os = "linux")]
impl PersistFailure {
    fn written(&self) -> bool {
        matches!(self, Self::Written(_))
    }

    fn into_io(self) -> std::io::Error {
        match self {
            Self::NotWritten(e) | Self::Written(e) => e,
        }
    }
}

#[cfg(target_os = "linux")]
impl std::fmt::Display for PersistFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotWritten(e) => write!(f, "{e}"),
            Self::Written(e) => write!(
                f,
                "{e}; the record was already written to disk, so the on-disk record is the new one"
            ),
        }
    }
}

/// 搬移双游标只许前进：倒退意味着有区间被跳过；越过条目数不是合法执行能产出的进度
#[cfg(target_os = "linux")]
fn ensure_reloc_forward(
    reloc: &Relocation,
    entry_index: u32,
    chunks_in_entry: u64,
) -> std::io::Result<()> {
    let (was_index, was_chunks) = (reloc.entry_index, reloc.chunks_in_entry);
    if entry_index < was_index || (entry_index == was_index && chunks_in_entry < was_chunks) {
        return Err(std::io::Error::other(format!(
            "the relocation cursor moved backwards ({was_index} / {was_chunks}) → ({entry_index} / {chunks_in_entry})"
        )));
    }
    if entry_index as usize > reloc.entries.len() {
        return Err(std::io::Error::other(
            "the relocation cursor points past the plan",
        ));
    }
    Ok(())
}

/// 全局进度只许前进：`transfer_cursor` 是跨条目的累计值，无条件单调；
/// `completed_steps` 自身也不得回退
#[cfg(target_os = "linux")]
fn ensure_progress_forward(
    was: Progress,
    completed_steps: u32,
    transfer_cursor: u64,
) -> std::io::Result<()> {
    if completed_steps < was.completed_steps {
        return Err(std::io::Error::other(format!(
            "the recovery record's progress moved backwards ({} / {}) → ({completed_steps} / {transfer_cursor})",
            was.completed_steps, was.transfer_cursor
        )));
    }
    if transfer_cursor < was.transfer_cursor {
        return Err(std::io::Error::other(format!(
            "the recovery record's transfer cursor moved backwards ({}) → ({transfer_cursor})",
            was.transfer_cursor
        )));
    }
    Ok(())
}

#[cfg(target_os = "linux")]
impl RecordJournal {
    /// 确定落点与记录头；此时**不产生文件**，记录在第一次 [`Self::record_span`] 时创建。
    /// 于是"在写盘前就被拒的命令"不会留下空记录占住落点
    pub(crate) fn create_image(
        record_path: &Path,
        current: SceneIdentity,
        op: OpKind,
        sector_size: u64,
        disk_guid: DiskGuidState,
        owner: Option<(u32, u32)>,
    ) -> Result<Self, RecordJournalError> {
        let (dir, name) = companion_parts(record_path)?;
        let relative = RelativePath::try_new(name.clone()).map_err(path_error)?;
        let temp = RelativePath::try_new(temp_name(&name)?).map_err(path_error)?;
        let anchor = DirFd::open_anchor(&dir)?;
        let mut tx = [0u8; 16];
        getrandom::fill(&mut tx).map_err(|e| RecordJournalError::Unsupported(format!("no random source: {e}")))?;
        let mut plan = [0u8; 16];
        getrandom::fill(&mut plan).map_err(|e| RecordJournalError::Unsupported(format!("no random source: {e}")))?;
        let mut record = Record::new(
            TxId::from_bytes(tx),
            current,
            PlanId::from_bytes(plan),
            PlanVersion::new(0),
            op,
            // 表类写命令没有持久计划，摘要为零值；回滚不读它
            PlanDigest::from_bytes([0u8; 32]),
            disk_guid,
        );
        record.action = RecoveryAction::Rollback(0);
        Ok(Self { anchor, relative, temp, file: None, record, sector_size, last_error: None, owner, handle_lost: false })
    }

    /// 打开既有记录以续跑。返回的 [`Record`] 是只读快照，写路径继续走 `self`。
    ///
    /// 只有"本工具写下、且属于当前目标"的完整记录才能续跑；其余形态一律拒绝——
    /// 调用方在此之前已用 active 枚举判过"有没有现场"，这里判的是"那份现场能不能用"
    pub(crate) fn open_image(
        record_path: &Path,
        current: SceneIdentity,
        sector_size: u64,
        owner: Option<(u32, u32)>,
    ) -> Result<(Self, Record), RecordJournalError> {
        let (dir, name) = companion_parts(record_path)?;
        let relative = RelativePath::try_new(name.clone()).map_err(path_error)?;
        let temp = RelativePath::try_new(temp_name(&name)?).map_err(path_error)?;
        let anchor = DirFd::open_anchor(&dir)?;
        let store = HostEvidenceStore;
        let file = store.open_at(&anchor, &relative).map_err(|e| {
            RecordJournalError::Unsupported(format!(
                "cannot open the recovery record at {}: {e:?}",
                record_path.display()
            ))
        })?;
        let bytes = match store.read(&file) {
            Ok(bytes) => bytes,
            Err(EvidenceIo::Io(e)) => return Err(RecordJournalError::Io(e)),
            Err(EvidenceIo::Unsupported(message)) => return Err(RecordJournalError::Unsupported(message)),
        };
        let record = match Evidence::classify(&bytes, &current) {
            Evidence::Recognized(record) => record,
            _ => {
                return Err(RecordJournalError::Unsupported(format!(
                    "the record at {} is not a complete record of this target, so it cannot be resumed",
                    record_path.display()
                )))
            }
        };
        let journal = Self {
            anchor,
            relative,
            temp,
            file: Some(file),
            record: record.clone(),
            sector_size,
            last_error: None,
            owner,
            handle_lost: false,
        };
        Ok((journal, record))
    }

    /// 绑定本次请求的摘要。新记录只写一次；已有摘要必须与本次请求一致——
    /// 换了参数（含 `--chunk`，它决定游标到字节的映射）的请求不能续跑别人的游标
    pub(crate) fn bind_plan(&mut self, digest: PlanDigest) -> Result<(), String> {
        if self.record.plan_digest == digest {
            return Ok(());
        }
        if self.record.plan_digest != PlanDigest::from_bytes([0u8; 32]) {
            return Err(format!(
                "the existing record belongs to a different `{}` request — its identity digest differs \
                 (the partition, size, or options changed)",
                self.record.op.command_name()
            ));
        }
        self.record.plan_digest = digest;
        Ok(())
    }

    pub(crate) fn op(&self) -> OpKind {
        self.record.op
    }

    /// 记录里绑定的请求摘要；尚未绑定（还没走到第一次写）时为零值
    pub(crate) fn plan_digest(&self) -> PlanDigest {
        self.record.plan_digest
    }

    /// 绑定 `copy` 的源区间与规划期摘要。与 [`Self::bind_plan`] 同形：只改内存，随下一次
    /// 落盘进入记录；已绑定的值必须逐字段相等——源换了的请求不能接管这份记录。落盘前
    /// 拒绝不会留下记录文件，`copy` 的只读判定因此仍可自由失败
    pub(crate) fn bind_source(&mut self, source: CopySource) -> Result<(), String> {
        if self.record.op != OpKind::Copy {
            return Err("only a `copy` record carries a source range".to_string());
        }
        if let Some(was) = self.record.source {
            if was == source {
                return Ok(());
            }
            return Err(
                "the existing record was written for a different source range (partition, extent, \
                 sector size or digest)"
                    .to_string(),
            );
        }
        self.record.source = Some(source);
        Ok(())
    }

    /// 记录里登记的 `copy` 源区间与摘要；非 `copy` 记录为 `None`。搬移中用它核对所读
    /// 字节，续跑与 `undo` 用它核对盘上现状
    pub(crate) fn source(&self) -> Option<CopySource> {
        self.record.source
    }

    pub(crate) fn progress(&self) -> Progress {
        self.record.progress
    }

    pub(crate) fn old_range(&self) -> Option<(u64, u64)> {
        self.record.old_range
    }

    /// 写下分区层提交前的原区间并落盘。新记录只写一次；已有值不因续跑时的
    /// 盘上几何（表可能已提交）而改写。`resize`、`resize-part` 与 `move` 会改动分区落位，
    /// 需要这份原区间；其余操作保持为空
    pub(crate) fn bind_old_range(&mut self, range: (u64, u64)) -> std::io::Result<()> {
        if self.record.old_range.is_some()
            || !matches!(self.record.op, OpKind::Resize | OpKind::ResizePart | OpKind::Move)
        {
            return Ok(());
        }
        self.record.old_range = Some(range);
        if let Err(e) = self.persist() {
            if !e.written() {
                self.record.old_range = None;
            }
            self.last_error = Some(e.to_string());
            return Err(e.into_io());
        }
        Ok(())
    }

    pub(crate) fn lvm_post(&self) -> Option<LvmPost> {
        self.record.lvm_post
    }

    /// 已落盘的搬移状态。`None` = 这不是搬移作业（或计划尚未绑定）
    pub(crate) fn relocation(&self) -> Option<&Relocation> {
        self.record.relocation.as_ref()
    }

    /// 绑定有序搬移计划与 LVM 收尾基线并一次落盘。新记录只写一次；已有计划必须逐字段
    /// 相等——换了分区、起点、长度、位移或语义的请求不能续跑别人的游标。基线必须与
    /// 计划同一次写入：分开写时中间崩溃会让续跑看到"有计划、无基线"，按当前盘面重算
    /// 增量必为 0，LV 收尾会被静默跳过。`ResizePart` 的起点变更搬移与 move 同构，
    /// 也携带条目表（进度恢复、续跑校验共用同一套机制）
    pub(crate) fn bind_relocation(
        &mut self,
        reloc: Relocation,
        lvm_post: Option<LvmPost>,
    ) -> std::io::Result<()> {
        if !matches!(self.record.op, OpKind::Resize | OpKind::ResizePart | OpKind::Move) {
            return Err(std::io::Error::other(
                "only a `move`, `resize` or `resize-part` record carries a relocation plan",
            ));
        }
        if let Some(was) = &self.record.relocation {
            if *was == reloc {
                return Ok(());
            }
            return Err(std::io::Error::other(
                "the existing record was written for a different relocation plan",
            ));
        }
        self.record.relocation = Some(reloc);
        let lvm_before = self.record.lvm_post;
        if lvm_post.is_some() {
            // 上一次作业已经写下基线时沿用旧值：续跑不重新选定 LV
            self.record.lvm_post = self.record.lvm_post.or(lvm_post);
        }
        if let Err(e) = self.persist() {
            if !e.written() {
                self.record.relocation = None;
                self.record.lvm_post = lvm_before;
            }
            self.last_error = Some(e.to_string());
            return Err(e.into_io());
        }
        Ok(())
    }

    /// 同时推进搬移游标与全局进度并单次落盘。自重叠搬移每拷一块都要写这两处，
    /// 分两次 persist 会在中间崩溃时留下"块进度与游标不一致"的记录。缩容状态不属于
    /// 检查点：落盘时沿用记录里的值
    pub(crate) fn record_transfer_checkpoint(
        &mut self,
        reloc: Option<(u32, u64)>,
        completed_steps: u32,
        transfer_cursor: u64,
    ) -> std::io::Result<()> {
        let was_progress = self.record.progress;
        let was_reloc = self
            .record
            .relocation
            .as_ref()
            .map(|r| (r.entry_index, r.chunks_in_entry));
        if let Some((entry_index, chunks_in_entry)) = reloc {
            let Some(current) = self.record.relocation.as_ref() else {
                return Err(std::io::Error::other(
                    "the recovery record carries no relocation plan",
                ));
            };
            ensure_reloc_forward(current, entry_index, chunks_in_entry)?;
        }
        ensure_progress_forward(was_progress, completed_steps, transfer_cursor)?;
        if let Some((entry_index, chunks_in_entry)) = reloc {
            let current = self.record.relocation.as_mut().expect("checked above");
            current.entry_index = entry_index;
            current.chunks_in_entry = chunks_in_entry;
        }
        self.record.progress = Progress { completed_steps, transfer_cursor, fs_phase: was_progress.fs_phase };
        if let Err(e) = self.persist() {
            if !e.written() {
                if let Some((entry_index, chunks_in_entry)) = was_reloc {
                    let current = self.record.relocation.as_mut().expect("checked above");
                    current.entry_index = entry_index;
                    current.chunks_in_entry = chunks_in_entry;
                }
                self.record.progress = was_progress;
            }
            self.last_error = Some(e.to_string());
            return Err(e.into_io());
        }
        Ok(())
    }

    /// 写下分区层提交后的 LVM 收尾基线并落盘。只有 `resize` 的记录带这一步；
    /// 一旦写下就不再接受另一个基线，换了作业的请求会被拒绝
    pub(crate) fn record_lvm_post(&mut self, post: LvmPost) -> std::io::Result<()> {
        if self.record.op != OpKind::Resize {
            return Err(std::io::Error::other(
                "only a `resize` record carries the LVM finish baseline",
            ));
        }
        if let Some(was) = self.record.lvm_post {
            if was == post {
                return Ok(());
            }
            return Err(std::io::Error::other(
                "the existing record was written for a different LVM finish step",
            ));
        }
        self.record.lvm_post = Some(post);
        if let Err(e) = self.persist() {
            if !e.written() {
                self.record.lvm_post = None;
            }
            self.last_error = Some(e.to_string());
            return Err(e.into_io());
        }
        Ok(())
    }

    /// 推进持久进度并落盘。只允许前进（stage 只前进或不变）——
    /// 倒退只会来自两处游标口径分叉，静默接受就等于让搬移循环跳过已覆盖的区间
    pub(crate) fn record_progress(
        &mut self,
        completed_steps: u32,
        transfer_cursor: u64,
        fs_phase: u8,
    ) -> std::io::Result<()> {
        let was = self.record.progress;
        ensure_progress_forward(was, completed_steps, transfer_cursor)?;
        self.record.progress = Progress { completed_steps, transfer_cursor, fs_phase };
        if let Err(e) = self.persist() {
            if !e.written() {
                self.record.progress = was;
            }
            self.last_error = Some(e.to_string());
            return Err(e.into_io());
        }
        Ok(())
    }

    /// 记下一段被覆盖的扇区原文并持久化。调用方随后才写目标字节。同一区间已登记时
    /// 不重复登记：`original` 保留首次见到的那份（undo 要还原的就是这些字节），
    /// `after` 取最后一次写入的写后内容（undo 用它核对盘面是否仍停在记录写后的样子）
    pub(crate) fn record_span(
        &mut self,
        first_lba: u64,
        original: Vec<u8>,
        after: Vec<u8>,
    ) -> std::io::Result<()> {
        self.record_spans(vec![(first_lba, original, after)])
    }

    /// 批量登记若干段并单次落盘：全部成功之后调用方才允许第一次目标写入。
    /// 区间判据与 [`Self::record_span`] 相同，只比 `(first_lba, sectors)`
    pub(crate) fn record_spans(
        &mut self,
        spans: Vec<(u64, Vec<u8>, Vec<u8>)>,
    ) -> std::io::Result<()> {
        let sector_size = u32::try_from(self.sector_size).map_err(|_| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "the sector size does not fit the record")
        })?;
        let base = self.record.restore.len();
        let mut replaced: Vec<(usize, Vec<u8>)> = Vec::new();
        let mut added = false;
        for (first_lba, original, after) in spans {
            if original.is_empty() {
                continue;
            }
            let sectors = u32::try_from((original.len() as u64).div_ceil(self.sector_size)).map_err(|_| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "the write spans too many sectors for the record",
                )
            })?;
            if after.len() != original.len() {
                self.record.restore.truncate(base);
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "the recorded pre-image and post-image spans differ in length",
                ));
            }
            if let Some(i) = self
                .record
                .restore
                .iter()
                .position(|e| e.first_lba == first_lba && e.sectors == sectors)
            {
                if self.record.restore[i].after != after {
                    let old = std::mem::replace(&mut self.record.restore[i].after, after);
                    replaced.push((i, old));
                }
                continue;
            }
            let entry = RestoreEntry { sector_size, first_lba, sectors, original, after };
            debug_assert!(entry.is_well_formed());
            self.record.restore.push(entry);
            added = true;
        }
        if !added && replaced.is_empty() {
            return Ok(());
        }
        match self.persist() {
            Ok(()) => Ok(()),
            Err(e) => {
                if !e.written() {
                    self.record.restore.truncate(base);
                    for (i, old) in replaced {
                        self.record.restore[i].after = old;
                    }
                }
                self.last_error = Some(e.to_string());
                Err(e.into_io())
            }
        }
    }

    /// 记录是否已落盘。为 false 时本次事务没有向目标写过任何字节
    pub(crate) fn is_persisted(&self) -> bool {
        self.file.is_some()
    }

    /// 最近一次持久化失败的说明
    pub(crate) fn last_error(&self) -> Option<&str> {
        self.last_error.as_deref()
    }

    /// 不可逆写入的屏障：记录一旦越过这一点，`undo` 只报告不回写。
    ///
    /// 记录尚未落盘时这里把它创建出来：第一次目标写入之前记录必须已经持久化，
    /// 而数据搬移类命令的第一次写入正是 `write_data_at`（它先调本函数再写字节）。
    /// 表区 preimage 要等表步骤才追加，因此此刻的记录只有屏障位、没有区间
    pub(crate) fn barrier(&mut self) -> std::io::Result<()> {
        if self.record.restore_blocked {
            return Ok(());
        }
        self.record.restore_blocked = true;
        if let Err(e) = self.persist() {
            if !e.written() {
                self.record.restore_blocked = false;
            }
            self.last_error = Some(e.to_string());
            return Err(e.into_io());
        }
        Ok(())
    }

    /// 越过 durable boundary：把 `Committed` 落盘
    pub(crate) fn commit(&mut self) -> std::io::Result<()> {
        if self.file.is_none() || self.record.commit == CommitState::Committed {
            return Ok(());
        }
        self.record.commit = CommitState::Committed;
        if let Err(e) = self.persist() {
            if !e.written() {
                self.record.commit = CommitState::Prepared;
            }
            self.last_error = Some(e.to_string());
            return Err(e.into_io());
        }
        crate::movepart::fault_record_committed();
        Ok(())
    }

    /// 提交并删除记录。没有任何写入时不做任何 I/O
    pub(crate) fn commit_and_discard(mut self) -> Result<(), RecordJournalError> {
        self.commit()?;
        let Some(file) = self.file.take() else {
            // file 为 None 的两种情形必须区分：记录从未创建（无盘上事实，无事可做）
            // 与 persist 替换成功后句柄重开失败（盘上记录还在）。后者当「无事可做」
            // 返回 Ok 会让调用方以为收尾成功，而记录仍挡在落点上
            if self.handle_lost {
                return Err(RecordJournalError::Unsupported(
                    "the recovery record was written to disk but its handle could not be reopened, \
                     so it cannot be verified before deletion; the record is left in place"
                        .to_string(),
                ));
            }
            return Ok(());
        };
        let outcome = RecoveryEngine::commit(Evidence::Recognized(self.record.clone()), false)
            .map_err(|refusal| RecordJournalError::Unsupported(format!("the record cannot be committed: {refusal:?}")))?;
        let (committed, permit, withheld) = outcome.into_parts();
        let permit = permit.ok_or_else(|| {
            RecordJournalError::Unsupported(format!("cleanup is withheld for this record: {withheld:?}"))
        })?;
        let store = HostEvidenceStore;
        match RecoveryEngine::cleanup(&store, &self.anchor, file, &committed, permit) {
            Ok(DirDurability::Persisted) => Ok(()),
            Err(CleanupError::Io(e)) => Err(evidence_io(e)),
            // 已写路径的清理侧拒绝：目标字节已落盘，调用方一律降级为 5
            Err(CleanupError::ReadbackMismatch(why)) => Err(RecordJournalError::ReadbackMismatch(why)),
        }
    }

    fn persist(&mut self) -> Result<(), PersistFailure> {
        self.record.plan_version = self
            .record
            .plan_version
            .next()
            .ok_or_else(|| {
                PersistFailure::NotWritten(std::io::Error::other(
                    "the recovery record's version counter overflowed",
                ))
            })?;
        let bytes = self
            .record
            .encode()
            .map_err(PersistFailure::NotWritten)?;
        if bytes.len() as u64 > super::record::MAX_RECORD_BYTES {
            return Err(PersistFailure::NotWritten(std::io::Error::other(
                "the recovery record reached its size bound; the write is refused",
            )));
        }
        let store = HostEvidenceStore;
        if let Some(file) = &self.file {
            if let Some(owner) = self.owner {
                let actual = crate::platform::evidence_store::file_owner(file).map_err(|e| {
                    PersistFailure::NotWritten(std::io::Error::other(format!(
                        "cannot read the record owner: {e:?}"
                    )))
                })?;
                if actual != owner {
                    return Err(PersistFailure::NotWritten(std::io::Error::new(
                        std::io::ErrorKind::PermissionDenied,
                        format!(
                            "{} is owned by uid {} gid {}, not by the target's owner uid {} gid {}",
                            self.relative.as_path().display(),
                            actual.0,
                            actual.1,
                            owner.0,
                            owner.1
                        ),
                    )));
                }
            }
            store
                .replace_atomic(&self.anchor, file, &self.temp, &bytes)
                .map_err(|e| PersistFailure::NotWritten(record_io(e)))?;
            // 与 settle_companion 同理：整份替换后旧句柄已不是名字指向的那份。
            // 每次 persist 都换上重新解析的句柄，后续 commit_and_discard 的
            // 删除前回读才能验到真正会被删掉的字节。重开失败时旧句柄必须丢弃
            // 而不是留着——按路径 unlink 删的是目录项指向的新文件，拿旧句柄
            // 回读会验到旧字节，验的是 A 删的是 B。此后失败归 Written：
            // 盘上已是新字节，调用方保持内存新态、只中止
            match store.open_at(&self.anchor, &self.relative) {
                Ok(fresh) => self.file = Some(fresh),
                Err(e) => {
                    self.file = None;
                    self.handle_lost = true;
                    return Err(PersistFailure::Written(std::io::Error::other(format!(
                        "cannot reopen {} after replacing it: {e:?}",
                        self.relative.as_path().display()
                    ))));
                }
            }
        } else {
            // 首次创建：落点已有条目时不覆盖，那是别人的记录
            match store.open_at(&self.anchor, &self.relative) {
                Ok(_) => {
                    return Err(PersistFailure::NotWritten(std::io::Error::new(
                        std::io::ErrorKind::AlreadyExists,
                        format!("{} already holds a recovery record", self.relative.as_path().display()),
                    )));
                }
                Err(PathError::NotFound) => {}
                Err(e) => {
                    return Err(PersistFailure::NotWritten(std::io::Error::other(format!(
                        "cannot resolve {}: {e:?}",
                        self.relative.as_path().display()
                    ))));
                }
            }
            store
                .create_atomic_owned(&self.anchor, &self.relative, &self.temp, &bytes, self.owner)
                .map_err(|e| PersistFailure::NotWritten(record_io(e)))?;
            match store.open_at(&self.anchor, &self.relative) {
                Ok(file) => self.file = Some(file),
                Err(e) => {
                    self.handle_lost = true;
                    return Err(PersistFailure::Written(std::io::Error::other(format!(
                        "cannot reopen {} after creating it: {e:?}",
                        self.relative.as_path().display()
                    ))));
                }
            }
        }
        Ok(())
    }
}

#[cfg(target_os = "linux")]
fn record_io(e: EvidenceIo) -> std::io::Error {
    match e {
        EvidenceIo::Io(e) => e,
        EvidenceIo::Unsupported(message) => std::io::Error::other(message),
    }
}

/// 非 Linux 预留端口：类型可构造性只为字段声明存在，任何使用都返回拒绝
#[cfg(not(target_os = "linux"))]
pub(crate) struct RecordJournal {
    _never: std::convert::Infallible,
}

#[cfg(not(target_os = "linux"))]
impl RecordJournal {
    /// 本平台是预留的 fail-closed 端口：记录无从落点，构造即拒绝
    pub(crate) fn create_image(
        _image: &Path,
        _current: SceneIdentity,
        _op: OpKind,
        _sector_size: u64,
        _disk_guid: DiskGuidState,
        _owner: Option<(u32, u32)>,
    ) -> Result<Self, RecordJournalError> {
        Err(RecordJournalError::Unsupported(
            "this platform has no directory-relative primitive; recovery records cannot be written here"
                .to_string(),
        ))
    }

    pub(crate) fn record_span(
        &mut self,
        _first_lba: u64,
        _original: Vec<u8>,
        _after: Vec<u8>,
    ) -> std::io::Result<()> {
        Err(std::io::Error::other("recovery records are not written on this platform"))
    }

    pub(crate) fn record_spans(
        &mut self,
        _spans: Vec<(u64, Vec<u8>, Vec<u8>)>,
    ) -> std::io::Result<()> {
        Err(std::io::Error::other("recovery records are not written on this platform"))
    }

    pub(crate) fn is_persisted(&self) -> bool {
        false
    }

    pub(crate) fn last_error(&self) -> Option<&str> {
        None
    }

    pub(crate) fn barrier(&mut self) -> std::io::Result<()> {
        Err(std::io::Error::other("recovery records are not written on this platform"))
    }

    pub(crate) fn commit(&mut self) -> std::io::Result<()> {
        Err(std::io::Error::other("recovery records are not written on this platform"))
    }

    pub(crate) fn commit_and_discard(self) -> Result<(), RecordJournalError> {
        Err(RecordJournalError::Unsupported(
            "recovery records are not written on this platform".to_string(),
        ))
    }

    /// 本平台没有可续跑的记录：构造永远失败，调用方只会看到这条拒绝
    pub(crate) fn open_image(
        _image: &Path,
        _current: SceneIdentity,
        _sector_size: u64,
        _owner: Option<(u32, u32)>,
    ) -> Result<(Self, crate::recovery::Record), RecordJournalError> {
        Err(RecordJournalError::Unsupported(
            "this platform has no directory-relative primitive; recovery records cannot be read here"
                .to_string(),
        ))
    }

    pub(crate) fn bind_plan(&mut self, _digest: crate::recovery::PlanDigest) -> Result<(), String> {
        Err("recovery records are not written on this platform".to_string())
    }

    pub(crate) fn bind_source(&mut self, _source: crate::recovery::CopySource) -> Result<(), String> {
        Err("recovery records are not written on this platform".to_string())
    }

    pub(crate) fn source(&self) -> Option<crate::recovery::CopySource> {
        match self._never {}
    }

    pub(crate) fn op(&self) -> OpKind {
        match self._never {}
    }

    pub(crate) fn plan_digest(&self) -> crate::recovery::PlanDigest {
        match self._never {}
    }

    pub(crate) fn progress(&self) -> crate::recovery::Progress {
        match self._never {}
    }

    pub(crate) fn old_range(&self) -> Option<(u64, u64)> {
        match self._never {}
    }

    pub(crate) fn bind_old_range(&mut self, _range: (u64, u64)) -> std::io::Result<()> {
        Err(std::io::Error::other("recovery records are not written on this platform"))
    }

    pub(crate) fn lvm_post(&self) -> Option<crate::recovery::LvmPost> {
        match self._never {}
    }

    pub(crate) fn relocation(&self) -> Option<&crate::recovery::Relocation> {
        match self._never {}
    }

    pub(crate) fn bind_relocation(
        &mut self,
        _reloc: crate::recovery::Relocation,
        _lvm_post: Option<crate::recovery::LvmPost>,
    ) -> std::io::Result<()> {
        Err(std::io::Error::other("recovery records are not written on this platform"))
    }

    pub(crate) fn record_transfer_checkpoint(
        &mut self,
        _reloc: Option<(u32, u64)>,
        _completed_steps: u32,
        _transfer_cursor: u64,
    ) -> std::io::Result<()> {
        Err(std::io::Error::other("recovery records are not written on this platform"))
    }

    pub(crate) fn record_lvm_post(
        &mut self,
        _post: crate::recovery::LvmPost,
    ) -> std::io::Result<()> {
        Err(std::io::Error::other("recovery records are not written on this platform"))
    }
    pub(crate) fn record_progress(
        &mut self,
        _completed_steps: u32,
        _transfer_cursor: u64,
        _fs_phase: u8,
    ) -> std::io::Result<()> {
        Err(std::io::Error::other("recovery records are not written on this platform"))
    }
}
