//! 块设备恢复记录仓库：`<state_root>/recovery/<identity-hash>`。
//!
//! 目录名取 identity-hash；锁键不在这条路径上——锁按载体
//! 派生（见 `targetlock`）。身份按证据强度分级：
//! WWN / EUI / 多次读数一致的 serial 为 `Strong`，dm / md / 只有 major:minor /
//! 读不出身份为 `Unusable`。状态根默认 `/var/lib/diskedit`，`DISKEDIT_STATE_DIR`
//! 只在 euid 0 时生效；HostRepository 根与文件属 uid 0，
//! 目录与文件 `mode & 0o022 == 0`。

#![cfg_attr(not(target_os = "linux"), allow(dead_code))]

use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

/// 默认状态根：FHS §5.8 的 `/var/lib/<name>`，主机级、跨重启保留
pub(crate) const DEFAULT_STATE_ROOT: &str = "/var/lib/diskedit";

/// 状态根。Linux 上 euid 非 0 时忽略 `DISKEDIT_STATE_DIR`；其余平台沿用
/// [`super::state_dir`] 的既有语义（这些平台没有块设备目标）
pub(crate) fn host_state_root() -> PathBuf {
    #[cfg(target_os = "linux")]
    {
        if euid_is_root()
            && let Some(v) = std::env::var_os("DISKEDIT_STATE_DIR")
            && !v.is_empty()
        {
            return PathBuf::from(v);
        }
        PathBuf::from(DEFAULT_STATE_ROOT)
    }
    #[cfg(not(target_os = "linux"))]
    {
        super::state_dir()
    }
}

