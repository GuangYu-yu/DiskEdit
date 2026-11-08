//! 记录准入前的占用判定。
//!
//! 作用域是**占用域**：整盘取该盘自身与它下面全部分区；分区取父盘与父盘全部分区；
//! dm/md 没有分区子目录，退化为设备自身。对域内每台设备依次取五条证据，任一命中
//! 即为占用：
//!
//! 1. mountinfo 的 major:minor（`/proc/self/mountinfo` 与其他可读命名空间的同一视图）；
//! 2. `/proc/swaps` 的 Filename（块设备比 `rdev`，交换文件比 `(dev, ino)`）；
//! 3. sysfs `holders/` 非空（首层非空即命中；下钻只为把持有链写进文案，至多 8 层）；
//! 4. 设备作为成员出现在任一 `/sys/block/md*/md/dev-*`；
//! 5. 目标为镜像时，任一 loop 的 `loop/backing_file` 还原后 `(dev, ino)` 与目标相同。
//!
//! 证据源存在却不可读、不可解析，或路径到 major:minor / `(dev, ino)` 的解析失败一律按
//! 已占用（fail-closed）；系统没有对应子系统（`/sys/block` 下没有 loop、没有 md）是
//! 已证明不存在。两种结论都落 `RunOutcome::refused`（3）：占用是请求与现状不匹配，不是盘故障。
//! 只读命令不经过这里。

use super::*;

/// 占用判定结论
#[cfg(target_os = "linux")]
pub(crate) enum Verdict {
    Idle,
    /// 命中证据：文案已含设备名、证据源与解除动作
    Occupied(String),
    /// 证据源存在却确认不了：按已占用处理（fail-closed）
    Unknown(String),
}

/// 记录准入的占用结论：`Err` 是拒绝文案，区分"确实在用"与
/// "确认不了"。与 [`refuse_if_occupied`] 共用同一份探测
pub(crate) fn occupied_detail(src: &FileSource) -> Result<(), String> {
    #[cfg(not(target_os = "linux"))]
    let _ = src;
    #[cfg(target_os = "linux")]
    return match probe(src) {
        Verdict::Idle => Ok(()),
        Verdict::Occupied(detail) => Err(format!(
            "{detail} — the target is in use; release it and retry \
             (`umount` / `swapoff` / `dmsetup remove` / `mdadm --stop` / `losetup -d`)"
        )),
        Verdict::Unknown(detail) => Err(format!(
            "cannot confirm the target is unused: {detail} — refusing (fail-closed)"
        )),
    };
    #[cfg(not(target_os = "linux"))]
    Ok(())
}

/// 记录准入的占用闸。占用或不可判定都返回 [`crate::outcome::RunOutcome::refused`]（3），
/// 文案区分"确实在用"与"确认不了"
pub(crate) fn refuse_if_occupied(src: &FileSource) -> Result<(), crate::outcome::RunOutcome> {
    occupied_detail(src).map_err(crate::outcome::RunOutcome::refused)
}

/// 只读命令的事实入口：报出 [`probe`] 的结论，不做准入判定，也不影响退出码。
/// 与 [`refuse_if_occupied`] 共用同一份探测，命中时给出首个证据
#[cfg(target_os = "linux")]
pub(crate) fn describe(src: &FileSource) -> (&'static str, Option<String>) {
    match probe(src) {
        Verdict::Idle => ("idle", None),
        Verdict::Occupied(detail) => ("occupied", Some(detail)),
        Verdict::Unknown(detail) => ("undecidable", Some(detail)),
    }
}

#[cfg(target_os = "linux")]
use std::os::unix::fs::MetadataExt;
#[cfg(target_os = "linux")]
use std::path::{Path, PathBuf};

/// 占用域内的一台块设备：sysfs 目录名、目录本身、`(major, minor)`
#[cfg(target_os = "linux")]
struct ScopedDevice {
    name: String,
    sysdir: PathBuf,
    dev_no: (u64, u64),
}

