//! 记录准入与 FS 步闸共用的占用判定：先收集事实（[`OccupancyFacts`]），再由唯一入口
//! [`verdict`] 回答"这块盘能不能动"。
//!
//! 作用域是**占用域**：整盘取该盘自身与它下面全部分区；分区取父盘与父盘全部分区；
//! dm/md 没有分区子目录，退化为设备自身。对域内每台设备收集六类事实，任一命中即为
//! 占用：
//!
//! 1. `mounts`：mountinfo 的 major:minor、source 路径，以及匿名设备号挂载的 stat
//!    补偿（btrfs 成员挂载时字段 3 报 anon bdev，与成员分区的 `st_rdev` 必不相同）；
//! 2. `swaps`：`/proc/swaps` 的 Filename（块设备比 `rdev`，交换文件比 `(dev, ino)`）；
//! 3. `holders`：sysfs `holders/` 非空（首层非空即命中；下钻是为了收集整棵子树里的
//!    全部持有者——`Extend` 的放行要求集合全是 `dm-*`，漏掉兄弟分支会把在用的栈判成
//!    空闲；至多 8 层）；
//! 4. `loops`：本目标作为 loop backing 且该 loop 在用（挂载 / swap / 持有 / 被独占
//!    claim）。身份取 `LOOP_GET_STATUS64` 的 `(lo_device, lo_inode)`，打不开节点才
//!    回退 sysfs 路径的 stat——backing 的路径表示会在同路径重建后指向别的文件，
//!    不作为首选判据。裸 attached 而无人使用的 loop 不算占用（内核独占权是判定
//!    "是否真有人用"的那一层）；
//! 5. `btrfs_members`：目标 superblock 顶层 `num_devices > 1`，即多设备 btrfs 的成员；
//! 6. `stack_members`：设备作为成员出现在任一 `/sys/block/md*/md/dev-*`。
//!
//! 结论只有"空闲"与"占用"两档；探测失败（证据源存在却不可读、loop 不能取独占权）
//! 单独成档 `Err`，出口是 4——确认不了空闲不得当成空闲。只读命令不经过准入闸，
//! `describe` 只把同一份结论报出来。没有这些证据源的平台（非 Linux 的块设备）在准入闸
//! 直接按平台不支持（6）拒绝，不让"没探测"冒充"空闲"；镜像上没有 mount/loop 子系统可
//! 占用，属"已证明不存在"，不是"未探测"。
//!
//! 判定形状（[`OpShape`]）决定两件事：`Extend` 只增不动已映射扇区，栈顶继续指向旧字节
//! 仍成立，dm 持有因此放行；`Overwrite` 会让栈顶指向被改写的字节，任何持有者都拦。多设备
//! 成员关系只在 `Overwrite` 时拦——扩容成员分区正是 `btrfs device resize` /
//! `mdadm --grow` 的正常前提。

use super::*;

/// 占用判定结论。探测失败不走这里：`Err(String)` 单独成档，出口是 4
#[cfg(target_os = "linux")]
pub(crate) enum Verdict {
    Idle,
    /// 命中证据：文案已含设备名、证据源与解除动作
    Occupied(String),
}

/// 占用事实的类别。出口码只由本枚举决定：一个事实类对应一个码值，文案留在调用点
/// （同一事实类在不同入口要说的解除动作不同）。调用点不得再按命令名或状态名分叉。
///
/// 变体按平台裁剪：非 Linux 没有 mountinfo / sysfs / loop，占用与探测失败在这些平台上
/// 不可能被构造，唯一的事实类就是"平台不可探测"
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OccupancyFact {
    /// 确定被占用：挂载 / 活动 swap / 块设备栈持有 / 多设备成员
    #[cfg(target_os = "linux")]
    Occupied,
    /// 占用探测失败：证据源存在却读不出，无法确认空闲
    #[cfg(target_os = "linux")]
    Unreadable,
    /// 指向目标的 loop 在用：挂载 / swap / 栈持有 / 内核拒绝独占打开
    #[cfg(target_os = "linux")]
    LoopOccupied,
    /// loop 无法取独占权：无法确认它在不在用
    #[cfg(target_os = "linux")]
    LoopUnreadable,
    /// 平台没有占用探测能力
    #[cfg(not(target_os = "linux"))]
    Unprobeable,
}

