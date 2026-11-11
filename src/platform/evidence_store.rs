//! Linux 证据存储：从锚点逐级 `openat(O_NOFOLLOW)` 解析，原子替换走同目录改名，
//! 删除与写入后都对目录 FD 执行 `fsync`。
//!
//! 父目录链中的每一级都要求目录且拒绝符号链接；最终分量要求普通文件。
//! 删除与改名复用解析时持有的父目录 FD 与文件名，不再按路径二次解析。

use std::ffi::{CString, OsStr, OsString};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path};

use super::{
    DirDurability, DirFd, EvidenceFile, EvidenceIo, EvidenceStore, PathError, RelativePath,
    RemoveOutcome, sealed,
};

/// `open_at` 解析出来的文件句柄组
pub(crate) struct OpenedFile {
    pub(crate) parent: OwnedFd,
    pub(crate) name: OsString,
    pub(crate) fd: OwnedFd,
}

pub(crate) struct HostEvidenceStore;

impl sealed::Sealed for HostEvidenceStore {}

impl EvidenceStore for HostEvidenceStore {
    fn open_at(&self, anchor: &DirFd, relative: &RelativePath) -> Result<EvidenceFile, PathError> {
        let (parent, name) = open_parent(anchor, relative)?;
        let fd = open_file(&parent, &name)?;
        Ok(EvidenceFile::from_linux(relative.clone(), OpenedFile { parent, name, fd }))
    }

    fn read(&self, file: &EvidenceFile) -> Result<Vec<u8>, EvidenceIo> {
        // try_clone 是 dup：两个 fd 共享偏移，read_to_end 会把共享偏移推到 EOF，
        // 同一份句柄的第二次读（删除前回读）就从 EOF 拿到 0 字节。本文件的读取
        // 一律 pread：偏移无关，也不扰动共享偏移
        let fd = file.state().fd.as_raw_fd();
        let mut out = Vec::new();
        let mut off: libc::off_t = 0;
        loop {
            let mut buf = [0u8; 8192];
            let n = unsafe {
                libc::pread(
                    fd,
                    buf.as_mut_ptr().cast::<libc::c_void>(),
                    buf.len(),
                    off,
                )
            };
            if n < 0 {
                return Err(EvidenceIo::Io(io::Error::last_os_error()));
            }
            if n == 0 {
                return Ok(out);
            }
            out.extend_from_slice(&buf[..n as usize]);
            off += n as libc::off_t;
        }
    }

    fn replace_atomic(
        &self,
        _anchor: &DirFd,
        file: &EvidenceFile,
        temp: &RelativePath,
        bytes: &[u8],
    ) -> Result<DirDurability, EvidenceIo> {
        let state = file.state();
        let temp_name = sibling_name(file.relative(), temp)?;
        // 整份替换沿用原记录的所有者：临时文件由本进程创建，root 改写他人镜像时
        // 不能把记录变成 root 所有
        let owner = file_owner(file)?;
        let temp_fd = create_file(&state.parent, &temp_name, Some(owner))?;
        write_all(temp_fd.as_raw_fd(), bytes).map_err(EvidenceIo::Io)?;
        sync_fd(temp_fd.as_raw_fd()).map_err(EvidenceIo::Io)?;
        drop(temp_fd);

        rename_sibling(&state.parent, &temp_name, &state.name)?;
        sync_dir(&state.parent).map_err(|e| {
            EvidenceIo::RenamedNotDurable(io::Error::other(format!(
                "the record was renamed into place but its directory fsync failed: {e}"
            )))
        })?;
        Ok(DirDurability::Persisted)
    }

    fn remove(
        &self,
        _anchor: &DirFd,
        file: EvidenceFile,
        _committed: &crate::recovery::CommittedRecord,
        _permit: &crate::recovery::CleanupPermit,
    ) -> RemoveOutcome {
        let state = file.state();
        let name = match c_name(&state.name) {
            Ok(name) => name,
            Err(e) => return RemoveOutcome::NotRemoved(EvidenceIo::Io(e)),
        };
        let raw = unsafe { libc::unlinkat(state.parent.as_raw_fd(), name.as_ptr(), 0) };
        if raw != 0 {
            return RemoveOutcome::NotRemoved(EvidenceIo::Io(io::Error::last_os_error()));
        }
        match sync_dir(&state.parent) {
            Ok(()) => RemoveOutcome::RemovedAndDurable,
            Err(_) => RemoveOutcome::RemovedDurabilityUnconfirmed,
        }
    }
}