/// 被探测的目标形态
#[cfg(target_os = "linux")]
enum Target {
    /// 已打开句柄的 `rdev`
    Block(u64),
    /// 已打开句柄的 `(st_dev, st_ino)`
    Image(u64, u64),
}

#[cfg(target_os = "linux")]
fn probe(src: &FileSource) -> Verdict {
    let meta = match src.metadata() {
        Ok(meta) => meta,
        Err(e) => return Verdict::Unknown(format!("cannot stat the open target: {e}")),
    };
    if !src.is_block {
        return probe_image(&meta);
    }
    let scope = match device_scope(meta.rdev()) {
        Ok(scope) => scope,
        Err(detail) => return Verdict::Unknown(detail),
    };
    match mounts_hit(&scope) {
        Ok(Some(detail)) => return Verdict::Occupied(detail),
        Ok(None) => {}
        Err(detail) => return Verdict::Unknown(detail),
    }
    match swaps_hit(Target::Block(meta.rdev())) {
        Ok(Some(detail)) => return Verdict::Occupied(detail),
        Ok(None) => {}
        Err(detail) => return Verdict::Unknown(detail),
    }
    for device in &scope {
        match holder_chain(&device.sysdir) {
            Ok(Some(chain)) => {
                return Verdict::Occupied(format!(
                    "{} is held by {} (evidence: sysfs holders/)",
                    device.name,
                    chain.join(" -> ")
                ))
            }
            Ok(None) => {}
            Err(detail) => return Verdict::Unknown(detail),
        }
    }
    match md_member_hit(&scope) {
        Ok(Some(detail)) => return Verdict::Occupied(detail),
        Ok(None) => {}
        Err(detail) => return Verdict::Unknown(detail),
    }
    Verdict::Idle
}

/// 镜像没有分区节点可查：证据 1、3、4 的对象都不存在，只剩交换文件与 loop backing
#[cfg(target_os = "linux")]
fn probe_image(meta: &std::fs::Metadata) -> Verdict {
    match swaps_hit(Target::Image(meta.dev(), meta.ino())) {
        Ok(Some(detail)) => return Verdict::Occupied(detail),
        Ok(None) => {}
        Err(detail) => return Verdict::Unknown(detail),
    }
    match image_loop_hit(meta) {
        Ok(Some(detail)) => Verdict::Occupied(detail),
        Ok(None) => Verdict::Idle,
        Err(detail) => Verdict::Unknown(detail),
    }
}

/// 目标的占用域。整盘与分区的区分取自 sysfs 的 `partition` 属性；dm/md 整设备按条
/// 退化为自身，不再展开分区
#[cfg(target_os = "linux")]
fn device_scope(rdev: u64) -> Result<Vec<ScopedDevice>, String> {
    let link = Path::new("/sys/dev/block")
        .join(format!("{}:{}", libc::major(rdev), libc::minor(rdev)));
    let sysdir = std::fs::canonicalize(&link).map_err(|e| {
        format!("cannot resolve {} to the device's sysfs directory: {e}", link.display())
    })?;
    let self_device = scoped_device(&sysdir)?;
    if is_dm_or_md_whole(&self_device.name) {
        return Ok(vec![self_device]);
    }
    let disk_dir = if has_partition_attr(&sysdir)? {
        sysdir
            .parent()
            .map(Path::to_path_buf)
            .ok_or_else(|| format!("{} has no parent directory", sysdir.display()))?
    } else {
        sysdir
    };
    let mut out = vec![scoped_device(&disk_dir)?];
    for part in partitions_of(&disk_dir)? {
        out.push(scoped_device(&part)?);
    }
    Ok(out)
}

/// `dm-<n>` / `md<n>` 这类整设备：分区名带 `p` 后缀（如 `dm-0p1`），不在此列
#[cfg(target_os = "linux")]
fn is_dm_or_md_whole(name: &str) -> bool {
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    name.strip_prefix("dm-").is_some_and(digits) || name.strip_prefix("md").is_some_and(digits)
}

