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
//! 5. 目标为镜像时，经 loop 的反向身份比对（`fsops::mount` 的同一份机制）：身份取
//!    `LOOP_GET_STATUS64` 的 `(lo_device, lo_inode)`，打不开节点才回退 sysfs 路径的
//!    stat——backing 的路径表示会在镜像同路径重建后指向别的文件，不作为首选判据。
//!    命中后按挂载 / swap / 持有者 / 独占 claim 分层回答；裸 attached 而无人使用的
//!    loop 不算占用（内核独占权是判定"是否真有人用"的那一层）。
//!
//! 结论分三档：命中证据 → `refused`（3，请求与现状不匹配）；证据源存在却不可读、
//! 不可解析 → 确认失败，`no_mutation`（4，环境不给证据）；系统没有对应子系统是
//! 已证明不存在。loop 节点因权限打不开（非 root 对 root:disk 0660）按"与我无关"
//! 跳过——建不出绑定的一方不被节点权限拦住自己的镜像操作。只读命令不经过这里。

use super::*;

/// 占用判定结论。探测失败不走这里：`Err(String)` 单独成档，出口是 4
#[cfg(target_os = "linux")]
pub(crate) enum Verdict {
    Idle,
    /// 命中证据：文案已含设备名、证据源与解除动作
    Occupied(String),
}

/// 写命令准入前的可写性核验：只读块设备按 I/O 故障（4）拒绝，一个字节都没动。
/// 与占用探测同一时刻执行——两者都要求"在记录与写盘之前给出明确结论"
pub(crate) fn refuse_read_only_block(src: &FileSource) -> Result<(), crate::outcome::RunOutcome> {
    #[cfg(target_os = "linux")]
    if src.read_only_block() {
        return Err(crate::outcome::RunOutcome::no_mutation(
            "the target is a read-only block device (BLKROGET) — every write would be refused \
             by the kernel; nothing was modified",
        ));
    }
    #[cfg(not(target_os = "linux"))]
    let _ = src;
    Ok(())
}

/// 探测失败单列的占用闸：确认不了空闲按 I/O 故障（4）浮出，不与"确实在用"（3）
/// 混在一档——前者换个环境重试有意义，后者先解除占用才有意义
pub(crate) fn refuse_if_occupied_strict(src: &FileSource) -> Result<(), crate::outcome::RunOutcome> {
    #[cfg(not(target_os = "linux"))]
    let _ = src;
    #[cfg(target_os = "linux")]
    return match probe(src) {
        Ok(Verdict::Idle) => Ok(()),
        Ok(Verdict::Occupied(detail)) => Err(crate::outcome::RunOutcome::refused(format!(
            "{detail} — the target is in use; release it and retry \
             (`umount` / `swapoff` / `dmsetup remove` / `mdadm --stop` / `losetup -d`)"
        ))),
        Err(why) => Err(crate::outcome::RunOutcome::no_mutation(format!(
            "cannot confirm the target is unused: {why}"
        ))),
    };
    #[cfg(not(target_os = "linux"))]
    Ok(())
}

/// 占用闸的 dm 放行口径：活动 LV 经 dm 压着 PV 是 `--grow-lv` 链路的常态，
/// 链本身要在那个栈上工作。其余口径与 [`refuse_if_occupied_strict`] 相同
pub(crate) fn refuse_if_occupied_strict_dm(src: &FileSource) -> Result<(), crate::outcome::RunOutcome> {
    #[cfg(not(target_os = "linux"))]
    let _ = src;
    #[cfg(target_os = "linux")]
    return match probe_with(src, true) {
        Ok(Verdict::Idle) => Ok(()),
        Ok(Verdict::Occupied(detail)) => Err(crate::outcome::RunOutcome::refused(format!(
            "{detail} — the target is in use; release it and retry \
             (`umount` / `swapoff` / `dmsetup remove` / `mdadm --stop` / `losetup -d`)"
        ))),
        Err(why) => Err(crate::outcome::RunOutcome::no_mutation(format!(
            "cannot confirm the target is unused: {why}"
        ))),
    };
    #[cfg(not(target_os = "linux"))]
    Ok(())
}