#[cfg(target_os = "linux")]
pub(crate) fn euid_is_root() -> bool {
    // SAFETY: geteuid 无参数、无副作用
    unsafe { libc::geteuid() == 0 }
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn euid_is_root() -> bool {
    true
}

/// 块设备身份等级。`Unusable` 携带原因：写命令据此退 6，不降级
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum StableIdentity {
    Strong { device_class: &'static str, stable_key: String },
    Unusable(String),
}

impl StableIdentity {
    /// identity-hash 的文本形式（32 位小写 hex）；`Unusable` 没有可用的目录名
    pub(crate) fn hash_hex(&self) -> Option<String> {
        match self {
            Self::Strong { device_class, stable_key } => {
                Some(identity_hash_hex(device_class, stable_key, None))
            }
            Self::Unusable(_) => None,
        }
    }

    /// 本目标记录目录。`record` 落在它下面；锁不在这一层——块设备的锁按载体落在
    /// `state_root` 根下，镜像的锁在 sidecar 目录内（见 `targetlock`）
    pub(crate) fn record_dir(&self) -> Option<PathBuf> {
        self.hash_hex().map(|hash| record_dir_under(&host_state_root(), &hash))
    }

    /// 单文件恢复记录的落点：`<state_root>/recovery/<identity-hash>/record`
    pub(crate) fn record_path(&self) -> Option<PathBuf> {
        self.record_dir().map(|dir| dir.join("record"))
    }
}

/// 纯路径拼装：测试给显式根，生产走 [`host_state_root`]
pub(crate) fn record_dir_under(root: &Path, hash_hex: &str) -> PathBuf {
    root.join("recovery").join(hash_hex)
}

/// identity-hash：恒等字段按序拼接后取 sha256 前 16 字节。
/// 各字段复用 `String`（`u32 LE` 字节数 + UTF-8）与 `Option<[u8; 20]>`
/// （`u8` 标志，1 时接 20 字节原序）的编码
pub(crate) fn identity_hash(
    device_class: &str,
    stable_key: &str,
    file_id: Option<[u8; 20]>,
) -> [u8; 16] {
    let mut input = Vec::with_capacity(8 + device_class.len() + stable_key.len() + 21);
    push_string(&mut input, device_class);
    push_string(&mut input, stable_key);
    match file_id {
        None => input.push(0),
        Some(id) => {
            input.push(1);
            input.extend_from_slice(&id);
        }
    }
    let digest = Sha256::digest(&input);
    let mut out = [0u8; 16];
    out.copy_from_slice(&digest[..16]);
    out
}

pub(crate) fn identity_hash_hex(
    device_class: &str,
    stable_key: &str,
    file_id: Option<[u8; 20]>,
) -> String {
    crate::support::hex_lower(&identity_hash(device_class, stable_key, file_id))
}

fn push_string(out: &mut Vec<u8>, value: &str) {
    out.extend_from_slice(&(value.len() as u32).to_le_bytes());
    out.extend_from_slice(value.as_bytes());
}

/// 块设备身份探测。读取失败、dm / md、以及没有任何 WWN / EUI / 可重复 serial
/// 都返回 `Unusable`；写命令据此退 6，不退回 major:minor
#[cfg(target_os = "linux")]
pub(crate) fn block_stable_identity(node: &Path) -> StableIdentity {
    match probe_strong_identity(node) {
        Ok(Some((device_class, stable_key))) => {
            StableIdentity::Strong { device_class, stable_key }
        }
        Ok(None) => StableIdentity::Unusable(
            "no WWN, EUI or repeatable serial is readable on this device".to_string(),
        ),
        Err(why) => StableIdentity::Unusable(why),
    }
}

#[cfg(target_os = "linux")]
fn probe_strong_identity(node: &Path) -> Result<Option<(&'static str, String)>, String> {
    let disk = super::block_device_at(node)
        .map_err(|e| format!("cannot resolve the whole-disk node of {}: {e}", node.display()))?;
    if has_sysfs_dir(&disk, "dm", "dm/uuid")? {
        return Err(
            "device-mapper targets are not a strong identity (dm / DM_UUID is not accepted)"
                .to_string(),
        );
    }
    if has_sysfs_dir(&disk, "md", "md/uuid")? {
        return Err("md targets are not a strong identity (md/uuid is not accepted)".to_string());
    }
    let partition = super::read_sysfs_attr(&node.join("partition"))
        .map_err(|e| format!("cannot read the partition attribute of {}: {e}", node.display()))?
        .map(|raw| {
            raw.parse::<u32>()
                .map_err(|e| format!("partition attribute is {raw:?}, not a number ({e})"))
        })
        .transpose()?;
    let Some(carrier) = strong_carrier(&disk)? else {
        return Ok(None);
    };
    Ok(Some(match partition {
        Some(number) => ("partition", format!("{carrier}{number}")),
        None => ("block", carrier),
    }))
}

/// dm / md 的判据：对应的属性可读，或 sysfs 目录本身存在。两者任一成立都算
/// ——`dm/uuid` 读不出来不能等同于"不是 dm"
#[cfg(target_os = "linux")]
fn has_sysfs_dir(disk: &Path, dir: &str, attr: &str) -> Result<bool, String> {
    match super::read_sysfs_attr(&disk.join(attr)) {
        Ok(Some(_)) => Ok(true),
        Ok(None) => Ok(disk.join(dir).is_dir()),
        Err(e) => Err(format!("cannot read sysfs {attr} of {}: {e}", disk.display())),
    }
}

/// WWN → EUI → serial：`wwid`、`device/wwid` 取第一个非空原文，再由
/// `device/serial` 兜底。serial 要求连续两次读数一致才当身份
#[cfg(target_os = "linux")]
fn strong_carrier(disk: &Path) -> Result<Option<String>, String> {
    for attr in ["wwid", "device/wwid"] {
        match super::read_sysfs_attr(&disk.join(attr)) {
            Ok(Some(v)) if !v.is_empty() => return Ok(Some(v)),
            Ok(_) => {}
            Err(e) => return Err(format!("cannot read sysfs {attr} of {}: {e}", disk.display())),
        }
    }
    let read_serial = || {
        super::read_sysfs_attr(&disk.join("device/serial"))
            .map_err(|e| format!("cannot read sysfs device/serial of {}: {e}", disk.display()))
    };
    let first = read_serial()?;
    let second = read_serial()?;
    match (first, second) {
        (Some(a), Some(b)) if a == b && a.is_empty() => Ok(None),
        (Some(a), Some(b)) if a == b => Ok(Some(a)),
        (Some(_), Some(_)) | (Some(_), None) | (None, Some(_)) => Err(
            "the device serial reads differently on repeated reads; it is not a stable identity"
                .to_string(),
        ),
        (None, None) => Ok(None),
    }
}

/// 准备本目标的记录目录。逐级创建 `<state_root>`、`<state_root>/recovery`、
/// `<state_root>/recovery/<hash>`，并核对 uid 0 与 `mode & 0o022 == 0`
pub(crate) fn ensure_record_dir(record: &Path) -> Result<(), HostRepoError> {
    let dir = record
        .parent()
        .ok_or_else(|| HostRepoError::Io(std::io::Error::other("the record path has no parent")))?;
    #[cfg(target_os = "linux")]
    {
        if !euid_is_root() {
            return Err(HostRepoError::NotRoot);
        }
        let recovery = dir
            .parent()
            .ok_or_else(|| HostRepoError::Io(std::io::Error::other("the record directory has no parent")))?;
        let root = recovery
            .parent()
            .ok_or_else(|| HostRepoError::Io(std::io::Error::other("the recovery directory has no parent")))?;
        for d in [root, recovery, dir] {
            create_dir_uid0(d)?;
        }
        Ok(())
    }
    #[cfg(not(target_os = "linux"))]
    {
        std::fs::create_dir_all(dir).map_err(HostRepoError::Io)
    }
}

/// 镜像旁的记录目录 `<image>.diskedit/`：`0700`、与目标文件同主。
/// 已存在时按同一形状复核；符号链接或非目录一律拒绝
pub(crate) fn ensure_sidecar_dir(dir: &Path, owner: Option<(u32, u32)>) -> Result<(), HostRepoError> {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};

        let created = match std::fs::DirBuilder::new().mode(0o700).create(dir) {
            Ok(()) => true,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => false,
            Err(e) => return Err(HostRepoError::Io(e)),
        };
        if created
            && let Some((uid, gid)) = owner
        {
            std::os::unix::fs::chown(dir, Some(uid), Some(gid)).map_err(HostRepoError::Io)?;
        }
        let meta = std::fs::symlink_metadata(dir).map_err(HostRepoError::Io)?;
        if !meta.is_dir() {
            return Err(HostRepoError::Insecure(format!("{} is not a directory", dir.display())));
        }
        if let Some((uid, gid)) = owner
            && (meta.uid() != uid || meta.gid() != gid)
        {
            return Err(HostRepoError::Insecure(format!(
                "{} is owned by {}:{}, not by the target file's {}:{}",
                dir.display(),
                meta.uid(),
                meta.gid(),
                uid,
                gid
            )));
        }
        if meta.permissions().mode() & 0o022 != 0 {
            return Err(HostRepoError::Insecure(format!("{} is writable by group or other", dir.display())));
        }
        Ok(())
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = owner;
        std::fs::create_dir_all(dir).map_err(HostRepoError::Io)
    }
}