#[cfg(target_os = "linux")]
fn scoped_device(dir: &Path) -> Result<ScopedDevice, String> {
    let name = dir
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| format!("{} has no usable device name", dir.display()))?
        .to_string();
    Ok(ScopedDevice { name, sysdir: dir.to_path_buf(), dev_no: dev_no_of(dir)? })
}

/// 设备的主次设备号取自 sysfs 的 `dev` 属性；读不出或格式不符按不可判定上抛
#[cfg(target_os = "linux")]
fn dev_no_of(dir: &Path) -> Result<(u64, u64), String> {
    let file = dir.join("dev");
    let text = std::fs::read_to_string(&file)
        .map_err(|e| format!("cannot read {}: {e}", file.display()))?;
    let (maj, min) = text
        .trim()
        .split_once(':')
        .ok_or_else(|| format!("{} is not a major:minor pair", file.display()))?;
    let maj = maj
        .parse::<u64>()
        .map_err(|e| format!("{} is not a major:minor pair: {e}", file.display()))?;
    let min = min
        .parse::<u64>()
        .map_err(|e| format!("{} is not a major:minor pair: {e}", file.display()))?;
    Ok((maj, min))
}

#[cfg(target_os = "linux")]
fn has_partition_attr(dir: &Path) -> Result<bool, String> {
    let attr = dir.join("partition");
    match std::fs::metadata(&attr) {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(format!("cannot stat {}: {e}", attr.display())),
    }
}

/// 整盘目录下带 `partition` 属性的子目录
#[cfg(target_os = "linux")]
fn partitions_of(disk_dir: &Path) -> Result<Vec<PathBuf>, String> {
    let entries = std::fs::read_dir(disk_dir)
        .map_err(|e| format!("cannot read {}: {e}", disk_dir.display()))?;
    let mut out = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| format!("cannot read {}: {e}", disk_dir.display()))?;
        let is_dir = entry
            .file_type()
            .map_err(|e| format!("cannot stat {}: {e}", entry.path().display()))?
            .is_dir();
        if is_dir && has_partition_attr(&entry.path())? {
            out.push(entry.path());
        }
    }
    Ok(out)
}

/// 证据 1：mountinfo 的 major:minor 命中域内任一设备
#[cfg(target_os = "linux")]
fn mounts_hit(scope: &[ScopedDevice]) -> Result<Option<String>, String> {
    let mounts = crate::fsops::read_mounts()
        .map_err(|e| format!("cannot read the mount table: {e}"))?;
    for device in scope {
        if let Some(entry) = mounts.iter().find(|m| m.dev_no == device.dev_no) {
            return Ok(Some(format!(
                "{} is mounted at {} (evidence: /proc/self/mountinfo)",
                device.name, entry.mount_point
            )));
        }
    }
    Ok(None)
}

/// 证据 2：`/proc/swaps` 的 Filename。块设备比 `rdev`，交换文件比 `(dev, ino)`
#[cfg(target_os = "linux")]
fn swaps_hit(target: Target) -> Result<Option<String>, String> {
    let path = Path::new("/proc/swaps");
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        // 没有 swap 子系统：已证明不存在，不是证据源缺失
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
    };
    for line in text.lines().skip(1) {
        let Some(field) = line.split_whitespace().next() else { continue };
        let meta = std::fs::metadata(field)
            .map_err(|e| format!("cannot stat the swap entry {field}: {e}"))?;
        let hit = match target {
            Target::Block(rdev) => meta.rdev() == rdev,
            Target::Image(dev, ino) => (meta.dev(), meta.ino()) == (dev, ino),
        };
        if hit {
            return Ok(Some(format!(
                "{field} is active swap and names this target (evidence: /proc/swaps)"
            )));
        }
    }
    Ok(None)
}