impl OccupancyFact {
    /// 唯一的映射点：事实类 → FS 层错误分类。占用 = 参数/现状不符（`InvalidArgument`），
    /// 读不到 = 环境故障（`Io`），平台不可探测 = `UnsupportedPlatform`。
    /// 出口码不在这里再写一遍——`From<FsError> for RunOutcome` 已把这三类固定为 3/4/6
    pub(crate) fn fs_error(self, msg: impl Into<String>) -> crate::fsops::FsError {
        use crate::fsops::FsError;
        let msg = msg.into();
        match self {
            #[cfg(target_os = "linux")]
            Self::Occupied => FsError::invalid(msg),
            #[cfg(target_os = "linux")]
            Self::LoopOccupied => FsError::invalid(msg),
            #[cfg(target_os = "linux")]
            Self::Unreadable => FsError::Io(io::Error::other(msg)),
            #[cfg(target_os = "linux")]
            Self::LoopUnreadable => FsError::Io(io::Error::other(msg)),
            #[cfg(not(target_os = "linux"))]
            Self::Unprobeable => FsError::platform(msg),
        }
    }

    /// 占用闸（`RunOutcome` 域）的出口：同一张表的转换结果
    pub(crate) fn outcome(self, msg: impl Into<String>) -> crate::outcome::RunOutcome {
        self.fs_error(msg).into()
    }
}

/// 写命令准入前的可写性核验：只读块设备按 I/O 故障（4）拒绝，一个字节都没动。
/// 与占用探测同一时刻执行——两者都要求"在记录与写盘之前给出明确结论"
pub(crate) fn refuse_read_only_block(src: &FileSource) -> Result<(), crate::outcome::RunOutcome> {
    #[cfg(target_os = "linux")]
    if src.read_only_block() {
        return Err(crate::outcome::RunOutcome::no_mutation(
            "the target is a read-only block device (BLKROGET) — every write would be refused \
             by the kernel",
        ));
    }
    #[cfg(not(target_os = "linux"))]
    let _ = src;
    Ok(())
}

/// 记录准入的占用闸。`shape` 决定 dm 持有是否放行：`Extend` 只增不动已映射扇区，
/// 活动 LV 经 dm 压着 PV 是 `--grow-lv` 链路的常态；`Overwrite` 全拦。
/// 出口码只由 [`OccupancyFact`] 决定——占用 3、探测失败 4、平台不可探测 6
#[cfg(target_os = "linux")]
pub(crate) fn refuse_if_occupied(src: &FileSource, shape: OpShape) -> Result<(), crate::outcome::RunOutcome> {
    match probe_shape(src, shape) {
        Ok(Verdict::Idle) => Ok(()),
        Ok(Verdict::Occupied(detail)) => Err(OccupancyFact::Occupied.outcome(format!(
            "{detail} — the target is in use; release it and retry \
             (`umount` / `swapoff` / `dmsetup remove` / `mdadm --stop` / `losetup -d`)"
        ))),
        Err(why) => Err(OccupancyFact::Unreadable.outcome(format!(
            "cannot confirm the target is unused: {why}"
        ))),
    }
}

/// 非 Linux 没有 mountinfo / sysfs / loop：块设备无从取证，按"平台不支持"（6）拒绝，
/// 不让"没探测"冒充"空闲"；镜像不存在这些占用来源，是"已证明不存在"，不是"未探测"
#[cfg(not(target_os = "linux"))]
pub(crate) fn refuse_if_occupied(src: &FileSource, _shape: OpShape) -> Result<(), crate::outcome::RunOutcome> {
    unprobed(src)
}

