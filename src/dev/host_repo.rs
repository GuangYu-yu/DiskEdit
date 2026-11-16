//! 块设备恢复记录仓库：`<state_root>/recovery/<identity-hash>`。
//!
//! 目录名取 identity-hash；锁键不在这条路径上——锁按载体
//! 派生（见 `targetlock`）。身份按证据强度分级：
//! WWN / EUI / 多次读数一致的 serial 为 `Strong`，dm / md / 只有 major:minor /
//! 读不出身份为 `Unusable`。状态根默认 `/var/lib/diskedit`，`DISKEDIT_STATE_DIR`
//! 只在 euid 0 时生效；HostRepository 根与文件属 uid 0，
//! 目录 `mode & 0o077 == 0`。

#![cfg_attr(not(target_os = "linux"), allow(dead_code))]

use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

/// 默认状态根：FHS §5.8 的 `/var/lib/<name>`，主机级、跨重启保留
pub(crate) const DEFAULT_STATE_ROOT: &str = "/var/lib/diskedit";

/// 状态根。Linux 上 euid 非 0 时忽略 `DISKEDIT_STATE_DIR`；其余平台返回
/// [`super::state_dir`]（这些平台没有块设备目标）
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

/// 块设备记录目录的身份键：目标（整盘载体稳定身份原文）+ 分区号。
///
/// 两个分量分别编码——目标用长度前缀，分区用一个标志字节加 `u32` 小端——不拼成
/// 一个字符串：拼接会让 `("ABC1", 2)` 与 `("ABC", 12)` 得到同一份输入，两个不同的
/// （目标, 分区）组合因此落到同一个记录目录。
///
/// 构造只发生在 [`probe_strong_identity`]（同模块内）；其余模块拿到的是已经定型的键，
/// 读记录与续跑判定都按它派生落点。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct IdentityKey {
    target: String,
    partition: Option<u32>,
}

impl IdentityKey {
    fn new(target: String, partition: Option<u32>) -> Self {
        Self { target, partition }
    }

    /// identity-hash 的文本形式（32 位小写 hex）
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub(crate) fn hash_hex(&self) -> String {
        identity_hash_hex(self)
    }

    /// 本目标记录目录。`record` 落在它下面；锁不在这一层——块设备的锁按载体落在
    /// `state_root` 根下，镜像的锁在 sidecar 目录内（见 `targetlock`）
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub(crate) fn record_dir(&self) -> PathBuf {
        record_dir_under(&host_state_root(), &self.hash_hex())
    }

    /// 单文件恢复记录的落点：`<state_root>/recovery/<identity-hash>/record`
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub(crate) fn record_path(&self) -> PathBuf {
        self.record_dir().join(super::RECORD_NAME)
    }

    fn encode(&self, out: &mut Vec<u8>) {
        push_string(out, &self.target);
        match self.partition {
            None => out.push(0),
            Some(number) => {
                out.push(1);
                out.extend_from_slice(&number.to_le_bytes());
            }
        }
    }
}

/// 测试用构造：生产代码只能经 [`probe_strong_identity`] 得到键
#[cfg(test)]
pub(crate) fn key_for_test(target: &str, partition: Option<u32>) -> IdentityKey {
    IdentityKey::new(target.to_string(), partition)
}

/// 纯路径拼装：测试给显式根，生产走 [`host_state_root`]
pub(crate) fn record_dir_under(root: &Path, hash_hex: &str) -> PathBuf {
    root.join("recovery").join(hash_hex)
}

/// identity-hash：键的分量按 [`IdentityKey::encode`] 编码后取 sha256 前 16 字节
fn identity_hash(key: &IdentityKey) -> [u8; 16] {
    let mut input = Vec::with_capacity(9 + key.target.len());
    key.encode(&mut input);
    let digest = Sha256::digest(&input);
    let mut out = [0u8; 16];
    out.copy_from_slice(&digest[..16]);
    out
}

fn identity_hash_hex(key: &IdentityKey) -> String {
    crate::support::hex_lower(&identity_hash(key))
}

fn push_string(out: &mut Vec<u8>, value: &str) {
    out.extend_from_slice(&(value.len() as u32).to_le_bytes());
    out.extend_from_slice(value.as_bytes());
}

/// 块设备记录目录的身份键。读取失败、dm / md、以及没有任何 WWN / EUI / 可重复
/// serial 都没有键；写命令据此退 6，不退回 major:minor。
/// 载体与分区号的读取在 `dev::topology::record_carrier`——那里是 sysfs 拓扑事实的唯一
/// 出口，本模块只负责把它变成记录目录的键
#[cfg(target_os = "linux")]
pub(crate) fn block_stable_identity(node: &Path) -> Result<IdentityKey, String> {
    match super::record_carrier(node)? {
        Some((carrier, partition)) => Ok(IdentityKey::new(carrier, partition)),
        None => Err("no WWN, EUI or repeatable serial is readable on this device".to_string()),
    }
}