impl HostEvidenceStore {
    /// 首次创建记录，并让新文件的 uid / gid 等于目标文件的所有者。
    /// `owner` 为 `None` 表示落点本身要求 uid 0（HostRepository），由进程身份决定
    pub(crate) fn create_atomic_owned(
        &self,
        anchor: &DirFd,
        relative: &RelativePath,
        temp: &RelativePath,
        bytes: &[u8],
        owner: Option<(u32, u32)>,
    ) -> Result<DirDurability, EvidenceIo> {
        let (parent, name) = open_parent(anchor, relative)
            .map_err(|e| EvidenceIo::Io(io::Error::other(format!("cannot resolve the record path: {e:?}"))))?;
        let temp_name = sibling_name(relative, temp)?;
        let temp_fd = create_file(&parent, &temp_name, owner)?;
        write_all(temp_fd.as_raw_fd(), bytes).map_err(EvidenceIo::Io)?;
        sync_fd(temp_fd.as_raw_fd()).map_err(EvidenceIo::Io)?;
        drop(temp_fd);

        rename_sibling(&parent, &temp_name, &name)?;
        sync_dir(&parent).map_err(|e| {
            EvidenceIo::RenamedNotDurable(io::Error::other(format!(
                "the record was created but its directory fsync failed: {e}"
            )))
        })?;
        Ok(DirDurability::Persisted)
    }
}

/// 逐级打开父目录链，返回最终分量所在的目录 FD 与文件名
fn open_parent(anchor: &DirFd, relative: &RelativePath) -> Result<(OwnedFd, OsString), PathError> {
    let mut current = anchor
        .try_clone()
        .map_err(|e| PathError::Inaccessible(e.to_string()))?;
    let mut parts: Vec<&OsStr> = relative
        .as_path()
        .components()
        .filter_map(|component| match component {
            Component::Normal(name) => Some(name),
            _ => None,
        })
        .collect();
    let name = parts.pop().ok_or(PathError::Empty)?;
    for part in parts {
        current = open_dir(&current, part)?;
    }
    Ok((current, name.to_os_string()))
}

fn open_dir(parent: &OwnedFd, name: &OsStr) -> Result<OwnedFd, PathError> {
    let cname = c_name(name).map_err(|e| PathError::Inaccessible(e.to_string()))?;
    let raw = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            cname.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if raw < 0 {
        return Err(open_error(parent.as_raw_fd(), name, io::Error::last_os_error()));
    }
    Ok(unsafe { OwnedFd::from_raw_fd(raw) })
}

fn open_file(parent: &OwnedFd, name: &OsStr) -> Result<OwnedFd, PathError> {
    let cname = c_name(name).map_err(|e| PathError::Inaccessible(e.to_string()))?;
    let raw = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            cname.as_ptr(),
            // O_NONBLOCK：目录里可能出现 FIFO——open(O_RDONLY) 对它会等写者，
            // 而类型判别（fstat）发生在 open 之后，不加这个标志探测就先挂死。
            // 普通文件的 I/O 不受 O_NONBLOCK 影响
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
        )
    };
    if raw < 0 {
        return Err(open_error(parent.as_raw_fd(), name, io::Error::last_os_error()));
    }
    let fd = unsafe { OwnedFd::from_raw_fd(raw) };
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    if unsafe { libc::fstat(fd.as_raw_fd(), stat.as_mut_ptr()) } != 0 {
        return Err(PathError::Inaccessible(io::Error::last_os_error().to_string()));
    }
    let stat = unsafe { stat.assume_init() };
    match stat.st_mode & libc::S_IFMT {
        libc::S_IFREG => Ok(fd),
        libc::S_IFDIR => Err(PathError::IsDirectory),
        _ => Err(PathError::NotAFile),
    }
}