/// 非 Linux 没有 mountinfo / sysfs / loop：块设备无从取证，按"平台不支持"（6）拒绝，
/// 不让"没探测"冒充"空闲"；镜像不存在这些占用来源，是"已证明不存在"，不是"未探测"
#[cfg(not(target_os = "linux"))]
fn unprobed(src: &FileSource) -> Result<(), crate::outcome::RunOutcome> {
    if src.is_block {
        return Err(OccupancyFact::Unprobeable.outcome(format!(
            "occupancy of {} cannot be probed on this platform — the target cannot be confirmed \
             unused",
            src.path.display()
        )));
    }
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

/// 判定形状。只影响 dm/md 持有：`Extend` 只增不动已映射扇区，栈顶继续指向旧字节
/// 仍成立；`Overwrite` 会让栈顶指向被改写的字节，任何持有者都拦。
/// 非 Linux 上没有任何探测，形状只作为调用点的语义标签传给平台桩
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OpShape {
    Extend,
    Overwrite,
}

#[cfg(target_os = "linux")]
impl From<crate::fsops::mount::WriteKind> for OpShape {
    fn from(kind: crate::fsops::mount::WriteKind) -> Self {
        match kind {
            crate::fsops::mount::WriteKind::Extend => Self::Extend,
            crate::fsops::mount::WriteKind::Overwrite => Self::Overwrite,
        }
    }
}

/// 占用判定的事实集合。每项只装"确实命中本目标"的证据；"不存在"与"读不出来"
/// 的区分由收集阶段的 `Err` 承担
#[cfg(target_os = "linux")]
pub(crate) struct OccupancyFacts {
    pub(crate) mounts: Vec<MountRef>,
    pub(crate) swaps: Vec<SwapRef>,
    pub(crate) holders: Vec<HolderRef>,
    pub(crate) loops: Vec<LoopRef>,
    /// 多设备文件系统的成员（判据见 [`btrfs_members`]）
    pub(crate) btrfs_members: Vec<MemberRef>,
    /// 块设备栈的成员关系：设备出现在某台 md 的 `/sys/block/md*/md/dev-*` 下。
    /// 不是 LVM2 PV 标签解析——PV 与 LV 的关系由 `holders/` 表达，`--grow-lv`
    /// 链路要按持有者放行
    pub(crate) stack_members: Vec<MemberRef>,
}

#[cfg(target_os = "linux")]
pub(crate) struct MountRef {
    pub(crate) device: String,
    pub(crate) mount_point: String,
}

#[cfg(target_os = "linux")]
pub(crate) struct SwapRef {
    pub(crate) entry: String,
}

#[cfg(target_os = "linux")]
pub(crate) struct HolderRef {
    pub(crate) device: String,
    /// 从该设备出发、在 sysfs `holders/` 子树里能到达的全部持有者。
    /// 是一个集合而不是一条路径：判定问「有没有非 dm 的栈」，
    /// 与它挂在哪一层、走哪条分支无关
    pub(crate) holders: Vec<String>,
}

/// 成员事实的完整句子：含设备名、所属文件系统/阵列与解除动作
#[cfg(target_os = "linux")]
pub(crate) struct MemberRef {
    pub(crate) detail: String,
}

#[cfg(target_os = "linux")]
pub(crate) enum LoopRef {
    /// loop 本身或它的分区正在挂载 / 做 swap
    Live(String),
    /// loop 之上有 dm/md 栈
    Held { loopdev: String, holders: Vec<String> },
    /// 内核拒绝独占打开：有使用者，但分不清是谁
    KernelBusy(String),
}

/// 唯一判定入口。顺序即证据的优先级：挂载与 swap 是"确定在用"，持有者与 loop 次之，
/// 多设备成员说明"单独改写不安全"
#[cfg(target_os = "linux")]
pub(crate) fn verdict(shape: OpShape, facts: &OccupancyFacts) -> crate::fsops::mount::Occupancy {
    use crate::fsops::mount::{dm_only, Occupancy};
    if !facts.mounts.is_empty() {
        return Occupancy::Mounted;
    }
    if !facts.swaps.is_empty() {
        return Occupancy::ActiveSwap;
    }
    let dm_released = |holders: &[String]| shape == OpShape::Extend && dm_only(holders);
    // 放行条件是「全部持有者都是 dm」，因此必须看完整集合：任一设备的持有集合里
    // 出现非 dm 栈即为占用，不能因为集合里 dm 排在前面就放行
    if let Some(held) = facts.holders.iter().find(|h| !dm_released(&h.holders)) {
        return Occupancy::Held(held.holders.clone());
    }
    for loop_ref in &facts.loops {
        match loop_ref {
            LoopRef::Live(via) => {
                return Occupancy::LoopBacked { via: via.clone(), holders: Vec::new() }
            }
            LoopRef::Held { loopdev, holders } if !dm_released(holders) => {
                return Occupancy::LoopBacked { via: loopdev.clone(), holders: holders.clone() }
            }
            LoopRef::Held { .. } => {}
            LoopRef::KernelBusy(via) => {
                return Occupancy::LoopBacked { via: via.clone(), holders: Vec::new() }
            }
        }
    }
    // 成员关系只挡整段重写：`Extend` 只动末端、不改成员自身的元数据，而扩容成员的分区
    // 正是 `btrfs device resize` / `mdadm --grow` 的正常前提
    if let Some(member) = facts
        .btrfs_members
        .iter()
        .chain(facts.stack_members.iter())
        .next()
        .filter(|_| shape == OpShape::Overwrite)
    {
        return Occupancy::Member { detail: member.detail.clone() };
    }
    Occupancy::Idle
}

/// 目标形态：块设备带占用域，镜像只有文件身份
#[cfg(target_os = "linux")]
enum Subject<'a> {
    Block { path: &'a Path, rdev: u64, scope: Vec<ScopedDevice> },
    Image { dev: u64, ino: u64 },
}

/// 一次读好的证据源。整盘逐分区判定时只读一次 /proc
#[cfg(target_os = "linux")]
struct Tables {
    mounts: Vec<crate::fsops::mount::MountEntry>,
    swaps: String,
}

/// `/proc/swaps` 缺失**不能读成"没有 swap"**：这个文件由 procfs 提供，缺了说明 procfs
/// 整体不可用，此刻连设备清单都取证不了。删掉这一行不是"已证明不存在"，而是把证据缺失
/// 伪装成空闲——写路径会据此放行
#[cfg(target_os = "linux")]
fn read_tables() -> Result<Tables, String> {
    let mounts = crate::fsops::read_mounts()
        .map_err(|e| format!("cannot read the mount table: {e}"))?;
    let swaps = std::fs::read_to_string("/proc/swaps")
        .map_err(|e| format!("cannot read /proc/swaps: {e}"))?;
    Ok(Tables { mounts, swaps })
}

#[cfg(target_os = "linux")]
fn probe(src: &FileSource) -> Result<Verdict, String> {
    probe_shape(src, OpShape::Overwrite)
}

#[cfg(target_os = "linux")]
fn probe_shape(src: &FileSource, shape: OpShape) -> Result<Verdict, String> {
    let facts = collect(src)?;
    match verdict(shape, &facts) {
        crate::fsops::mount::Occupancy::Idle => Ok(Verdict::Idle),
        occupied => Ok(Verdict::Occupied(describe_occupied(&facts, &occupied))),
    }
}

/// 把判定结论写成一句含证据来源的话：拒绝文案是用户解除占用的唯一线索
#[cfg(target_os = "linux")]
fn describe_occupied(facts: &OccupancyFacts, occupied: &crate::fsops::mount::Occupancy) -> String {
    use crate::fsops::mount::Occupancy;
    match occupied {
        Occupancy::Mounted => match facts.mounts.first() {
            Some(m) => format!("{} is mounted at {} (evidence: mountinfo)", m.device, m.mount_point),
            None => "the target is mounted (evidence: mountinfo)".to_string(),
        },
        Occupancy::ActiveSwap => match facts.swaps.first() {
            Some(s) => format!("{} is active swap (evidence: /proc/swaps)", s.entry),
            None => "the target is active swap (evidence: /proc/swaps)".to_string(),
        },
        Occupancy::Held(holders) => match facts.holders.iter().find(|h| h.holders == *holders) {
            Some(h) => format!(
                "{} is held by {} (evidence: sysfs holders/)",
                h.device,
                holders.join(", ")
            ),
            None => format!("the target is held by {} (evidence: sysfs holders/)", holders.join(", ")),
        },
        Occupancy::LoopBacked { via, holders } => match holders.is_empty() {
            true => format!(
                "{via} is a loop onto the target and is in use (mounted, active swap, or claimed by another stack)"
            ),
            false => format!("{via} is a loop onto the target and is held by {}", holders.join(" -> ")),
        },
        Occupancy::Member { detail } => detail.clone(),
        Occupancy::Idle => String::new(),
    }
}

/// 记录准入与只读报告共用：目标句柄一个，占用域由设备号推出
#[cfg(target_os = "linux")]
pub(crate) fn collect(src: &FileSource) -> Result<OccupancyFacts, String> {
    let meta = src
        .metadata()
        .map_err(|e| format!("cannot stat the open target: {e}"))?;
    let tables = read_tables()?;
    if !src.is_block {
        return gather(&tables, &Subject::Image { dev: meta.dev(), ino: meta.ino() }, src);
    }
    let scope = device_scope(meta.rdev())?;
    gather(
        &tables,
        &Subject::Block { path: src.path.as_path(), rdev: meta.rdev(), scope },
        src,
    )
}

/// 单节点取证：记录准入与 FS 步闸的单点判定
#[cfg(target_os = "linux")]
pub(crate) fn collect_node(dev: &str) -> Result<OccupancyFacts, String> {
    let tables = read_tables()?;
    gather_node(&tables, dev)
}

/// 多节点取证：整盘逐分区判定时证据源只读一遍
#[cfg(target_os = "linux")]
pub(crate) fn collect_nodes(nodes: &[String]) -> Result<Vec<OccupancyFacts>, String> {
    let tables = read_tables()?;
    nodes.iter().map(|dev| gather_node(&tables, dev)).collect()
}

#[cfg(target_os = "linux")]
fn gather_node(tables: &Tables, dev: &str) -> Result<OccupancyFacts, String> {
    let path = Path::new(dev);
    let meta = std::fs::metadata(path)
        .map_err(|e| format!("cannot stat {dev} to confirm it is unused: {e}"))?;
    let rdev = meta.rdev();
    let scope = vec![scoped_self(rdev)?];
    // 成员判据要读目标自身的超级块；只在这一步打开，不改变节点状态
    let src = FileSource::open_read_only(path)
        .map_err(|e| format!("cannot open {dev} to read its filesystem superblock: {e}"))?;
    gather(tables, &Subject::Block { path, rdev, scope }, &src)
}

#[cfg(target_os = "linux")]
fn gather(tables: &Tables, subject: &Subject, src: &FileSource) -> Result<OccupancyFacts, String> {
    let scope: &[ScopedDevice] = match subject {
        Subject::Block { scope, .. } => scope,
        Subject::Image { .. } => &[],
    };
    Ok(OccupancyFacts {
        mounts: mounts_of(tables, subject),
        swaps: swaps_of(tables, subject)?,
        holders: holders_of(scope)?,
        loops: loops_of(tables, subject)?,
        btrfs_members: member_facts(src)?,
        stack_members: md_members_of(scope)?,
    })
}

/// 目标的占用域。整盘与分区的区分取自 sysfs 的 `partition` 属性；dm/md 整设备按条
/// 退化为自身，不展开分区
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

/// 只认这一台设备本身（不展开父盘与兄弟分区）：FS 步闸按节点判定，
/// 兄弟分区在用不代表这台在用
#[cfg(target_os = "linux")]
fn scoped_self(rdev: u64) -> Result<ScopedDevice, String> {
    let link = Path::new("/sys/dev/block")
        .join(format!("{}:{}", libc::major(rdev), libc::minor(rdev)));
    let sysdir = std::fs::canonicalize(&link).map_err(|e| {
        format!("cannot resolve {} to the device's sysfs directory: {e}", link.display())
    })?;
    scoped_device(&sysdir)
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

/// 证据 1：mountinfo 的 major:minor、source 路径，或匿名设备号的 stat 补偿
#[cfg(target_os = "linux")]
fn mounts_of(tables: &Tables, subject: &Subject) -> Vec<MountRef> {
    let Subject::Block { path, scope, .. } = subject else { return Vec::new() };
    let given = path.to_string_lossy().into_owned();
    let mut out = Vec::new();
    for device in scope {
        for entry in &tables.mounts {
            let hit = entry.dev_no == device.dev_no
                || entry.source.as_str() == given
                || (entry.dev_no.0 == 0
                    && crate::fsops::mount::source_is_device(&entry.source, device.dev_no));
            if hit {
                out.push(MountRef {
                    device: device.name.clone(),
                    mount_point: entry.mount_point.clone(),
                });
            }
        }
    }
    out
}

/// 证据 2：`/proc/swaps` 的 Filename。块设备比 `rdev`，交换文件比 `(dev, ino)`
#[cfg(target_os = "linux")]
fn swaps_of(tables: &Tables, subject: &Subject) -> Result<Vec<SwapRef>, String> {
    let given = match subject {
        Subject::Block { path, .. } => path.to_string_lossy().into_owned(),
        Subject::Image { .. } => String::new(),
    };
    let mut out = Vec::new();
    for line in tables.swaps.lines().skip(1) {
        let Some(field) = line.split_whitespace().next() else { continue };
        if field == given {
            out.push(SwapRef { entry: field.to_string() });
            continue;
        }
        let meta = std::fs::metadata(field)
            .map_err(|e| format!("cannot stat the swap entry {field}: {e}"))?;
        let hit = match subject {
            Subject::Block { scope, .. } => scope
                .iter()
                .any(|d| meta.rdev() == libc::makedev(d.dev_no.0 as u32, d.dev_no.1 as u32)),
            Subject::Image { dev, ino } => (meta.dev(), meta.ino()) == (*dev, *ino),
        };
        if hit {
            out.push(SwapRef { entry: field.to_string() });
        }
    }
    Ok(out)
}

/// 证据 3：`holders/` 非空。每台设备各自成一个集合，首层非空即命中；
/// 下钻是为了把整棵子树里的持有者都收进来（层数上限 8，按已访问目录防环）
#[cfg(target_os = "linux")]
fn holders_of(scope: &[ScopedDevice]) -> Result<Vec<HolderRef>, String> {
    let mut out = Vec::new();
    for device in scope {
        if let Some(holders) = holder_chain(&device.sysdir)? {
            out.push(HolderRef { device: device.name.clone(), holders });
        }
    }
    Ok(out)
}

/// 证据 4：经 loop 的反向身份比对。dm 放行由 [`verdict`] 统一做，这里只报原始事实
#[cfg(target_os = "linux")]
fn loops_of(tables: &Tables, subject: &Subject) -> Result<Vec<LoopRef>, String> {
    use crate::fsops::mount::{loop_occupancy_for, BackingMatch, LoopOccupancy};
    let target = match subject {
        Subject::Block { rdev, .. } => BackingMatch::Block { rdev: *rdev },
        Subject::Image { dev, ino } => BackingMatch::File { dev: *dev, ino: *ino },
    };
    match loop_occupancy_for(target, &tables.mounts, &tables.swaps)
        .map_err(|e| e.to_string())?
    {
        None => Ok(Vec::new()),
        Some(LoopOccupancy::Live(via)) => Ok(vec![LoopRef::Live(via)]),
        Some(LoopOccupancy::Held { loopdev, holders }) => {
            Ok(vec![LoopRef::Held { loopdev, holders }])
        }
        Some(LoopOccupancy::KernelBusy { loopdev }) => Ok(vec![LoopRef::KernelBusy(loopdev)]),
        Some(LoopOccupancy::Unclaimable { loopdev, why }) => Err(format!(
            "cannot take an exclusive claim on {loopdev}, a loop onto the target: {why}"
        )),
    }
}

/// 证据 5：多设备文件系统的成员。判据来自目标自身的 superblock，未挂载也成立
#[cfg(target_os = "linux")]
fn member_facts(src: &FileSource) -> Result<Vec<MemberRef>, String> {
    let probe = crate::fsid::probe_fs(src, 0, src.size)
        .map_err(|e| format!("cannot probe the filesystem on the target: {e}"))?;
    let Some(kind) = probe.content.filesystem() else { return Ok(Vec::new()) };
    if !spans_devices(kind) {
        return Ok(Vec::new());
    }
    if kind == crate::domain::FsKind::Btrfs {
        return btrfs_members(src);
    }
    Ok(Vec::new())
}

/// 跨设备成员语义。新增 `FsKind` 变体时这里必须逐个表态——穷尽匹配，没有 `_`
#[cfg(target_os = "linux")]
fn spans_devices(kind: crate::domain::FsKind) -> bool {
    use crate::domain::FsKind;
    match kind {
        FsKind::Btrfs => true,
        FsKind::Squashfs
        | FsKind::Erofs
        | FsKind::Ext
        | FsKind::Xfs
        | FsKind::F2fs
        | FsKind::Exfat
        | FsKind::Vfat
        | FsKind::Ntfs
        | FsKind::HfsPlus
        | FsKind::Apfs => false,
    }
}

/// btrfs 多设备判据：超级块在目标起点 + 0x10000，magic 在超级块内 +0x40，顶层
/// `num_devices` 在 +0x88（`__le64`）。下表偏移一律**从超级块起点计**；与目标长度的
/// 比较先把偏移折成「绝对字节位置」，单位不与切片长度混用。
///
/// 出处：内核 `fs/btrfs/fs.h`（`BTRFS_SUPER_INFO_OFFSET
/// SZ_64K`、`sizeof(struct btrfs_super_block) == 4096`）、内核
/// `include/uapi/linux/btrfs_tree.h` 与 btrfs-progs `kernel-shared/uapi/btrfs_tree.h`
/// 的 `struct btrfs_super_block` 字段序（`fsid` @0x20、`magic` @0x40、
/// `root_dir_objectid` @0x80、`num_devices` @0x88）、btrfs 官方 On-disk Format
/// §Superblock 的 (Offset, Size, Field) 表。magic 的比对依据只有
/// [`crate::fsid::BTRFS_MAGIC`] 一份。`num_devices > 1` 说明这块设备只是成员
/// 之一：未挂载时它既不在 mountinfo 里、也没有持有者，但同组其它设备仍引用它的元数据，
/// 单独重写会毁掉整个文件系统。
///
/// 「是不是 btrfs」的判据在 [`crate::fsid::probe_fs`]：调用方 [`member_facts`] 只在它
/// 已由 magic 认出 Btrfs 之后才进来，所以本函数的 magic 比对是防线、不是判据——两次读
/// 之间内容可能变化，比对不符按「不是 btrfs」返回空事实，与识别侧的结论不冲突。
///
/// 取不到时分两种：magic 不符是「已证明不是 btrfs」，返回空事实；magic 相符而
/// `num_devices` 落在目标之外（目标过小）或读失败是「无法证明」，返回 `Err`，
/// 由调用方按未知拒绝——多设备 btrfs 的成员不能被当成单设备放行。
#[cfg(target_os = "linux")]
fn btrfs_members(src: &FileSource) -> Result<Vec<MemberRef>, String> {
    const SUPER_OFFSET: u64 = 0x10000;
    const MAGIC_OFFSET: usize = 0x40;
    const NUM_DEVICES_OFFSET: usize = 0x88;
    // 两个字段末字节的绝对位置：magic @0x10040..0x10048，num_devices @0x10088..0x10090
    const MAGIC_END: u64 = SUPER_OFFSET + MAGIC_OFFSET as u64 + 8;
    const NUM_DEVICES_END: u64 = SUPER_OFFSET + NUM_DEVICES_OFFSET as u64 + 8;
    // 连 magic 都读不到时没有任何证据能排除 btrfs，按未知返回
    if src.size < MAGIC_END {
        return Err(format!(
            "the target is {} bytes long, ending before byte {MAGIC_END} — too short to hold a \
             btrfs superblock magic, so a multi-device btrfs member cannot be ruled out",
            src.size
        ));
    }
    let mut sb = [0u8; NUM_DEVICES_OFFSET + 8];
    // 只读目标覆盖得到的那一段：magic 一定在内，num_devices 不一定
    let covered = (src.size - SUPER_OFFSET).min(sb.len() as u64) as usize;
    src.read_at(SUPER_OFFSET, &mut sb[..covered])
        .map_err(|e| format!("cannot read the btrfs superblock: {e}"))?;
    let magic = &sb[MAGIC_OFFSET..MAGIC_OFFSET + 8];
    if *magic != crate::fsid::BTRFS_MAGIC {
        return Ok(Vec::new());
    }
    // magic 命中而超级块头在 num_devices 之前结束：成员数无法读出，按未知返回
    if covered < sb.len() {
        return Err(format!(
            "the target is {} bytes long, ending before byte {NUM_DEVICES_END} — the btrfs \
             superblock magic is present but its num_devices field cannot be read",
            src.size
        ));
    }
    let mut raw = [0u8; 8];
    raw.copy_from_slice(&sb[NUM_DEVICES_OFFSET..NUM_DEVICES_OFFSET + 8]);
    let devices = u64::from_le_bytes(raw);
    if devices <= 1 {
        return Ok(Vec::new());
    }
    let fsid = crate::support::hex_lower(&sb[0x20..0x30]);
    Ok(vec![MemberRef {
        detail: format!(
            "the target is one of {devices} devices of the btrfs filesystem {fsid} — rewriting it \
             alone would destroy metadata the other members still reference; use `btrfs device` \
             commands instead"
        ),
    }])
}

/// 证据 6：域内设备作为成员出现在任一 `/sys/block/md*/md/dev-*`
///
/// `/sys/block` 缺失**不能读成"没有 md 成员"**：没有 sysfs 就连设备清单都没有，
/// 成员关系无从采集。删掉这一行不是"已证明不存在"，而是把证据缺失伪装成空闲
#[cfg(target_os = "linux")]
fn md_members_of(scope: &[ScopedDevice]) -> Result<Vec<MemberRef>, String> {
    let names: Vec<&str> = scope.iter().map(|d| d.name.as_str()).collect();
    let entries = std::fs::read_dir("/sys/block")
        .map_err(|e| format!("cannot read /sys/block: {e}"))?;
    let mut out = Vec::new();
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
                out.push(MemberRef {
                    detail: format!(
                        "{member} is a member of the md array {name} — deactivate the array first \
                         (evidence: {})",
                        md_dir.join(format!("dev-{member}")).display()
                    ),
                });
            }
        }
    }
    Ok(out)
}

