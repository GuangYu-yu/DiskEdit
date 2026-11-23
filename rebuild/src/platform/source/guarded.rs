//! 私有目录、私有文件与原子写。
//!
//! 目录以 0700 创建，文件以 0600 打开；打开文件时带 `O_NOFOLLOW` 与 `O_CLOEXEC`，
//! 并拒绝末级符号链接和硬链接数大于 1 的文件，避免经链接写到另一个对象。
//!
//! 原子写先把内容写入与目标同目录的暂存文件，`fsync` 后再 `rename` 为正式名，
//! 最后同步目录项。暂存文件名为 `.<name>.<pid>.<n>.tmp`，`n` 从 0 起递增，
//! 以 `O_CREAT | O_EXCL` 抢占，冲突时换下一个编号。

use std::path::{Path, PathBuf};

use super::super::{PlatformError, PlatformResult};

/// 逐级创建权限为 0700 的目录；已存在且是目录时视为成功。
#[cfg(unix)]
pub fn create_private_dir(path: &Path) -> PlatformResult<()> {
    use std::os::unix::fs::DirBuilderExt;

    let mut current = PathBuf::new();
    for component in path.components() {
        current.push(component);
        let mut builder = std::fs::DirBuilder::new();
        builder.mode(0o700);
        match builder.create(&current) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                let metadata = std::fs::symlink_metadata(&current)
                    .map_err(|error| PlatformError::io(&current, error))?;
                if metadata.file_type().is_symlink() {
                    return Err(PlatformError::unsupported(format!(
                        "{} 是符号链接，拒绝在其上创建目录",
                        current.display()
                    )));
                }
                if !metadata.is_dir() {
                    return Err(PlatformError::unsupported(format!(
                        "{} 已存在且不是目录",
                        current.display()
                    )));
                }
            }
            Err(error) => return Err(PlatformError::io(&current, error)),
        }
    }
    Ok(())
}

/// 非 Unix 平台不提供该目录权限模型。
#[cfg(not(unix))]
pub fn create_private_dir(path: &Path) -> PlatformResult<()> {
    Err(PlatformError::not_applicable(format!(
        "当前平台不支持创建私有目录 {}",
        path.display()
    )))
}

/// 以 0600 打开文件；`create` 为真时允许创建。
///
/// 末级为符号链接、不是普通文件、或硬链接数大于 1 时拒绝。
#[cfg(unix)]
pub fn open_private_file(path: &Path, create: bool) -> PlatformResult<std::fs::File> {
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};

    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return Err(PlatformError::unsupported(format!(
                "{} 是符号链接，拒绝打开",
                path.display()
            )));
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(PlatformError::io(path, error)),
    }

    let mut options = std::fs::OpenOptions::new();
    options
        .read(true)
        .write(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .mode(0o600);
    if create {
        options.create(true);
    }
    let file = options
        .open(path)
        .map_err(|error| PlatformError::io(path, error))?;
    let metadata = file
        .metadata()
        .map_err(|error| PlatformError::io(path, error))?;
    if !metadata.is_file() {
        return Err(PlatformError::unsupported(format!(
            "{} 不是普通文件",
            path.display()
        )));
    }
    if metadata.nlink() > 1 {
        return Err(PlatformError::unsupported(format!(
            "{} 的硬链接数为 {}，拒绝打开",
            path.display(),
            metadata.nlink()
        )));
    }
    Ok(file)
}

/// 非 Unix 平台不提供该打开方式。
#[cfg(not(unix))]
pub fn open_private_file(path: &Path, _create: bool) -> PlatformResult<std::fs::File> {
    Err(PlatformError::not_applicable(format!(
        "当前平台不支持以私有方式打开 {}",
        path.display()
    )))
}

/// 在 `dir` 下原子写出 `bytes`，正式名为 `name`。
#[cfg(unix)]
pub fn write_atomic(dir: &Path, name: &str, bytes: &[u8]) -> PlatformResult<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;

    if name.is_empty() || name.contains('/') || name == "." || name == ".." {
        return Err(PlatformError::parse(format!("不是合法的文件名：{name:?}")));
    }
    let parent = std::fs::symlink_metadata(dir).map_err(|error| PlatformError::io(dir, error))?;
    if !parent.is_dir() {
        return Err(PlatformError::unsupported(format!(
            "{} 不是目录",
            dir.display()
        )));
    }
    let final_path = dir.join(name);
    let pid = std::process::id();
    let mut attempt: u32 = 0;
    loop {
        let tmp = dir.join(format!(".{name}.{pid}.{attempt}.tmp"));
        let mut options = std::fs::OpenOptions::new();
        options
            .write(true)
            .create_new(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .mode(0o600);
        match options.open(&tmp) {
            Ok(mut file) => {
                let written = file
                    .write_all(bytes)
                    .and_then(|()| file.sync_all())
                    .map_err(|error| PlatformError::io(&tmp, error));
                if let Err(error) = written {
                    discard(&tmp);
                    return Err(error);
                }
                drop(file);
                if let Err(error) = std::fs::rename(&tmp, &final_path) {
                    discard(&tmp);
                    return Err(PlatformError::io(&final_path, error));
                }
                sync_dir(dir)?;
                return Ok(());
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                attempt = match attempt.checked_add(1) {
                    Some(next) => next,
                    None => {
                        return Err(PlatformError::unsupported(format!(
                            "在 {} 下无法分配到可用的暂存文件名",
                            dir.display()
                        )));
                    }
                };
            }
            Err(error) => return Err(PlatformError::io(&tmp, error)),
        }
    }
}

/// 非 Unix 平台不提供该写入方式。
#[cfg(not(unix))]
pub fn write_atomic(dir: &Path, _name: &str, _bytes: &[u8]) -> PlatformResult<()> {
    Err(PlatformError::not_applicable(format!(
        "当前平台不支持在 {} 下原子写入",
        dir.display()
    )))
}

#[cfg(unix)]
fn discard(path: &Path) {
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(_) => {}
    }
}

#[cfg(unix)]
fn sync_dir(dir: &Path) -> PlatformResult<()> {
    let handle = std::fs::File::open(dir).map_err(|error| PlatformError::io(dir, error))?;
    handle
        .sync_all()
        .map_err(|error| PlatformError::io(dir, error))
}