fn create_file(
    parent: &OwnedFd,
    name: &OsStr,
    owner: Option<(u32, u32)>,
) -> Result<OwnedFd, EvidenceIo> {
    let cname = c_name(name).map_err(EvidenceIo::Io)?;
    let raw = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            cname.as_ptr(),
            libc::O_WRONLY
                | libc::O_CREAT
                | libc::O_EXCL
                | libc::O_NOFOLLOW
                | libc::O_CLOEXEC,
            0o600 as libc::mode_t,
        )
    };
    if raw < 0 {
        return Err(EvidenceIo::Io(io::Error::last_os_error()));
    }
    let fd = unsafe { OwnedFd::from_raw_fd(raw) };
    if let Some((uid, gid)) = owner {
        let current = file_owner_fd(fd.as_raw_fd()).map_err(EvidenceIo::Io)?;
        if current != (uid, gid) && unsafe { libc::fchown(fd.as_raw_fd(), uid, gid) } != 0 {
            return Err(EvidenceIo::Io(io::Error::last_os_error()));
        }
    }
    Ok(fd)
}

/// 已解析证据文件的所有者；替换时用它保住记录不因写入者身份而换主
pub(crate) fn file_owner(file: &EvidenceFile) -> Result<(u32, u32), EvidenceIo> {
    file_owner_fd(file.state().fd.as_raw_fd()).map_err(EvidenceIo::Io)
}