/// 证据 3 的辅助：从一台设备出发，收集 `holders/` 子树里能到达的全部持有者。
/// 按层展开所有分支，不只沿第一个持有者下钻——`holders/` 是树，只走首条会让
/// 兄弟分支的子树落在判定之外。层数上限 8，按已访问目录防环
#[cfg(target_os = "linux")]
fn holder_chain(sysdir: &Path) -> Result<Option<Vec<String>>, String> {
    let mut holders = Vec::new();
    let mut visited = vec![sysdir.to_path_buf()];
    let mut frontier = vec![sysdir.to_path_buf()];
    for _ in 0..8 {
        let mut next = Vec::new();
        for dir in &frontier {
            for name in read_holders(dir)? {
                let holder_dir = Path::new("/sys/block").join(&name);
                // 持有者本身是块设备，目录必然存在；解析不了说明它已在采集窗口内消失，
                // 其子树是否含非 dm 栈无从确认，不能按「已看到的名字都无害」处理
                let real = std::fs::canonicalize(&holder_dir).map_err(|e| {
                    format!("cannot resolve {} to inspect its holders: {e}", holder_dir.display())
                })?;
                if visited.contains(&real) {
                    continue;
                }
                visited.push(real.clone());
                holders.push(name);
                next.push(real);
            }
        }
        if next.is_empty() {
            break;
        }
        frontier = next;
    }
    Ok(if holders.is_empty() { None } else { Some(holders) })
}