/// 取锁前准备记录目录：镜像的 `record.lock` 落在 sidecar 目录内，目录必须先于锁就位；
/// 块设备的锁落在 `state_root` 根下，逐级建出记录目录时同时建出该根。
/// 身份没有记录落点（dm / md / 无稳定身份的块设备）时不动，由调用方决定退 6
pub(crate) fn prepare_record_dir(
    identity: &super::TargetIdentity,
    owner: Option<(u32, u32)>,
) -> Result<(), HostRepoError> {
    let Some(record) = identity.record_path() else {
        return Ok(());
    };
    match identity.kind {
        super::TargetKind::Block => ensure_record_dir(record),
        super::TargetKind::Image => {
            let dir = record.parent().ok_or_else(|| {
                HostRepoError::Io(std::io::Error::other("the record path has no parent"))
            })?;
            ensure_sidecar_dir(dir, owner)
        }
    }
}

#[cfg(target_os = "linux")]
fn create_dir_uid0(dir: &Path) -> Result<(), HostRepoError> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};

    match std::fs::DirBuilder::new().mode(0o700).create(dir) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(HostRepoError::Io(e)),
    }
    let meta = std::fs::symlink_metadata(dir).map_err(HostRepoError::Io)?;
    if !meta.is_dir() {
        return Err(HostRepoError::Insecure(format!("{} is not a directory", dir.display())));
    }
    if meta.uid() != 0 {
        return Err(HostRepoError::Insecure(format!(
            "{} is owned by uid {}, not 0",
            dir.display(),
            meta.uid()
        )));
    }
    if meta.permissions().mode() & 0o022 != 0 {
        return Err(HostRepoError::Insecure(format!(
            "{} is writable by group or other",
            dir.display()
        )));
    }
    Ok(())
}

#[derive(Debug)]
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) enum HostRepoError {
    /// 非 root：HostRepository 根与文件要求 uid 0，写命令退 6
    NotRoot,
    /// 目录或文件的所有者 / 权限形状不满足要求
    Insecure(String),
    Io(std::io::Error),
}

impl std::fmt::Display for HostRepoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotRoot => f.write_str("the recovery repository must be owned by uid 0"),
            Self::Insecure(detail) => f.write_str(detail),
            Self::Io(e) => write!(f, "{e}"),
        }
    }
}