fn file_owner_fd(fd: RawFd) -> io::Result<(u32, u32)> {
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    if unsafe { libc::fstat(fd, stat.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let stat = unsafe { stat.assume_init() };
    Ok((stat.st_uid, stat.st_gid))
}

fn write_all(fd: RawFd, bytes: &[u8]) -> io::Result<()> {
    let mut written = 0;
    while written < bytes.len() {
        let n = unsafe { libc::write(fd, bytes[written..].as_ptr().cast(), bytes.len() - written) };
        if n < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(err);
        }
        if n == 0 {
            return Err(io::Error::new(io::ErrorKind::WriteZero, "the evidence file accepted no bytes"));
        }
        written += n as usize;
    }
    Ok(())
}

fn rename_sibling(parent: &OwnedFd, from: &OsStr, to: &OsStr) -> Result<(), EvidenceIo> {
    let from = c_name(from).map_err(EvidenceIo::Io)?;
    let to = c_name(to).map_err(EvidenceIo::Io)?;
    let raw = unsafe {
        libc::renameat(parent.as_raw_fd(), from.as_ptr(), parent.as_raw_fd(), to.as_ptr())
    };
    if raw != 0 {
        return Err(EvidenceIo::Io(io::Error::last_os_error()));
    }
    Ok(())
}

fn sync_fd(fd: RawFd) -> io::Result<()> {
    if unsafe { libc::fsync(fd) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// 目录 fsync 失败时改名/删除已经发生：错误信息显式带上这一事实，调用方不得宣称已持久化
fn sync_dir(dir: &OwnedFd) -> Result<(), io::Error> {
    if unsafe { libc::fsync(dir.as_raw_fd()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// 临时文件必须与正式文件同目录：两段相对路径的父目录按文本相等
fn sibling_name(file: &RelativePath, temp: &RelativePath) -> Result<OsString, EvidenceIo> {
    let file_parent: Option<&Path> = file.as_path().parent();
    let temp_parent: Option<&Path> = temp.as_path().parent();
    if file_parent != temp_parent {
        return Err(EvidenceIo::Io(io::Error::new(
            io::ErrorKind::InvalidInput,
            "the temporary evidence file must sit in the same directory as the record",
        )));
    }
    temp.as_path()
        .file_name()
        .map(|name| name.to_os_string())
        .ok_or_else(|| {
            EvidenceIo::Io(io::Error::new(
                io::ErrorKind::InvalidInput,
                "the temporary evidence path has no file name",
            ))
        })
}

fn c_name(name: &OsStr) -> io::Result<CString> {
    CString::new(name.as_bytes()).map_err(|_| {
        io::Error::new(io::ErrorKind::InvalidInput, "a path component contains NUL")
    })
}

fn open_error(parent: RawFd, name: &OsStr, err: io::Error) -> PathError {
    match err.raw_os_error() {
        Some(libc::ELOOP) => PathError::Symlink,
        Some(libc::ENOENT) => PathError::NotFound,
        Some(libc::ENOTDIR) => {
            // O_NOFOLLOW 只作用于本次调用的尾分量；带 O_DIRECTORY 时符号链接可能先
            // 以 ENOTDIR 失败，故用 lstat 区分子类
            if is_symlink(parent, name) {
                PathError::Symlink
            } else {
                PathError::NotDirectory
            }
        }
        _ => PathError::Inaccessible(err.to_string()),
    }
}

fn is_symlink(parent: RawFd, name: &OsStr) -> bool {
    let Ok(cname) = c_name(name) else {
        return false;
    };
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    if unsafe { libc::fstatat(parent, cname.as_ptr(), stat.as_mut_ptr(), libc::AT_SYMLINK_NOFOLLOW) }
        != 0
    {
        return false;
    }
    let stat = unsafe { stat.assume_init() };
    stat.st_mode & libc::S_IFMT == libc::S_IFLNK
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recovery::{
        CleanupError, Evidence, OpKind, PlanDigest, PlanId, PlanVersion, Record, RecoveryEngine,
        SceneIdentity, TxId,
    };
    use crate::platform::{IdentityMaterial, TargetId};

    fn temp_dir(tag: &str) -> crate::testdir::TestDir {
        crate::testdir::test_dir(tag)
    }

    /// 父目录链中的符号链接被拒绝，最后一级的符号链接同样被拒绝
    #[test]
    fn symlink_components_are_refused() {
        let root = temp_dir("symlink");
        let real = root.join("real");
        std::fs::create_dir(&real).unwrap();
        std::fs::write(real.join("evidence.bin"), b"x").unwrap();
        std::os::unix::fs::symlink(&real, root.join("link")).unwrap();
        std::os::unix::fs::symlink(real.join("evidence.bin"), root.join("final")).unwrap();

        let anchor = DirFd::open_anchor(&root).unwrap();
        let store = HostEvidenceStore;
        let through_link = RelativePath::try_new("link/evidence.bin").unwrap();
        assert!(matches!(store.open_at(&anchor, &through_link), Err(PathError::Symlink)));
        let final_link = RelativePath::try_new("final").unwrap();
        assert!(matches!(store.open_at(&anchor, &final_link), Err(PathError::Symlink)));
    }

    /// 最终分量是目录时单独报出来
    #[test]
    fn a_directory_is_not_an_evidence_file() {
        let root = temp_dir("directory");
        std::fs::create_dir(root.join("evidence.bin")).unwrap();
        let anchor = DirFd::open_anchor(&root).unwrap();
        let store = HostEvidenceStore;
        let relative = RelativePath::try_new("evidence.bin").unwrap();
        assert!(matches!(store.open_at(&anchor, &relative), Err(PathError::IsDirectory)));
    }

    /// 整份替换后读到新内容，临时文件不再存在
    #[test]
    fn replace_is_atomic_and_readable() {
        let root = temp_dir("replace");
        std::fs::write(root.join("evidence.bin"), b"").unwrap();
        let anchor = DirFd::open_anchor(&root).unwrap();
        let store = HostEvidenceStore;
        let relative = RelativePath::try_new("evidence.bin").unwrap();
        let file = store.open_at(&anchor, &relative).unwrap();
        let temp = RelativePath::try_new("evidence.tmp.0001").unwrap();
        let durability = store.replace_atomic(&anchor, &file, &temp, b"record bytes").unwrap();
        assert_eq!(durability, DirDurability::Persisted);
        assert!(!root.join("evidence.tmp.0001").exists());

        let reopened = store.open_at(&anchor, &relative).unwrap();
        assert_eq!(store.read(&reopened).unwrap(), b"record bytes");
    }

    /// 临时文件与正式文件不同目录时拒绝替换
    #[test]
    fn a_temp_file_must_be_a_sibling() {
        let root = temp_dir("temp_sibling");
        std::fs::create_dir(root.join("other")).unwrap();
        std::fs::write(root.join("evidence.bin"), b"").unwrap();
        let anchor = DirFd::open_anchor(&root).unwrap();
        let store = HostEvidenceStore;
        let relative = RelativePath::try_new("evidence.bin").unwrap();
        let file = store.open_at(&anchor, &relative).unwrap();
        let temp = RelativePath::try_new("other/evidence.tmp").unwrap();
        assert!(matches!(
            store.replace_atomic(&anchor, &file, &temp, b"x"),
            Err(EvidenceIo::Io(_))
        ));
    }

    /// 提交与许可签发之后，清理入口真正删除文件并报告目录持久化
    #[test]
    fn cleanup_deletes_only_through_commit_and_permit() {
        let root = temp_dir("cleanup");
        let anchor = DirFd::open_anchor(&root).unwrap();
        let store = HostEvidenceStore;

        let current = SceneIdentity::from_target(TargetId::from_material(
            IdentityMaterial::image(None, Some([5; 16])).unwrap(),
        ));
        let mut record = Record::new(
            TxId::from_bytes([1; 16]),
            current.clone(),
            PlanId::from_bytes([2; 16]),
            PlanVersion::new(1),
            OpKind::ResizePart,
            PlanDigest::from_bytes([3; 32]),
            crate::dev::DiskGuidState::Unavailable,
        );
        let bytes = record.encode().unwrap();
        std::fs::write(root.join("evidence.bin"), &bytes).unwrap();

        let relative = RelativePath::try_new("evidence.bin").unwrap();
        let file = store.open_at(&anchor, &relative).unwrap();
        let evidence = Evidence::classify(&store.read(&file).unwrap(), &current);
        let outcome = RecoveryEngine::commit(evidence, false).unwrap();
        let (committed, permit, withheld) = outcome.into_parts();
        assert!(withheld.is_none());

        let durability =
            RecoveryEngine::cleanup(&store, &anchor, file, &committed, permit.unwrap()).unwrap();
        assert_eq!(durability, DirDurability::Persisted);
        assert!(!root.join("evidence.bin").exists());
    }

    /// 删除前回读与待删记录不符（tx_id 不同或字节已不可识别）：不删、保留记录
    #[test]
    fn cleanup_readback_mismatch_keeps_the_record() {
        let root = temp_dir("readback");
        let anchor = DirFd::open_anchor(&root).unwrap();
        let store = HostEvidenceStore;
        let current = SceneIdentity::from_target(TargetId::from_material(
            IdentityMaterial::image(None, Some([6; 16])).unwrap(),
        ));
        let mut on_disk = Record::new(
            TxId::from_bytes([1; 16]),
            current.clone(),
            PlanId::from_bytes([2; 16]),
            PlanVersion::new(1),
            OpKind::ResizePart,
            PlanDigest::from_bytes([3; 32]),
            crate::dev::DiskGuidState::Unavailable,
        );
        std::fs::write(root.join("evidence.bin"), on_disk.encode().unwrap()).unwrap();

        let other = Record::new(
            TxId::from_bytes([9; 16]),
            current,
            PlanId::from_bytes([2; 16]),
            PlanVersion::new(1),
            OpKind::ResizePart,
            PlanDigest::from_bytes([3; 32]),
            crate::dev::DiskGuidState::Unavailable,
        );
        let outcome = RecoveryEngine::commit(Evidence::Recognized(other), false).unwrap();
        let (committed, permit, withheld) = outcome.into_parts();
        assert!(withheld.is_none());
        let relative = RelativePath::try_new("evidence.bin").unwrap();
        let file = store.open_at(&anchor, &relative).unwrap();
        let err = RecoveryEngine::cleanup(&store, &anchor, file, &committed, permit.unwrap()).unwrap_err();
        assert!(matches!(err, CleanupError::ReadbackMismatch(_)), "{err:?}");
        assert!(root.join("evidence.bin").exists());

        // 字节已不可识别：同样不删。remove 消费句柄，拒绝后需重新打开
        std::fs::write(root.join("evidence.bin"), b"torn").unwrap();
        let file = store.open_at(&anchor, &relative).unwrap();
        let outcome = RecoveryEngine::commit(Evidence::Recognized(on_disk), false).unwrap();
        let (committed, permit, _) = outcome.into_parts();
        let err = RecoveryEngine::cleanup(&store, &anchor, file, &committed, permit.unwrap()).unwrap_err();
        assert!(matches!(err, CleanupError::ReadbackMismatch(_)), "{err:?}");
        assert!(root.join("evidence.bin").exists());
    }
}