/// `holders/` 是 sysfs 给**每个**块设备都建的属性目录，缺失说明采集窗口内该设备已消失
/// 或 sysfs 视图不完整——"没有持有者"是它存在且为空才成立的结论。读不到上抛，
/// 不能读成"没人持有"
#[cfg(target_os = "linux")]
fn read_holders(sysdir: &Path) -> Result<Vec<String>, String> {
    let dir = sysdir.join("holders");
    let entries = std::fs::read_dir(&dir)
        .map_err(|e| format!("cannot read {}: {e}", dir.display()))?;
    let mut out = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| format!("cannot read {}: {e}", dir.display()))?;
        out.push(entry.file_name().to_string_lossy().into_owned());
    }
    Ok(out)
}

#[cfg(all(test, target_os = "linux"))]
mod image_probe {
    use super::{collect, verdict, OpShape};
    use crate::fsops::mount::Occupancy;

    /// 没有任何 loop 关联的普通文件是空闲：六类证据全部"已证明不存在"。
    /// 判错方向只允许把空闲判成占用，不允许反过来——这条向量钉住后者。
    #[test]
    fn a_plain_file_with_no_loop_backing_is_idle() {
        let dir = tempfile::Builder::new().prefix("diskedit-occ-").tempdir().unwrap();
        let img = dir.path().join("disk.img");
        std::fs::write(&img, vec![0u8; 64 * 1024]).unwrap();
        let src = crate::dev::FileSource::open_read_only_image(&img, None).unwrap();
        let facts = collect(&src).unwrap();
        assert!(matches!(verdict(OpShape::Overwrite, &facts), Occupancy::Idle));
    }
}