/// 准备本目标的记录目录。逐级创建 `<state_root>`、`<state_root>/recovery`、
/// `<state_root>/recovery/<hash>`，并核对 uid 0 与 `mode & 0o077 == 0`
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

/// 镜像记录目录 `<image_state_root>/images/<键摘要>/`：`0700`、属主为当前用户。
/// 逐级核对状态根、`images`、键目录：任何一级是符号链接 / 非目录 / 对其他用户可读都拒绝
/// （记录里存着写前的分区表）。状态根之上的中间层不属于本工具的命名空间，只建不核
pub(crate) fn ensure_image_record_dir(dir: &Path) -> Result<(), HostRepoError> {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};

        let images = dir.parent().ok_or_else(|| {
            HostRepoError::Io(std::io::Error::other("the image record directory has no parent"))
        })?;
        let root = images.parent().ok_or_else(|| {
            HostRepoError::Io(std::io::Error::other("the image directory has no parent"))
        })?;
        // 状态根之上的中间层只建不核；`root` 本身必须按 0700 由下面的循环建立，
        // 预建一次会让它带上 umask 的 0755，随后被同一轮复核拒绝
        let parent = root.parent().ok_or_else(|| {
            HostRepoError::Io(std::io::Error::other("the image state root has no parent"))
        })?;
        std::fs::create_dir_all(parent).map_err(HostRepoError::Io)?;
        let (uid, gid) = current_euid_gid();
        for d in [root, images, dir] {
            match std::fs::DirBuilder::new().mode(0o700).create(d) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(HostRepoError::Io(e)),
            }
            let meta = std::fs::symlink_metadata(d).map_err(HostRepoError::Io)?;
            if !meta.is_dir() {
                return Err(HostRepoError::Insecure(format!("{} is not a directory", d.display())));
            }
            if meta.uid() != uid || meta.gid() != gid {
                return Err(HostRepoError::Insecure(format!(
                    "{} is owned by {}:{}, not by the current user {}:{}",
                    d.display(),
                    meta.uid(),
                    meta.gid(),
                    uid,
                    gid
                )));
            }
            let mode = meta.permissions().mode();
            if mode & 0o077 != 0 {
                return Err(HostRepoError::Insecure(format!(
                    "{} is accessible to group or other (mode {:04o}); the recovery record holds the pre-write \
                     partition table, which other local accounts must not read — run: chmod 700 {}",
                    d.display(),
                    mode & 0o7777,
                    d.display()
                )));
            }
        }
        Ok(())
    }
    #[cfg(not(target_os = "linux"))]
    {
        std::fs::create_dir_all(dir).map_err(HostRepoError::Io)
    }
}

/// 当前进程的 euid / egid：镜像记录目录归写下它的用户，不归 root
#[cfg(target_os = "linux")]
fn current_euid_gid() -> (u32, u32) {
    // SAFETY: geteuid / getegid 无参数、无副作用
    unsafe { (libc::geteuid(), libc::getegid()) }
}

/// 镜像旁的记录目录 `<image>.diskedit/`：`0700`、与目标文件同主。
/// 已存在时按同一形状复核：group/other 的任何权限都拒绝（记录里存着写前的分区表，
/// 同机其他账户不该读到）；符号链接或非目录一律拒绝
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
        let mode = meta.permissions().mode();
        if mode & 0o077 != 0 {
            return Err(HostRepoError::Insecure(format!(
                "{} is accessible to group or other (mode {:04o}); the recovery record holds the pre-write \
                 partition table, which other local accounts must not read — run: chmod 700 {}",
                dir.display(),
                mode & 0o7777,
                dir.display()
            )));
        }
        Ok(())
    }
    #[cfg(not(target_os = "linux"))]
    {
        // audit-allow: D-23
        let _ = owner;
        std::fs::create_dir_all(dir).map_err(HostRepoError::Io)
    }
}