/// 只读命令的事实入口：报出 [`probe`] 的结论，不做准入判定，也不影响退出码。
/// 与准入闸共用同一份探测，命中时给出首个证据
#[cfg(target_os = "linux")]
pub(crate) fn describe(src: &FileSource) -> (&'static str, Option<String>) {
    match probe(src) {
        Ok(Verdict::Idle) => ("idle", None),
        Ok(Verdict::Occupied(detail)) => ("occupied", Some(detail)),
        Err(why) => ("undecidable", Some(why)),
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
fn probe(src: &FileSource) -> Result<Verdict, String> {
    probe_with(src, false)
}

/// `allow_dm_held` 只在镜像路径生效：dm 持有的 loop 按空闲放行（LVM 链要经它
/// 工作），其余持有者照常上报
#[cfg(target_os = "linux")]
fn probe_with(src: &FileSource, allow_dm_held: bool) -> Result<Verdict, String> {
    let meta = match src.metadata() {
        Ok(meta) => meta,
        Err(e) => return Err(format!("cannot stat the open target: {e}")),
    };
    if !src.is_block {
        return probe_image(&meta, allow_dm_held);
    }
    let scope = device_scope(meta.rdev())?;
    match mounts_hit(&scope) {
        Ok(Some(detail)) => return Ok(Verdict::Occupied(detail)),
        Ok(None) => {}
        Err(detail) => return Err(detail),
    }
    match swaps_hit(Target::Block(meta.rdev())) {
        Ok(Some(detail)) => return Ok(Verdict::Occupied(detail)),
        Ok(None) => {}
        Err(detail) => return Err(detail),
    }
    for device in &scope {
        match holder_chain(&device.sysdir) {
            Ok(Some(chain)) => {
                return Ok(Verdict::Occupied(format!(
                    "{} is held by {} (evidence: sysfs holders/)",
                    device.name,
                    chain.join(" -> ")
                )))
            }
            Ok(None) => {}
            Err(detail) => return Err(detail),
        }
    }
    match md_member_hit(&scope) {
        Ok(Some(detail)) => return Ok(Verdict::Occupied(detail)),
        Ok(None) => {}
        Err(detail) => return Err(detail),
    }
    Ok(Verdict::Idle)
}

/// 镜像没有分区节点可查：证据 1、3、4 的对象都不存在，只剩交换文件与经 loop 的
/// 反向身份比对（身份与分层判定都在 `fsops::mount`，准入与 FS 步闸共用一份）。
/// dm 持有的 loop 在这里放行与否由调用方的口径决定；镜像准入不带 `--grow-lv`
/// 时不放行，镜像被 dm 栈压着改表仍是占用
#[cfg(target_os = "linux")]
fn probe_image(meta: &std::fs::Metadata, allow_dm_held: bool) -> Result<Verdict, String> {
    match swaps_hit(Target::Image(meta.dev(), meta.ino())) {
        Ok(Some(detail)) => return Ok(Verdict::Occupied(detail)),
        Ok(None) => {}
        Err(detail) => return Err(detail),
    }
    let mounts = crate::fsops::mount::read_mounts().map_err(|e| e.to_string())?;
    let swaps = std::fs::read_to_string("/proc/swaps")
        .map_err(|e| format!("cannot read /proc/swaps: {e}"))?;
    match crate::fsops::mount::loop_occupancy_for(
        crate::fsops::mount::BackingMatch::File { dev: meta.dev(), ino: meta.ino() },
        &mounts,
        &swaps,
        crate::fsops::mount::OccupancyView {
            dm_held_is_idle: allow_dm_held,
            unopenable_is_skipped: true,
        },
    ) {
        Ok(None) => Ok(Verdict::Idle),
        Ok(Some(crate::fsops::mount::LoopOccupancy::Live(path))) => Ok(Verdict::Occupied(format!(
            "{path} is mounted on this image or carries an active swap"
        ))),
        Ok(Some(crate::fsops::mount::LoopOccupancy::Held { loopdev, holders })) => {
            Ok(Verdict::Occupied(format!(
                "{loopdev} is held by {} (evidence: sysfs holders/)",
                holders.join(" -> ")
            )))
        }
        Ok(Some(crate::fsops::mount::LoopOccupancy::KernelBusy { loopdev })) => {
            Ok(Verdict::Occupied(format!(
                "{loopdev} is claimed by another user — the kernel refuses an exclusive open (EBUSY)"
            )))
        }
        Ok(Some(crate::fsops::mount::LoopOccupancy::Unclaimable { loopdev, why })) => Err(format!(
            "cannot take an exclusive claim on {loopdev}, a loop onto this image: {why}"
        )),
        Err(e) => Err(e.to_string()),
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
#[cfg(all(test, target_os = "linux"))]
mod image_probe {
    use super::{probe_image, Verdict};

    /// 没有任何 loop 关联的普通文件是空闲：五类证据全部"已证明不存在"。
    /// 判错方向只允许把空闲判成占用，不允许反过来——这条向量钉住后者。
    #[test]
    fn a_plain_file_with_no_loop_backing_is_idle() {
        let dir = tempfile::Builder::new().prefix("diskedit-occ-").tempdir().unwrap();
        let img = dir.path().join("disk.img");
        std::fs::write(&img, vec![0u8; 64 * 1024]).unwrap();
        let meta = std::fs::metadata(&img).unwrap();
        assert!(matches!(probe_image(&meta, false), Ok(Verdict::Idle)));
    }
}