/// 持有者与成员关系直接构造事实，钉住 [`verdict`] 的形状规则。
/// 这些用例不产生运行证据：它们只保证判据被写下来
#[cfg(all(test, target_os = "linux"))]
mod holder_decisions {
    use super::{verdict, HolderRef, MemberRef, OccupancyFacts, OpShape};
    use crate::fsops::mount::Occupancy;

    fn facts(holders: Vec<Vec<&str>>, members: Vec<&str>) -> OccupancyFacts {
        OccupancyFacts {
            mounts: Vec::new(),
            swaps: Vec::new(),
            holders: holders
                .into_iter()
                .enumerate()
                .map(|(i, names)| HolderRef {
                    device: format!("dev{i}"),
                    holders: names.into_iter().map(str::to_string).collect(),
                })
                .collect(),
            loops: Vec::new(),
            btrfs_members: members
                .into_iter()
                .map(|detail| MemberRef { detail: detail.to_string() })
                .collect(),
            stack_members: Vec::new(),
        }
    }

    /// `Extend` 只在持有集合全是 dm 时放行；`Overwrite` 对同一集合全拦
    #[test]
    fn extend_releases_a_holder_set_only_when_every_name_is_dm() {
        let dm = facts(vec![vec!["dm-0", "dm-1"]], Vec::new());
        assert!(matches!(verdict(OpShape::Extend, &dm), Occupancy::Idle));
        assert!(matches!(verdict(OpShape::Overwrite, &dm), Occupancy::Held(_)));

        let mixed = facts(vec![vec!["dm-0", "md0"]], Vec::new());
        assert!(matches!(verdict(OpShape::Extend, &mixed), Occupancy::Held(_)));
    }

    /// 放行看全部持有集合：第一台设备全是 dm 不能替第二台设备的非 dm 持有者放行
    #[test]
    fn a_non_dm_holder_in_any_set_blocks_the_extend() {
        let f = facts(vec![vec!["dm-0"], vec!["md0"]], Vec::new());
        assert!(matches!(verdict(OpShape::Extend, &f), Occupancy::Held(_)));
    }

    /// 成员关系只在 `Overwrite` 时拦：扩容成员分区是 `btrfs device resize` /
    /// `mdadm --grow` 的前提
    #[test]
    fn members_block_overwrite_only() {
        let f = facts(Vec::new(), vec!["member of a multi-device filesystem"]);
        assert!(matches!(verdict(OpShape::Extend, &f), Occupancy::Idle));
        assert!(matches!(verdict(OpShape::Overwrite, &f), Occupancy::Member { .. }));
    }
}