/// 取锁前准备记录目录：镜像的记录目录落在每用户状态根里（无状态根时锁退回 sidecar
/// 目录，写入本身会因没有记录落点被拒）；块设备的锁落在 `state_root` 根下，逐级建出
/// 记录目录时同时建出该根。身份没有记录落点（dm / md / 无稳定身份的块设备）时不动，
/// 由调用方决定退 6
pub(crate) fn prepare_record_dir(
    identity: &super::TargetIdentity,
    owner: Option<(u32, u32)>,
) -> Result<(), HostRepoError> {
    match identity.kind {
        super::TargetKind::Block => {
            let Some(record) = identity.record_path() else {
                return Ok(());
            };
            ensure_record_dir(record)
        }
        super::TargetKind::Image => match identity.record_path() {
            Some(record) => {
                let dir = record.parent().ok_or_else(|| {
                    HostRepoError::Io(std::io::Error::other("the record path has no parent"))
                })?;
                ensure_image_record_dir(dir)
            }
            None => {
                let dir = super::suffix_path(identity.data_path(), super::RECORD_SUFFIX);
                ensure_sidecar_dir(&dir, owner)
            }
        },
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
    let mode = meta.permissions().mode();
    if mode & 0o077 != 0 {
        return Err(HostRepoError::Insecure(format!(
            "{} is accessible to group or other (mode {:04o}); the recovery record holds the pre-write \
             partition table, which other local accounts must not read — run: chmod 700 {}",
            dir.display(),
            mode & 0o7777,
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

#[cfg(test)]
mod tests {
    use super::*;

    /// 目标与分区分量分别编码：目标带长度前缀，分区用标志字节 + `u32` 小端，
    /// `("ABC1", 2)` 与 `("ABC", 12)` 因此得到两个不同的键、两个不同的落点
    #[test]
    fn the_identity_key_keeps_the_target_and_the_partition_apart() {
        let carrier_one = IdentityKey::new("ABC1".to_string(), Some(2));
        let carrier_twelve = IdentityKey::new("ABC".to_string(), Some(12));
        assert_eq!(format!("{}{}", "ABC1", 2), format!("{}{}", "ABC", 12));
        assert_ne!(carrier_one, carrier_twelve);
        assert_ne!(carrier_one.hash_hex(), carrier_twelve.hash_hex());
        assert_ne!(
            carrier_one,
            IdentityKey::new("ABC1".to_string(), Some(23)),
            "the partition number is a component, not a suffix of the target"
        );
        assert_ne!(
            IdentityKey::new("ABC1".to_string(), None),
            IdentityKey::new("ABC1".to_string(), Some(1)),
            "a whole disk and its first partition are different targets"
        );
    }

    /// 键的编码固定下来：分量顺序、长度前缀、分区标志都不能在版本间漂移，
    /// 否则盘上已有的记录目录会集体失去归属
    #[test]
    fn the_identity_hash_pins_the_key_encoding() {
        let block = IdentityKey::new("naa.5000c500aabbccdd".to_string(), None);
        assert_eq!(block.hash_hex(), "f244755a6aac2f9a88fd04c61930c017");
        assert_eq!(
            IdentityKey::new("naa.5000c500aabbccdd".to_string(), Some(1)).hash_hex(),
            "6e81eae3518e9dfc1342d7011a62b13c"
        );
        assert_eq!(
            IdentityKey::new("eui.0025385812345678".to_string(), None).hash_hex(),
            "fd180a0e4410f80e4c17e2614502d84f"
        );
        assert_eq!(block.hash_hex().len(), 32);
        assert_eq!(
            block.record_dir(),
            record_dir_under(&host_state_root(), "f244755a6aac2f9a88fd04c61930c017")
        );
        assert!(block.record_path().ends_with(crate::dev::RECORD_NAME));
    }

    /// 拼接式旧键算出的目录名既不是 `("ABC1", 2)` 的落点，也不是 `("ABC", 12)` 的落点。
    /// 目录名只决定派生落点：记录根按载荷身份认领，旧键目录里的记录只要载荷声明本目标，
    /// 仍会进入本目标的候选集（见 `recovery::engine::scan_record_root`）
    #[test]
    fn a_legacy_concatenated_slot_is_not_the_key_of_either_target() {
        assert_eq!(legacy_hash_hex("partition", "ABC12"), "9aa2c12926b1417f1830d1c77d09cb23");
        let legacy = legacy_hash_hex("partition", "ABC12");
        assert_ne!(legacy, IdentityKey::new("ABC1".to_string(), Some(2)).hash_hex());
        assert_ne!(legacy, IdentityKey::new("ABC".to_string(), Some(12)).hash_hex());
        assert_ne!(
            legacy_hash_hex("block", "naa.5000c500aabbccdd"),
            IdentityKey::new("naa.5000c500aabbccdd".to_string(), None).hash_hex()
        );
    }

    /// 旧版 identity-hash：`device_class` + 拼成一串的载体与分区号 + 空的 file_id 标志
    fn legacy_hash_hex(device_class: &str, stable_key: &str) -> String {
        let mut input = Vec::new();
        push_string(&mut input, device_class);
        push_string(&mut input, stable_key);
        input.push(0);
        crate::support::hex_lower(&Sha256::digest(&input)[..16])
    }
}