/// 证据 3：`holders/` 非空。首个非空层已决定结论，继续下钻只为把持有链写进文案；
/// 层数上限 8，按已访问目录防环
#[cfg(target_os = "linux")]
fn holder_chain(sysdir: &Path) -> Result<Option<Vec<String>>, String> {
    let mut chain = Vec::new();
    let mut current = sysdir.to_path_buf();
    let mut visited = vec![current.clone()];
    for _ in 0..8 {
        let names = read_holders(&current)?;
        if names.is_empty() {
            break;
        }
        chain.extend(names.iter().cloned());
        let Some(next) = names.first() else { break };
        let next_dir = Path::new("/sys/block").join(next);
        let Ok(real) = std::fs::canonicalize(&next_dir) else { break };
        if visited.contains(&real) {
            break;
        }
        visited.push(real.clone());
        current = real;
    }
    Ok(if chain.is_empty() { None } else { Some(chain) })
}

#[cfg(target_os = "linux")]
fn read_holders(sysdir: &Path) -> Result<Vec<String>, String> {
    let dir = sysdir.join("holders");
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(format!("cannot read {}: {e}", dir.display())),
    };
    let mut out = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| format!("cannot read {}: {e}", dir.display()))?;
        out.push(entry.file_name().to_string_lossy().into_owned());
    }
    Ok(out)
}

/// 证据 4：域内设备作为成员出现在任一 `/sys/block/md*/md/dev-*`
#[cfg(target_os = "linux")]
fn md_member_hit(scope: &[ScopedDevice]) -> Result<Option<String>, String> {
    let names: Vec<&str> = scope.iter().map(|d| d.name.as_str()).collect();
    let entries = match std::fs::read_dir("/sys/block") {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("cannot read /sys/block: {e}")),
    };
    for entry in entries {
        let entry = entry.map_err(|e| format!("cannot read /sys/block: {e}"))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.starts_with("md") {
            continue;
        }
        let is_dir = std::fs::metadata(entry.path())
            .map_err(|e| format!("cannot stat {}: {e}", entry.path().display()))?
            .is_dir();
        if !is_dir {
            continue;
        }
        let md_dir = entry.path().join("md");
        let members = match std::fs::read_dir(&md_dir) {
            Ok(members) => members,
            // 目录不在：这台 md 设备没有阵列侧成员表，不构成命中
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(format!("cannot read {}: {e}", md_dir.display())),
        };
        for member in members {
            let member = member.map_err(|e| format!("cannot read {}: {e}", md_dir.display()))?;
            let member = member.file_name().to_string_lossy().into_owned();
            let Some(member) = member.strip_prefix("dev-") else { continue };
            if names.contains(&member) {
                return Ok(Some(format!(
                    "{member} is a member of the md array {name} (evidence: {})",
                    md_dir.join(format!("dev-{member}")).display()
                )));
            }
        }
    }
    Ok(None)
}

/// 证据 5：任一 loop 的 `loop/backing_file` 还原转义后 `stat` 到与目标相同的
/// `(dev, ino)`。未绑定的 loop 没有该项，不算命中；读得出却 `stat` 不了按不可判定
#[cfg(target_os = "linux")]
fn image_loop_hit(meta: &std::fs::Metadata) -> Result<Option<String>, String> {
    let (dev, ino) = (meta.dev(), meta.ino());
    let entries = match std::fs::read_dir("/sys/block") {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("cannot read /sys/block: {e}")),
    };
    for entry in entries {
        let entry = entry.map_err(|e| format!("cannot read /sys/block: {e}"))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if !crate::fsops::is_loop_name(&name) {
            continue;
        }
        let attr = entry.path().join("loop/backing_file");
        let text = match std::fs::read_to_string(&attr) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(format!("cannot read {}: {e}", attr.display())),
        };
        let text = text.trim_end_matches(&['\n', '\r'][..]);
        if text.is_empty() {
            continue;
        }
        let decoded = crate::fsops::decode_octal(text);
        let path = decoded.strip_suffix(" (deleted)").unwrap_or(&decoded);
        let backing = std::fs::metadata(path)
            .map_err(|e| format!("cannot stat the loop backing file {path}: {e}"))?;
        if (backing.dev(), backing.ino()) == (dev, ino) {
            return Ok(Some(format!(
                "{name} maps this image as its backing file (evidence: {})",
                attr.display()
            )));
        }
    }
    Ok(None)
}
