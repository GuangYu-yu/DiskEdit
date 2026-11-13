//! 挂载/占用探测与 offset loop 映射：写前的空闲校验。

use super::*;

/// /proc/self/mountinfo 一条记录（man proc_pid_mountinfo(5)）：固定字段
/// 1=mount ID、2=parent ID、3=major:minor、4=root、5=mount point、6=mount options，
/// 随后是数量可变的 optional fields，最后以单独的 "-" 分界，之后依次为 filesystem type、
/// mount source、super options。路径字段对空格/tab/换行/反斜杠/# 用八进制 \NNN 转义。
#[cfg(target_os = "linux")]
pub(crate) struct MountEntry {
    /// (major, minor)，与 st_rdev 经 libc::major/minor 分解后可比
    pub dev_no: (u64, u64),
    /// 已解码的挂载点
    pub mount_point: String,
    /// 已解码的设备/源路径
    pub source: String,
    /// `-` 之后的文件系统类型（如 `btrfs`）：设备身份的判据按它分派
    pub fstype: String,
    /// 读到这条记录的进程号：该进程所在 mount namespace 的可查标识。
    /// 本进程命名空间视图（/proc/self/mountinfo）没有对应进程号，为 `None`
    pub pid: Option<u32>,
}

/// 解码八进制转义（\NNN，如空格 = \040）；非转义序列原样保留
#[cfg(target_os = "linux")]
pub(crate) fn decode_octal(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\'
            && i + 3 < b.len()
            && b[i + 1..i + 4].iter().all(|c| (b'0'..=b'7').contains(c))
        {
            // 先在 u32 上合成再收进 u8：八进制位值最大 7*64+7*8+7=511，全程 u8 会在
            // \777 这类畸形序列上回绕（debug panic / release 511→255），畸形
            // mountinfo 行不该有让解析进程退出的能力
            let v = (b[i + 1] - b'0') as u32 * 64 + (b[i + 2] - b'0') as u32 * 8 + (b[i + 3] - b'0') as u32;
            out.push(v as u8);
            i += 4;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// mountinfo 一行 → MountEntry；字段不足或格式不符返回 None
#[cfg(target_os = "linux")]
pub(crate) fn parse_mountinfo(line: &str) -> Option<MountEntry> {
    let mut it = line.split_whitespace();
    let _mount_id = it.next()?;
    let _parent_id = it.next()?;
    let dev = it.next()?;
    let _root = it.next()?;
    let mnt = it.next()?;
    let _opts = it.next()?;
    // optional fields 数量可变，不能按固定下标取尾部字段：以单独的 "-" 为分界
    let rest: Vec<&str> = it.collect();
    let sep = rest.iter().position(|&f| f == "-")?;
    let fstype = rest.get(sep + 1)?;
    let source = rest.get(sep + 2)?;
    let (maj, min) = dev.split_once(':')?;
    Some(MountEntry {
        dev_no: (maj.parse().ok()?, min.parse().ok()?),
        mount_point: decode_octal(mnt),
        source: decode_octal(source),
        fstype: (*fstype).to_string(),
        pid: None,
    })
}

/// `/proc/<pid>/mountinfo` 读取失败时，哪些 errno 能证明"该进程已不引用挂载命名空间"：
/// - `NotFound`：遍历窗口内退出
/// - `ESRCH`：读到目录项时进程已消失
/// - `EINVAL`：nsproxy 已清空——内核 `mounts_open_common` 在 `!nsp || !nsp->mnt_ns`
///   时返回 EINVAL（fs/proc_namespace.c），该进程不再引用任何挂载
///
/// 权限不足（`EACCES`）不在此列：`hidepid` 隐藏的是"能不能看"，不是"有没有挂载"。
/// 读不到别人的 mountinfo 时，那些命名空间里的挂载仍然可能存在，按探测失败上抛。
///
/// 这几类读不到**等于已证明不存在**：挂载命名空间随最后一个引用它的进程退出而销毁，
/// 进程既然已经不在了，它名下不可能还挂着东西——这不是"证据源缺失"，不能升级成未知。
#[cfg(target_os = "linux")]
fn skippable_mountinfo_error(e: &io::Error) -> bool {
    e.kind() == io::ErrorKind::NotFound
        || matches!(e.raw_os_error(), Some(libc::ESRCH | libc::EINVAL))
}

/// 读取所有可读进程的 mountinfo。同一个设备可以在别的 mount namespace 里挂载，
/// 只看 /proc/self 会把那种占用漏成空闲（容器经 --device 拿到宿主设备即是一例）。
/// 逐 PID 读取，按 nsfs 的 (dev, ino) 去重——同一命名空间的进程给出同一份视图。
/// 只有能证明"该进程已不引用挂载命名空间"的读取失败才跳过（见
/// [`skippable_mountinfo_error`]）；权限不足意味着无法确认，按探测失败上抛。
/// 任何一行解析失败都整体报错：调用方的契约是 "确认不了空闲即不算空闲"
/// （见 probe_occupancy），跳过坏行会把目标设备恰好落在那一行上的情形误判成未挂载
/// ——宁可整个操作拒做
#[cfg(target_os = "linux")]
pub(crate) fn read_mounts() -> io::Result<Vec<MountEntry>> {
    use std::os::unix::fs::MetadataExt;
    let mut out = Vec::new();
    let mut namespaces = std::collections::HashSet::new();
    for entry in std::fs::read_dir("/proc")? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if name.is_empty() || !name.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        // 目录名是 pid：填不进 u32 的名字不可能对应可读的 /proc 项
        let Ok(pid) = name.parse::<u32>() else { continue };
        // 取不到命名空间身份（权限/竞态）就不去重，照常尝试读取它
        let ns = std::fs::metadata(entry.path().join("ns/mnt"))
            .ok()
            .map(|m| (m.dev(), m.ino()));
        if let Some(key) = ns
            && namespaces.contains(&key)
        {
            continue;
        }
        let path = entry.path().join("mountinfo");
        let s = match std::fs::read_to_string(&path) {
            Ok(s) => s,
            Err(e) if skippable_mountinfo_error(&e) => continue,
            Err(e) => return Err(io::Error::other(format!("cannot read {}: {e}", path.display()))),
        };
        append_mountinfo(&mut out, &s, &path, Some(pid))?;
        // 读到一份视图之后才登记该命名空间：读失败的进程不能替同一命名空间里仍在运行的
        // 进程消耗掉唯一一次读取机会
        if let Some(key) = ns {
            namespaces.insert(key);
        }
    }
    Ok(out)
}

/// 本进程 mount namespace 的挂载表（/proc/self/mountinfo）。
///
/// 这一份**不承担占用判定**：占用必须读 [`read_mounts`] 的全部可读命名空间视图，
/// 否则别的命名空间里的挂载会被读成空闲。它只用于本命名空间里可操作对象的解析
/// （挂载点/叠挂）：宿主的 shared 传播会把同一挂载带进其他命名空间，全视图里
/// 同一条记录出现多份，叠挂判定会把"同一个挂载的多个视图"误读成
/// "mounted multiple times"
#[cfg(target_os = "linux")]
pub(crate) fn read_current_namespace_mounts() -> io::Result<Vec<MountEntry>> {
    let path = std::path::Path::new("/proc/self/mountinfo");
    let s = std::fs::read_to_string(path)?;
    let mut out = Vec::new();
    append_mountinfo(&mut out, &s, path, None)?;
    Ok(out)
}

#[cfg(target_os = "linux")]
fn append_mountinfo(
    out: &mut Vec<MountEntry>,
    s: &str,
    path: &std::path::Path,
    pid: Option<u32>,
) -> io::Result<()> {
    for l in s.lines() {
        let mut entry = parse_mountinfo(l).ok_or_else(|| {
            io::Error::other(format!("unparseable {} line: {l}", path.display()))
        })?;
        entry.pid = pid;
        out.push(entry);
    }
    Ok(())
}

/// sysfs 组件名是否为 loop 设备本身（`loop0`）；`loop0p1` 这类分区名不算——
/// 尾部不是纯数字
#[cfg(target_os = "linux")]
pub(crate) fn is_loop_name(s: &str) -> bool {
    s.len() > 4 && s.starts_with("loop") && s[4..].bytes().all(|b| b.is_ascii_digit())
}

/// 一个块设备 sysfs 目录下的持有者（`holders/` 的项名，如 `dm-0`、`md0`）。
/// 持有者表示该设备正被另一个块设备栈当作底层使用。目录读不出来按探测失败上抛：
/// "确认不了没人持有"不能读成"没人持有"
#[cfg(target_os = "linux")]
fn holders_at(sysdir: &Path) -> io::Result<Vec<String>> {
    let dir = sysdir.join("holders");
    let rd = std::fs::read_dir(&dir)
        .map_err(|e| io::Error::other(format!("cannot read {}: {e}", dir.display())))?;
    let mut out = Vec::new();
    for entry in rd {
        let entry = entry.map_err(|e| io::Error::other(format!("cannot read {}: {e}", dir.display())))?;
        out.push(entry.file_name().to_string_lossy().into_owned());
    }
    Ok(out)
}

/// 设备占用探测结果（所有可读 mount namespace 的 mountinfo + /proc/swaps +
/// 块设备持有者 + loop 反向身份 + 多设备文件系统成员）。FS 操作要求分区未挂载、
/// 未作 swap、未被别的块设备栈持有，是本工具策略，内核并不强制（man resize2fs：
/// 内核支持在线扩容时可直接扩已挂载的 ext）；挂载态扩容走 xfs/btrfs 的临时挂载路径，
/// 不经此判定
#[cfg(target_os = "linux")]
pub(crate) enum Occupancy {
    /// 未挂载且非活动 swap、无持有者
    Idle,
    /// 挂载表中命中
    Mounted,
    /// /proc/swaps 命中
    ActiveSwap,
    /// `/sys/dev/block/<maj>:<min>/holders` 非空：设备被另一个块设备栈当作底层使用
    /// （LVM PV / dm-crypt / MD 成员 / loop 源）。栈顶设备才是挂载表里出现的那个名字，
    /// 底层设备本身看不见挂载或 swap 记录
    Held(Vec<String>),
    /// 本设备是某个 loop 的 backing，且该 loop（含 `losetup -P` 展开出的分区）在用。
    /// loop 不会把自己登记成 backing 的 holder：`loop_configure` 只 claim loop 自己的
    /// 设备（drivers/block/loop.c:927-935），backing 由 losetup 以普通 open 交给内核
    /// （util-linux lib/loopdev.c: `open(filename, O_RDWR|O_CLOEXEC)`）——所以
    /// sysfs holders 里没有它，第二方以 O_EXCL 打开 backing 也不会 EBUSY。挂载/swap
    /// 记录落在 loop 名下，backing 自己的 mountinfo 与 swap 表同样看不见
    LoopBacked {
        /// 在用的一环：loop 设备本身，或它的一个分区
        via: String,
        /// loop 之上的块设备栈（dm/md），空 = 只有挂载或 swap
        holders: Vec<String>,
    },
    /// 目标的多设备文件系统成员关系：它只是某个 btrfs 文件系统（或 md 阵列）的一员，
    /// 未挂载时既不在挂载表里、也没有持有者，单独整段重写会毁掉同组其它设备仍引用的元数据
    Member {
        /// 含设备名、所属文件系统/阵列与解除动作的完整句子
        detail: String,
    },
}

/// 探测设备节点的占用状态。`shape` 由调用方给：整段重写传 `Overwrite`，
/// 只扩末端的表写入传 `Extend`。放行条件（`Extend` 下持有者全是 dm）在
/// [`crate::dev::verdict`] 内判定，调用方不对单条持有链复算。
/// Err = 探测本身失败——调用方一律 fail-closed，
/// "确认不了空闲"不得当成"空闲"
#[cfg(target_os = "linux")]
pub(crate) fn probe_occupancy(dev: &str, shape: crate::dev::OpShape) -> io::Result<Occupancy> {
    let facts = crate::dev::collect_node(dev).map_err(io::Error::other)?;
    Ok(crate::dev::verdict(shape, &facts))
}

/// mountinfo 里是否有指向该设备的记录。双判据取并集：dev_no（st_rdev）之外比对
/// source 路径——btrfs 成员挂载在 mountinfo 字段 3 报匿名设备号（anon bdev，与成员
/// 分区 st_rdev 必不相同），但 source 仍是成员设备路径，单靠 dev_no 会漏检
#[cfg(target_os = "linux")]
fn mounted_in(entries: &[MountEntry], dev: &str, dev_no: (u64, u64)) -> bool {
    entries.iter().any(|e| {
        e.dev_no == dev_no || e.source == dev || (e.dev_no.0 == 0 && source_is_device(&e.source, dev_no))
    })
}

/// source 节点的真实设备号是否即 `dev_no`。用于字段 3 报匿名设备号的挂载（btrfs 等）：
/// 那种记录与 st_rdev 必然不等，而 `/dev/disk/by-uuid/…` 这类别名与目标路径串也不等，
/// 只有 stat 跟随链接后得到的 st_rdev 才可比。source 不是块设备节点（tmpfs 等伪文件系统）
/// 或已不可访问时返回 false——这类记录本就不以块设备为底
///
/// 非绝对路径的 source（`none`、`tmpfs`、`drvfs` 等）直接判否，不去 stat：这类名字不是
/// 路径，stat 会按进程工作目录解析成一次真实查找——工作目录在慢速文件系统（如 WSL 的
/// 9p/drvfs）上时，十几次这样的查找就是毫秒级开销，落进 loop 拆除的瞬态窗口里会把
/// "节点已删、sysfs 未收"读成确认失败
#[cfg(target_os = "linux")]
pub(crate) fn source_is_device(source: &str, dev_no: (u64, u64)) -> bool {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    if !source.starts_with('/') {
        return false;
    }
    std::fs::metadata(source).is_ok_and(|m| {
        m.file_type().is_block_device()
            && (libc::major(m.rdev()) as u64, libc::minor(m.rdev()) as u64) == dev_no
    })
}

/// /proc/swaps 里是否有该设备。同一设备的两种判据取并集：路径相同，或 st_rdev 相同。
/// stat 失败时两项都无法确认，按本模块的 fail-closed 口径报错——当成"这一项不是目标"
/// 就是把"确认不了"读成"空闲"
#[cfg(target_os = "linux")]
fn active_swap_in(swaps: &str, dev: &str, rdev: u64) -> io::Result<bool> {
    use std::os::unix::fs::MetadataExt;
    for line in swaps.lines().skip(1) {
        let Some(field) = line.split_whitespace().next() else { continue };
        if field == dev {
            return Ok(true);
        }
        let fm = std::fs::metadata(field).map_err(|e| {
            io::Error::other(format!(
                "cannot stat swap entry {field} to confirm {dev} is not active as swap: {e}"
            ))
        })?;
        if fm.rdev() == rdev {
            return Ok(true);
        }
    }
    Ok(false)
}

/// FS 步的占用闸：占用或探测失败都拒绝（fail-closed——探测失败若被当作
/// "未挂载"，会对已挂载的 FS 执行 resize，那是数据损坏）。
///
/// 分类只回答"成因在请求还是在环境"：确定被占用 ⇒ 请求与现状不符（`InvalidArgument`，3）；
/// 探测失败 ⇒ 环境故障（`Io`，4）。"本次是否写过盘"由命令层的归责点
/// `settled` 统一处理——写过的调用点会被抬成 5，这一层不替它混码
#[cfg(target_os = "linux")]
pub(crate) fn require_unmounted(dev: &str) -> Result<(), FsError> {
    use crate::dev::OccupancyFact;
    match probe_occupancy(dev, crate::dev::OpShape::Overwrite) {
        Ok(Occupancy::Idle) => Ok(()),
        Ok(Occupancy::Held(holders)) => Err(OccupancyFact::Occupied.fs_error(format!(
            "{dev} is held by {} — deactivate that stack first (FS operations require an unmounted partition)",
            holders.join(", ")
        ))),
        Ok(Occupancy::LoopBacked { via, .. }) => Err(OccupancyFact::Occupied.fs_error(format!(
            "{dev} is the backing of {via}, which is in use — unmount/swapoff/deactivate that loop first \
             (FS operations require an unmounted partition)"
        ))),
        Ok(Occupancy::Member { detail }) => Err(OccupancyFact::Occupied.fs_error(format!(
            "{detail} (FS operations require a standalone partition)"
        ))),
        Ok(Occupancy::Mounted) => Err(OccupancyFact::Occupied.fs_error(format!(
            "{dev} is mounted — unmount/deactivate first (FS operations require an unmounted partition)"
        ))),
        Ok(Occupancy::ActiveSwap) => Err(OccupancyFact::Occupied.fs_error(format!(
            "{dev} is active as swap — unmount/deactivate first (FS operations require an unmounted partition)"
        ))),
        Err(e) => Err(OccupancyFact::Unreadable.fs_error(format!(
            "cannot confirm {dev} is unmounted: {e} — refusing (fail-closed)"
        ))),
    }
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn require_unmounted(_dev: &str) -> Result<(), FsError> {
    Ok(())
}

/// `struct loop_info64`（include/uapi/linux/loop.h）：LOOP_GET_STATUS64 的载荷。
/// 只用到前两个字段，但 ioctl 按整个结构体拷贝，布局必须与内核逐字节一致
#[cfg(target_os = "linux")]
#[repr(C)]
struct LoopInfo64 {
    lo_device: u64,
    lo_inode: u64,
    lo_rdevice: u64,
    lo_offset: u64,
    lo_sizelimit: u64,
    lo_number: u32,
    lo_encrypt_type: u32,
    lo_encrypt_key_size: u32,
    lo_flags: u32,
    lo_file_name: [u8; 64],
    lo_crypt_name: [u8; 64],
    lo_encrypt_key: [u8; 32],
    lo_init: [u64; 2],
}

/// include/uapi/linux/loop.h：LOOP_GET_STATUS64
#[cfg(target_os = "linux")]
const LOOP_GET_STATUS64: libc::c_int = 0x4C05;

/// 一个 loop 记录的 backing 身份（三项都来自 `LOOP_GET_STATUS64`）
#[cfg(target_os = "linux")]
pub(crate) struct LoopBacking {
    pub(crate) dev: u64,
    pub(crate) ino: u64,
    pub(crate) rdev: u64,
}

/// 一条 loop 记录的全部几何（`LOOP_GET_STATUS64`）：backing 身份 + 数据起点 + 长度上限
#[cfg(target_os = "linux")]
pub(crate) struct LoopMapping {
    pub(crate) backing: LoopBacking,
    /// 数据起点，字节
    pub(crate) offset: u64,
    /// 数据终点相对起点的上限，字节
    pub(crate) sizelimit: u64,
}

/// 对已打开的 loop 节点取状态。None = 内核报 ENXIO（`lo_state != Lo_bound`），
/// 即确定的"没有绑定文件"
#[cfg(target_os = "linux")]
fn loop_status_of(node: &Path, f: &std::fs::File) -> io::Result<Option<LoopMapping>> {
    use std::os::unix::io::AsRawFd;
    let mut info: LoopInfo64 = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::ioctl(f.as_raw_fd(), LOOP_GET_STATUS64 as _, &mut info) };
    if rc != 0 {
        let e = io::Error::last_os_error();
        return match e.raw_os_error() {
            Some(libc::ENXIO) => Ok(None),
            _ => Err(io::Error::other(format!("LOOP_GET_STATUS64 on {} failed: {e}", node.display()))),
        };
    }
    Ok(Some(LoopMapping {
        backing: LoopBacking { dev: info.lo_device, ino: info.lo_inode, rdev: info.lo_rdevice },
        offset: info.lo_offset,
        sizelimit: info.lo_sizelimit,
    }))
}

/// 要在 loop 的 backing 里找的那个对象
#[cfg(target_os = "linux")]
pub(crate) enum BackingMatch {
    /// 普通文件：`(st_dev, st_ino)`
    File { dev: u64, ino: u64 },
    /// 块设备：设备号，节点以哪条路径暴露都命中
    Block { rdev: u64 },
}

#[cfg(target_os = "linux")]
impl BackingMatch {
    fn matches(&self, b: &LoopBacking) -> bool {
        match *self {
            Self::File { dev, ino } => b.dev == dev && b.ino == ino,
            Self::Block { rdev } => b.rdev != 0 && b.rdev == rdev,
        }
    }
}

/// 打不开设备节点时（`PermissionDenied`）的身份回退：`/sys/block/<name>/loop/backing_file`
/// 是 sysfs 属性（world-readable），不需要打开 `/dev` 节点。内容是内核对 backing 的
/// 路径表示；对它做 `metadata` 得到与本进程识别目标同一套 `(st_dev, st_ino, st_rdev)`，
/// 因此仍按对象身份比对，不按路径字符串比对。属性为空（未绑定）返回 None；
/// 其余读不出、路径已失效（stat 失败）都上抛——确认不了就由调用方 fail-closed
#[cfg(target_os = "linux")]
fn backing_identity_from_sysfs(name: &str) -> io::Result<Option<LoopBacking>> {
    use std::os::unix::fs::MetadataExt;
    let attr = format!("/sys/block/{name}/loop/backing_file");
    let raw = match std::fs::read_to_string(&attr) {
        Ok(s) => s,
        // 未绑定：该属性组由 loop_configure 建立、在 __loop_clr_fd 里移除
        // （drivers/block/loop.c v6.8: loop_sysfs_init :1093 / loop_sysfs_exit :1161），
        // 所以属性不存在和 ENXIO/ENODEV 一样是确定的"没有绑定文件"
        Err(e) if matches!(e.raw_os_error(), Some(libc::ENOENT) | Some(libc::ENXIO) | Some(libc::ENODEV)) => {
            return Ok(None)
        }
        Err(e) => {
            return Err(io::Error::new(
                e.kind(),
                format!("cannot read {attr}: {e} — refusing (fail-closed)"),
            ));
        }
    };
    let path = raw.trim();
    if path.is_empty() {
        return Ok(None);
    }
    // backing 被删除后内核在路径尾加 " (deleted)"，此时 stat 会失败——按确认不了上抛
    let path = path.strip_suffix(" (deleted)").unwrap_or(path);
    let md = std::fs::metadata(path).map_err(|e| {
        io::Error::new(
            e.kind(),
            format!("cannot stat loop backing {path} to verify its identity: {e} — refusing (fail-closed)"),
        )
    })?;
    Ok(Some(LoopBacking { dev: md.dev(), ino: md.ino(), rdev: md.rdev() }))
}

/// loop 设备当前绑定的 backing 的**对象身份**；未绑定返回 None。
///
/// 内核 `loop_get_status` 对 backing 的 `f_path` 做 `vfs_getattr`，把
/// `huge_encode_dev(stat.dev)` / `stat.ino` / `huge_encode_dev(stat.rdev)` 填进
/// `lo_device` / `lo_inode` / `lo_rdevice`（drivers/block/loop.c:1205-1229）。三项都
/// 取自已打开对象而不是路径：普通文件 backing 按 `(dev, ino)` 认，块设备 backing 按
/// `rdev`（设备号）认——节点名、别名、bind mount 都不影响设备号，而普通文件的
/// `st_rdev` 为 0，两种判据不会互相误命中。这条判定不经过任何用户态可见的文件名，
/// 所以 backing 的路径在本 mount namespace 不可解析时仍能命中。
/// ENXIO（内核在 `lo_state != Lo_bound` 时的返回）是确定的"没有绑定文件"，
/// 不算探测失败；ioctl 被拒、getattr 失败都无法确认，上抛后由调用方按
/// fail-closed 拒绝。
///
/// 打不开设备节点时（节点属 root:disk 0660）不能据此认定"没有绑定"——绑定可以是
/// 别的用户或别的 namespace 建立的。此时改读 sysfs 的 `loop/backing_file` 并对其
/// 目标做 stat（见 [`backing_identity_from_sysfs`]）：那条路径不需要打开 `/dev` 节点，
/// 拿到的仍是对象身份。sysfs 也读不出时才上抛，由调用方 fail-closed。
/// 其余打开失败（节点不存在、`ENODEV`、`EIO`）一律上抛：`/sys` 是全局视图，
/// `/dev` 属于 mount namespace，只说明本进程确认不了
#[cfg(target_os = "linux")]
fn loop_backing_identity(name: &str) -> io::Result<Option<LoopBacking>> {
    loop_backing_of_node(Path::new(&format!("/dev/{name}")))
}

/// 同一个 ioctl，按设备节点路径打开。身份层（[`crate::dev::TargetIdentity::loop_backed`]）
/// 手里只有打开时的节点路径，而"这个 loop 现在绑的是哪个对象"只能从内核取——
/// 与占用判定共用这一处，两处各写一遍正是"名字被复用"能混进来的原因
#[cfg(target_os = "linux")]
pub(crate) fn loop_backing_of_node(node: &Path) -> io::Result<Option<LoopBacking>> {
    let f = match std::fs::File::open(node) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::PermissionDenied => {
            let name = node.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            return backing_identity_from_sysfs(&name);
        }
        Err(e) => {
            return Err(io::Error::new(
                e.kind(),
                format!("cannot open {} to verify its backing file: {e}", node.display()),
            ));
        }
    };
    Ok(loop_status_of(node, &f)?.map(|m| m.backing))
}

/// 刚由 `losetup` 建立的 loop：重读内核记录得到 backing 与映射区间。
/// 打开失败与 ioctl 失败都上抛，不回退到 sysfs 的路径表示——那一条只回答
/// "绑的是哪个对象"，答不出 offset/sizelimit，用它与请求比对等于少验一项
#[cfg(target_os = "linux")]
pub(crate) fn loop_mapping_of_node(node: &Path) -> io::Result<Option<LoopMapping>> {
    let f = std::fs::File::open(node)
        .map_err(|e| io::Error::other(format!("cannot open {} to read its loop mapping: {e}", node.display())))?;
    loop_status_of(node, &f)
}

/// 指向本镜像的 loop 被使用的形态
#[cfg(target_os = "linux")]
pub(crate) enum LoopOccupancy {
    /// loop 本身或它的分区被挂载或是活动 swap：字段是那个设备路径
    Live(String),
    /// loop（或 `losetup -P` 展开出的分区）被上层块设备栈持有
    Held { loopdev: String, holders: Vec<String> },
    /// 内核拒绝独占打开（EBUSY）：挂载、swap 或他人的独占 claim 占着它
    KernelBusy { loopdev: String },
    /// 拿不到独占权（节点缺失、权限不足、I/O 错误等）：不是"空闲"的证据
    Unclaimable { loopdev: String, why: String },
}

/// 一个 loop 及其 `losetup -P` 展开出的分区：`(设备名, sysfs 目录)`。
/// `partition` 属性存在即为分区；除 NotFound 之外的读取失败都上抛——确认不了哪个
/// 目录是分区，就可能漏掉那个节点上的占用。属性文件（`uevent`、`size` 等）不是候选，
/// 对它们拼 `partition` 只会得到 ENOTDIR
#[cfg(target_os = "linux")]
fn loop_and_partitions(sysdir: &Path, name: &str) -> io::Result<Vec<(String, PathBuf)>> {
    let mut out = vec![(name.to_string(), sysdir.to_path_buf())];
    for child in std::fs::read_dir(sysdir)? {
        let child = child?;
        let is_dir = child
            .file_type()
            .map_err(|e| io::Error::other(format!("cannot stat {}: {e}", child.path().display())))?
            .is_dir();
        if !is_dir {
            continue;
        }
        let part = child.path().join("partition");
        match std::fs::metadata(&part) {
            Ok(_) => out.push((child.file_name().to_string_lossy().into_owned(), child.path())),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(io::Error::other(format!("cannot stat {}: {e}", part.display()))),
        }
    }
    Ok(out)
}

/// 指向同一镜像的 loop 中，哪一个正被使用——挂载、活动 swap，或被块设备栈持有。
///
/// 镜像是本工具内唯一没有分区节点可查的目标：本工具要经 loop 才能碰它的分区，而那个
/// loop 与用户自己映射的是**两个设备**——只探测新建的那个看不见用户那份占用。故反向
/// 枚举所有 loop，按 backing 文件身份（st_dev + st_ino，取自 LOOP_GET_STATUS64）
/// 与本镜像比对，命中后检查该 loop 及其分区的挂载、swap 与持有者。
/// sysfs 的 `loop/backing_file` 是路径表示，既可能在本进程的 mount namespace 不可
/// 解析，也可能已指向被替换过的文件，故**不作为首选判据**；只有打不开 `/dev` 节点时
/// 才用它做身份回退（对路径做 stat，仍比对象号，见 [`backing_identity_from_sysfs`]）。
/// 任一环节确认不了都上抛，由调用方 fail-closed；枚举本身来自全局 sysfs，不随 mount
/// namespace 变化。
///
/// 挂载视图来自 procfs，可见范围取决于**挂载该 procfs 时所在的 PID 命名空间**
/// （man pid_namespaces(7)）——只有宿主初始 PID 命名空间的 procfs 才列得出全部相关进程；
/// 容器内自行挂载的 procfs 看不见祖先命名空间的进程，`NSpid` 一个值也证明不了"已经看到
/// 全部"。故本函数的判据由两部分组成：可解释的挂载/swap/holder 视图，与内核独占 claim
/// （[`loop_occupancy_for`] 取得并留持）。前者受命名空间限制，后者与命名空间无关，
/// 两者都不成立时才是空闲；任一环节给不出结论一律拒绝
#[cfg(target_os = "linux")]
pub(crate) fn image_loop_occupancy(src: &FileSource) -> io::Result<Option<LoopOccupancy>> {
    use std::os::unix::fs::MetadataExt;
    let md = src.metadata().map_err(|e| {
        io::Error::other(format!("cannot stat the open image to identify it: {e}"))
    })?;
    let mounts = read_mounts()?;
    let swaps = std::fs::read_to_string("/proc/swaps")?;
    loop_occupancy_for(BackingMatch::File { dev: md.dev(), ino: md.ino() }, &mounts, &swaps)
}

/// 已取得的一条 loop 独占权：fd 维持 claim，记录则供每次消费时重核。
/// 名字与"fd 仍打开着"都不构成持有证据：`O_EXCL` 只排他打开、不冻结映射，另一个打开
/// 仍可通过 `LOOP_SET_STATUS64` 改写几何，本进程的 fd 并不因此失效；故登记时把内核
/// 给出的那份映射一并记下，之后每次按 `LOOP_GET_STATUS64` 重读比对
#[cfg(target_os = "linux")]
struct LoopClaim {
    loopdev: String,
    file: std::fs::File,
    mapping: LoopMapping,
}

/// 登记时的映射与此刻内核给出的是不是同一条。`None` = 内核报已无绑定，同样不是持有
#[cfg(target_os = "linux")]
fn claim_still_holds(recorded: &LoopMapping, current: Option<&LoopMapping>) -> bool {
    match current {
        Some(c) => {
            c.backing.dev == recorded.backing.dev
                && c.backing.ino == recorded.backing.ino
                && c.backing.rdev == recorded.backing.rdev
                && c.offset == recorded.offset
                && c.sizelimit == recorded.sizelimit
        }
        None => false,
    }
}

/// 表里同名登记此刻是否仍代表持有：`None` = 表里没有这个名字，`Some(true)` = 重读的映射
/// 与登记一致，`Some(false)` = 有名字但映射已不是登记那条，条目在此移除（fd 随之关闭）。
/// 名字只用来找回登记，判定一律基于重读
#[cfg(target_os = "linux")]
fn consume_registered_claim(claims: &mut Vec<LoopClaim>, loopdev: &str) -> io::Result<Option<bool>> {
    let Some(pos) = claims.iter().position(|c| c.loopdev == loopdev) else {
        return Ok(None);
    };
    let now = loop_status_of(Path::new(loopdev), &claims[pos].file)?;
    if claim_still_holds(&claims[pos].mapping, now.as_ref()) {
        return Ok(Some(true));
    }
    claims.remove(pos);
    Ok(Some(false))
}

/// 已取得的 loop 独占权，持有到进程退出。CLI 一次调用只做一件事，
/// 进程退出即释放；提前 drop 会把"检查通过 → 写盘"之间的窗口重新打开。按 loop 节点名
/// 索引而非用后即弃：同一进程内多处闸口（表写入、prepare、mkfs）都会走到判定，重复 claim
/// 会与自己已持的 claim 冲突而把空闲读成占用。索引只用于找回登记，是否仍持有由
/// [`claim_still_holds`] 按重读的映射回答
#[cfg(target_os = "linux")]
static LOOP_CLAIMS: std::sync::Mutex<Vec<LoopClaim>> = std::sync::Mutex::new(Vec::new());

/// 对 loop 设备取内核独占权：`O_RDWR | O_EXCL`、不带 `O_CREAT`。man open(2)：Linux 2.6
/// 起，块设备正在被使用（挂载、swap、他人的独占打开）时同一组合以 EBUSY 失败；
/// 只读挂载同样算使用。探测对象取 loop **父设备**：分区被 claim 时整盘的 claim 状态
/// 一并变化，故对父设备 claim 能撞上挂在分区上的使用——这条冲突关系可读于 Linux 3.19 的
/// `fs/block_dev.c`（`bd_start_claiming` / `bd_prepare_to_claim`）与当前主线
/// `block/bdev.c`（`bd_may_claim`）。它只覆盖守块设备约定的使用者，是额外的一层防线，
/// 不构成"已覆盖全部使用者"的结论
#[cfg(target_os = "linux")]
fn claim_loop_exclusive(loopdev: &str) -> io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_EXCL)
        .open(loopdev)
}

/// O_EXCL 打开失败的两种含义：EBUSY = 内核把它判给了别的使用者（占用）；其余错误
/// （权限、节点缺失、I/O）不构成"设备空闲"的证据，按确认失败处理。两者都不放行，
/// 区别只在给用户的原因与出口（占用是请求与现状不匹配，确认失败是环境不给证据）
#[cfg(target_os = "linux")]
fn claim_failure(loopdev: String, err: io::Error) -> LoopOccupancy {
    if err.raw_os_error() == Some(libc::EBUSY) {
        LoopOccupancy::KernelBusy { loopdev }
    } else {
        LoopOccupancy::Unclaimable { loopdev, why: err.to_string() }
    }
}

/// 反向枚举 loop，找出 backing 命中 `target` 的那一个，并回答它是否在用。
/// 镜像（文件 backing）与块设备（设备号 backing）共用这一份判据。
///
/// 这里只报事实，不套策略：dm 持有是否放行由 [`crate::dev::verdict`] 按操作形状决定，
/// 节点打不开一律上抛（调用方 fail-closed），不因"建不出这只绑定的用户无权读取"而跳过
#[cfg(target_os = "linux")]
pub(crate) fn loop_occupancy_for(
    target: BackingMatch,
    mounts: &[MountEntry],
    swaps: &str,
) -> io::Result<Option<LoopOccupancy>> {
    use std::os::unix::fs::MetadataExt;
    // 每个 loop 独立成形：持有者记在真正拥有它的那个 loop 名下。跨 loop 汇总会把
    // "B 上的占用"说成"A 被持有"，而拒绝消息是用户解除占用的唯一线索
    let mut held: Vec<(String, Vec<String>)> = Vec::new();
    for entry in std::fs::read_dir("/sys/block")? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if !is_loop_name(&name) {
            continue;
        }
        match loop_backing_identity(&name) {
            Ok(Some(b)) if target.matches(&b) => {}
            Ok(_) => continue,
            // 身份读不出来就无法指认这只 loop 绑的是谁：确认不了，按 fail-closed 上抛
            Err(e) => return Err(e),
        }
        let loopdev = format!("/dev/{name}");
        let mut holders: Vec<String> = Vec::new();
        for (node, sysdir) in loop_and_partitions(&entry.path(), &name)? {
            let path = format!("/dev/{node}");
            let m = std::fs::metadata(&path).map_err(|e| {
                io::Error::new(
                    e.kind(),
                    format!("cannot stat {path} to confirm it is unused: {e}"),
                )
            })?;
            let rdev = m.rdev();
            let dev_no = (libc::major(rdev) as u64, libc::minor(rdev) as u64);
            if mounted_in(mounts, &path, dev_no) || active_swap_in(swaps, &path, rdev)? {
                return Ok(Some(LoopOccupancy::Live(path)));
            }
            holders.extend(holders_at(&sysdir)?);
        }
        if !holders.is_empty() {
            held.push((loopdev, holders));
            continue;
        }
        // 挂载 / swap / 持有者都没命中：向内核要独占权并留持到进程退出。这一步与
        // mount namespace 无关，覆盖"挂载视图看不见的那个命名空间"；反过来它也只覆盖
        // 守块设备约定的使用者，普通文件写入既不经 claim 也不产生 claim
        let mut claims = match LOOP_CLAIMS.lock() {
            Ok(c) => c,
            Err(_) => {
                return Ok(Some(LoopOccupancy::Unclaimable {
                    loopdev,
                    why: "loop claim registry is poisoned".to_string(),
                }))
            }
        };
        // 同名登记只说明本进程曾经打开过这个名字。重读到未绑定或换了映射即丢弃该条
        // （fd 随之关闭，旧 claim 释放），按此刻的设备重新 claim——不复用过期结论
        match consume_registered_claim(&mut claims, &loopdev) {
            Ok(Some(true)) => continue,
            Ok(_) => {}
            Err(e) => {
                return Ok(Some(LoopOccupancy::Unclaimable {
                    loopdev,
                    why: format!("cannot re-read its loop mapping to tell whether this process still holds it: {e}"),
                }))
            }
        }
        match claim_loop_exclusive(&loopdev) {
            Ok(f) => match loop_status_of(Path::new(&loopdev), &f) {
                // 记录 claim 时内核给出的映射：之后的每次消费都按它重核
                Ok(Some(mapping)) if target.matches(&mapping.backing) => {
                    claims.push(LoopClaim { loopdev, file: f, mapping })
                }
                // claim 与登记之间这只 loop 已解绑或换成别的 backing：它不再映射本镜像，
                // 不登记；fd 在此关闭，claim 一并释放
                Ok(_) => {}
                Err(e) => {
                    return Ok(Some(LoopOccupancy::Unclaimable {
                        loopdev,
                        why: format!("cannot read the loop mapping just claimed: {e}"),
                    }))
                }
            },
            // 拿不到独占权不等于"没人用"：EBUSY 已有单独变体，其余原因按确认不了上报
            Err(e) => return Ok(Some(claim_failure(loopdev, e))),
        }
    }
    // 多个 loop 都被持有时报第一个；loopdev 与 holders 来自同一次扫描
    match held.into_iter().next() {
        Some((loopdev, holders)) => Ok(Some(LoopOccupancy::Held { loopdev, holders })),
        None => Ok(None),
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    #![allow(clippy::let_underscore_must_use)] // 清理临时镜像与 loop 有意忽略失败
    use super::*;

    #[test]
    fn loop_info64_matches_the_uapi_layout() {
        // include/uapi/linux/loop.h：5 个 __u64、4 个 __u32、LO_NAME_SIZE(64) 两个、
        // LO_KEY_SIZE(32)、lo_init[2]；总长 232（8 字节对齐）
        assert_eq!(std::mem::size_of::<LoopInfo64>(), 232);
        assert_eq!(std::mem::offset_of!(LoopInfo64, lo_device), 0);
        assert_eq!(std::mem::offset_of!(LoopInfo64, lo_inode), 8);
        assert_eq!(std::mem::offset_of!(LoopInfo64, lo_file_name), 56);
    }

    /// 打不开节点不等于"没有绑定"：节点不存在只说明本进程确认不了，
    /// 必须上抛给调用方按 fail-closed 拒绝；EACCES 走 sysfs 回退，回退也失败同样上抛
    #[test]
    fn unopenable_loop_node_is_not_evidence_of_no_binding() {
        let err = match loop_backing_identity("diskedit-nonexistent-loop") {
            Err(e) => e,
            Ok(_) => panic!("an unopenable node must not be read as 'no binding'"),
        };
        assert!(err.to_string().contains("cannot open /dev/diskedit-nonexistent-loop"));
    }

    /// 跳过 mountinfo 读取失败只在"能证明进程已不引用挂载命名空间"时成立。
    /// 权限不足（hidepid 隐藏 `/proc/<pid>/mountinfo`）说明不了有没有挂载，
    /// 把它与进程退出一并跳过，探测就会在看不见的命名空间上宣称"空闲"
    #[test]
    fn mountinfo_skip_only_covers_proven_process_exit() {
        assert!(skippable_mountinfo_error(&io::Error::from(io::ErrorKind::NotFound)));
        assert!(skippable_mountinfo_error(&io::Error::from_raw_os_error(libc::ESRCH)));
        // nsproxy 已清空：mounts_open_common 的 `!nsp || !nsp->mnt_ns` 分支
        assert!(skippable_mountinfo_error(&io::Error::from_raw_os_error(libc::EINVAL)));
        assert!(!skippable_mountinfo_error(&io::Error::from_raw_os_error(libc::EACCES)));
        assert!(!skippable_mountinfo_error(&io::Error::from_raw_os_error(libc::EIO)));
    }

    /// backing 的认法按对象类型分开：文件走 (dev, ino)，块设备走设备号——
    /// 后者不随节点名/别名/bind mount 变化。普通文件的 st_rdev 为 0，
    /// 不能与任何设备号相认
    #[test]
    fn backing_match_separates_file_identity_from_device_numbers() {
        let block = LoopBacking { dev: 7, ino: 42, rdev: 0x0801 };
        assert!(BackingMatch::File { dev: 7, ino: 42 }.matches(&block));
        assert!(!BackingMatch::File { dev: 7, ino: 43 }.matches(&block));
        assert!(BackingMatch::Block { rdev: 0x0801 }.matches(&block));

        let file = LoopBacking { dev: 7, ino: 42, rdev: 0 };
        assert!(!BackingMatch::Block { rdev: 0 }.matches(&file), "rdev 0 must never name a device");
        assert!(BackingMatch::File { dev: 7, ino: 42 }.matches(&file));
    }

    /// attach 之后的自证按三个独立维度判：backing 对象、起点、长度上限。
    /// 只核对其中一项会让另外两项被写工具当作已核实的事实
    #[test]
    fn loop_mapping_covers_backing_offset_and_limit_together() {
        let map = |dev, ino, rdev, offset, sizelimit| LoopMapping {
            backing: LoopBacking { dev, ino, rdev },
            offset,
            sizelimit,
        };
        let file = BackingMatch::File { dev: 7, ino: 42 };
        let block = BackingMatch::Block { rdev: 0x0801 };

        let good = map(7, 42, 0, 4096, 8192);
        assert!(file.matches(&good.backing));
        assert!(mapping_covers(&good, 4096, 8192));
        // 起点差一个字节、长度上限差一个字节都算另一个映射
        assert!(!mapping_covers(&good, 4097, 8192));
        assert!(!mapping_covers(&good, 4096, 8193));
        // 未设上限（0）会让 loop 延伸到 backing 末端，不是请求的区间
        assert!(!mapping_covers(&map(7, 42, 0, 4096, 0), 4096, 8192));

        // backing 换了对象：区间数字再对也不是本次请求
        assert!(!file.matches(&map(7, 43, 0, 4096, 8192).backing));
        assert!(block.matches(&map(0, 0, 0x0801, 4096, 8192).backing));
        assert!(!block.matches(&map(7, 42, 0, 4096, 8192).backing));
    }

    /// 表里有这个名字不等于仍持有：登记之后同一 loopN 可以被外部解绑并重绑，
    /// fd 一直有效，只有重读的映射才能回答"这条 claim 还成不成立"
    #[test]
    fn a_registered_loop_name_is_not_proof_of_holding() {
        let map = |dev, ino, offset, sizelimit| LoopMapping {
            backing: LoopBacking { dev, ino, rdev: 0 },
            offset,
            sizelimit,
        };
        let recorded = map(7, 42, 4096, 8192);
        assert!(claim_still_holds(&recorded, Some(&map(7, 42, 4096, 8192))));
        // detach + 重绑到另一个文件：名字与 fd 都没变，映射已不是登记时那条
        assert!(!claim_still_holds(&recorded, Some(&map(7, 43, 4096, 8192))));
        // 同一个 backing，但起点或长度上限被换掉
        assert!(!claim_still_holds(&recorded, Some(&map(7, 42, 0, 8192))));
        assert!(!claim_still_holds(&recorded, Some(&map(7, 42, 4096, 16384))));
        // 已解绑
        assert!(!claim_still_holds(&recorded, None));
    }

    /// 真实 loop：登记之后另一个进程（不带 O_EXCL）仍能通过 LOOP_SET_STATUS64 改写同一只
    /// loop 的几何，名字与本进程的 fd 都不变——表里有名字不等于仍持有。
    /// 需要 root 与 /dev/loop-control；缺就打印原因并跳过
    #[cfg(target_os = "linux")]
    #[test]
    fn a_claim_is_rechecked_against_the_live_mapping() {
        use std::os::unix::io::AsRawFd;
        if unsafe { libc::geteuid() } != 0 {
            eprintln!("skip: root required for a real loop");
            return;
        }
        let dir = crate::testdir::test_dir("claim");
        let img = dir.join("claim.img");
        std::fs::write(&img, vec![0u8; 8 * 1024 * 1024]).unwrap();
        let out = match std::process::Command::new("losetup")
            .args(["-f", "--show", "-o", "0", "--sizelimit", "4194304"])
            .arg(&img)
            .output()
        {
            Ok(o) if o.status.success() => o,
            _ => {
                eprintln!("skip: losetup cannot create a loop here");
                return;
            }
        };
        let dev = String::from_utf8_lossy(&out.stdout).trim().to_string();
        let claimed = match claim_loop_exclusive(&dev) {
            Ok(f) => f,
            Err(e) => {
                eprintln!("skip: cannot claim {dev}: {e}");
                release_loop(&dev);
                return;
            }
        };
        let recorded = match loop_status_of(Path::new(&dev), &claimed) {
            Ok(Some(m)) => m,
            Ok(None) => {
                eprintln!("skip: {dev} reports no mapping right after the claim");
                drop(claimed);
                release_loop(&dev);
                return;
            }
            Err(e) => {
                eprintln!("skip: cannot read the mapping of {dev}: {e}");
                drop(claimed);
                release_loop(&dev);
                return;
            }
        };
        let backing_ino = recorded.backing.ino;
        let mut registry = vec![LoopClaim { loopdev: dev.clone(), file: claimed, mapping: recorded }];
        // include/uapi/linux/loop.h：LOOP_SET_STATUS64。另一个不带 O_EXCL 的打开
        // 在 O_EXCL 持有者仍在时也能改 sizelimit，内核随即重算容量
        const LOOP_SET_STATUS64: libc::c_int = 0x4C04;
        let second = std::fs::OpenOptions::new().read(true).write(true).open(&dev).unwrap();
        let mut info: LoopInfo64 = unsafe { std::mem::zeroed() };
        let read_rc = unsafe { libc::ioctl(second.as_raw_fd(), LOOP_GET_STATUS64 as _, &mut info) };
        info.lo_sizelimit = 6 * 1024 * 1024;
        let write_rc = unsafe { libc::ioctl(second.as_raw_fd(), LOOP_SET_STATUS64 as _, &info) };
        let verdict = consume_registered_claim(&mut registry, &dev);
        let now = loop_status_of(Path::new(&dev), &std::fs::File::open(&dev).unwrap()).unwrap();
        // 先清理再断言：O_EXCL 的 fd 之前，detach 只会被推迟
        drop(second);
        release_loop(&dev);
        assert_eq!((read_rc, write_rc), (0, 0), "changing the geometry of a claimed loop must work");
        assert_eq!(verdict.unwrap(), Some(false), "a name still in the table must not be read as holding");
        assert!(registry.is_empty(), "the stale entry must have been dropped");
        let now = now.unwrap();
        assert_eq!(now.sizelimit, 6 * 1024 * 1024);
        assert_eq!(now.backing.ino, backing_ino, "the backing did not move; only the geometry drifted");
    }

    /// O_EXCL 的两种失败含义不同，不可合并：EBUSY 是内核说"它被别人占着"（请求与现状
    /// 不匹配），权限/节点缺失/I/O 是"确认不了"（fail-closed）。把后者读成空闲，正是
    /// 这条防线要防的事——容器里 `/dev/loop*` 多半就不在本命名空间可见
    #[test]
    fn exclusive_claim_failure_is_classified_by_errno() {
        match claim_failure("loop9".to_string(), io::Error::from_raw_os_error(libc::EBUSY)) {
            LoopOccupancy::KernelBusy { loopdev } => assert_eq!(loopdev, "loop9"),
            _ => panic!("EBUSY must be reported as busy, not as unclaimable"),
        }
        for errno in [libc::EACCES, libc::ENOENT, libc::EIO] {
            match claim_failure("loop9".to_string(), io::Error::from_raw_os_error(errno)) {
                LoopOccupancy::Unclaimable { loopdev, why } => {
                    assert_eq!(loopdev, "loop9");
                    assert!(!why.is_empty(), "the underlying error must be carried through");
                }
                _ => panic!("errno {errno} must not be read as an occupancy verdict"),
            }
        }
    }
}

/// 本次写入对目标区间的形状。占用判据据此分档：起点不变、末端不变或只增的写入
/// （含 no-op，连表项都不写）不会动到上层栈已映射的扇区；搬移、缩小与整段重写
/// （mkfs）都会让栈顶设备继续指向被改写的字节
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteKind {
    /// 起点不变，末端不变或变大
    Extend,
    /// 搬移、缩小或整段重写
    Overwrite,
}

/// 被 DM 持有的设备是否可放行本次写入。LVM PV 与 dm-crypt 的标准增长流程是
/// 先长分区、再 pvresize / cryptsetup resize——增长不改变 DM 栈已映射的扇区，
/// 故纯扩容放行；MD/loop 等栈没有这种增量语义，任何写入都可能与栈顶视图脱节
#[cfg(target_os = "linux")]
pub(crate) fn dm_only(holders: &[String]) -> bool {
    // 空切片不是"全部持有者都是 dm-"：它意味着调用方拿不到持有者列表，
    // `all` 在空集上恒真，会把"没有证据"读成"证据支持放行"
    !holders.is_empty() && holders.iter().all(|h| h.starts_with("dm-"))
}

/// 写入路径的占用前置闸（表写入与 mkfs）：目标在首次落盘前须空闲。表写入的调用点在
/// prepare 层（movepart 的 prepare_resize / prepare_apply）与 MBR resize 命令层——那是
/// 任何写盘的唯一必经关口：占用在这里被判成 Refused（请求与现状不匹配）而不是 Partial，
/// 不会留下"表已写、FS 步被占"的一半状态。块设备另有整盘 O_EXCL 独占打开这道内核级防线，
/// 但其保护面要说准：它挡的是挂载 / swap / 他人的 O_EXCL 打开（open 即 EBUSY），
/// **不挡**不守约定的普通并发写——本闸对其是纵深防御。被 losetup 当作 backing 的
/// 块设备这两道都看不见（loop 不登记 holder，losetup 也不以 O_EXCL 打开 backing），
/// 由 [`Occupancy::LoopBacked`] 补：反向枚举 loop，按设备号认 backing。镜像没有分区
/// 节点可查，却仍有一道内核级防线可用：对绑定到同一文件的每个 loop 取 `O_RDWR|O_EXCL`
/// 独占权（[`claim_loop_exclusive`]，与 mount namespace 无关）并留持到进程退出，占用
/// 先按"有没有 loop 绑定到同一文件且在用"判定（[`crate::dev::verdict`] 的
/// loop 证据，身份取自 LOOP_GET_STATUS64 而非路径），再用这道 claim 收口。两端保护面都要说准：claim 只挡
/// 守块设备约定的使用者（挂载 / swap / 他人的独占打开），不挡按镜像路径直写字节的
/// 并发写者；挂载视图只在宿主初始 PID 命名空间的 procfs 上完整
/// （man pid_namespaces(7)），容器内的视图给不出全部进程时，只能依赖 claim 的结论。
/// `start_bytes` = 分区起始字节（与 find_block_partition_node 的换算基准一致）
#[cfg(target_os = "linux")]
pub fn ensure_idle_before_write(src: &FileSource, part: u32, start_bytes: u64, kind: WriteKind) -> Result<(), crate::outcome::RunOutcome> {
    use crate::dev::OccupancyFact;
    if !src.is_block {
        return image_target_unused(src, kind);
    }
    let node = find_block_partition_node(src, part, start_bytes).map_err(crate::outcome::RunOutcome::from)?;
    // 形态决定放行范围：`Extend` 只放行持有者全是 dm 的栈，判定与文案都在
    // [`crate::dev::verdict`] 的结论里；这里不对 `Held` 的返回值做第二次放行
    match probe_occupancy(&node, crate::dev::OpShape::from(kind)) {
        Ok(Occupancy::Idle) => Ok(()),
        Ok(Occupancy::Mounted) => Err(OccupancyFact::Occupied.outcome(format!(
            "{node} is mounted — unmount before resizing (moving a mounted partition's extents invalidates the live mount)"
        ))),
        Ok(Occupancy::ActiveSwap) => Err(OccupancyFact::Occupied.outcome(format!("{node} is active swap — run swapoff first"))),
        Ok(Occupancy::Held(holders)) => Err(OccupancyFact::Occupied.outcome(format!(
            "{node} is held by {} — deactivate that stack first (the write would change bytes the holder still maps)",
            holders.join(", ")
        ))),
        Ok(Occupancy::LoopBacked { via, holders }) => Err(OccupancyFact::Occupied.outcome(format!(
            "{node} is the backing of {via}{} — unmount/swapoff/deactivate that loop first \
             (the write would change bytes the loop still maps)",
            if holders.is_empty() { String::new() } else { format!(" (held by {})", holders.join(", ")) }
        ))),
        Ok(Occupancy::Member { detail }) => Err(OccupancyFact::Occupied.outcome(detail)),
        Err(e) => Err(OccupancyFact::Unreadable.outcome(format!(
            "cannot confirm {node} is unmounted: {e} — refusing (fail-closed)"
        ))),
    }
}

/// 镜像目标的占用判定：目标级，不区分分区。结论来自与记录准入同一份事实与
/// [`crate::dev::verdict`]：形状只影响 dm 持有（`Extend` 放行、`Overwrite` 拦），
/// 探测失败一律是"一个字节都没写"的 4，不是"请求与现状不符"的 3
#[cfg(target_os = "linux")]
fn image_target_unused(src: &FileSource, kind: WriteKind) -> Result<(), crate::outcome::RunOutcome> {
    use crate::dev::OccupancyFact;
    let facts = crate::dev::collect(src).map_err(|why| {
        OccupancyFact::Unreadable.outcome(format!(
            "cannot confirm no loop has this image in use: {why} — refusing (fail-closed)"
        ))
    })?;
    match crate::dev::verdict(crate::dev::OpShape::from(kind), &facts) {
        Occupancy::Idle => Ok(()),
        Occupancy::Mounted => Err(OccupancyFact::Occupied.outcome(
            "the image is mounted — unmount it first (editing the image changes a live filesystem)",
        )),
        Occupancy::ActiveSwap => Err(OccupancyFact::Occupied.outcome(
            "the image is active as swap — run swapoff first (editing the image changes a live swap area)",
        )),
        Occupancy::Held(holders) => Err(OccupancyFact::Occupied.outcome(format!(
            "the image is held by {} — deactivate that stack first (editing the image changes bytes the holder still maps)",
            holders.join(", ")
        ))),
        Occupancy::LoopBacked { via, holders } => Err(OccupancyFact::Occupied.outcome(format!(
            "{via} is a loop onto this image and is in use — unmount/swapoff/deactivate it first \
             (editing the image changes bytes that view still maps){}",
            if holders.is_empty() { String::new() } else { format!(" (held by {})", holders.join(", ")) }
        ))),
        Occupancy::Member { detail } => Err(OccupancyFact::Occupied.outcome(detail)),
    }
}

/// 分区表写入前的占用闸（整盘范围）：写表会让盘上布局与内核仍在使用的分区视图分叉，
/// 故目标上任何一个分区在用都要拒绝——镜像则是任何绑定到该镜像的 loop 在用即拒绝。
/// 与 [`ensure_idle_before_write`] 的分区级判定不同，本闸不区分本次改哪个分区，
/// 因此不适用于"扩容放行 DM 持有"的形状判定。
///
/// 调用点是所有**直接改表**的命令：`new` / `add` / `delete` / `create` /
/// `set name|type|flag` / `undo`。搬移类命令（`resize-part` / `move` / `resize` /
/// `apply`）在 prepare 层按操作形状做分区级判定，不走这里。`copy` 是显式例外：
/// 它只在经重叠校验的空闲区新增条目，不改既有分区的起止，也不搬移目标分区的 extent
/// （见 `movepart::prepare_copy`）。
///
/// 块设备的整盘 O_EXCL 已在打开时挡住挂载 / swap / 他人的独占打开，这里的逐分区探测
/// 是纵深防御，覆盖不守约定的并发写者留下的内核视图
#[cfg(target_os = "linux")]
pub fn ensure_table_write_idle(src: &FileSource) -> Result<(), crate::outcome::RunOutcome> {
    if src.is_block {
        return ensure_whole_device_idle(src);
    }
    image_target_unused(src, WriteKind::Overwrite)
}

/// 块设备：盘节点自身与 sysfs 列出的每个分区节点都须空闲。分区列表读不出来
/// 按探测失败上抛——"确认不了哪些分区在用"不能读成"没有分区在用"
#[cfg(target_os = "linux")]
fn ensure_whole_device_idle(src: &FileSource) -> Result<(), crate::outcome::RunOutcome> {
    use crate::dev::OccupancyFact;
    use crate::outcome::RunOutcome;
    let disk = block_disk_name(src).map_err(RunOutcome::from)?;
    let sysdir = Path::new("/sys/block").join(&disk);
    // 盘节点用用户给出的路径：by-id / mapper 之类的别名在 /dev/<sysfs 名> 下不一定存在。
    // 它必须仍指 fd 打开的那台设备——探测按路径 stat，指错了设备就是用另一台的空闲
    // 状态给本次写入放行（判据与 fsops 其余节点解析同一份）
    crate::dev::ensure_node_is_device(Path::new(&src.path), &sysdir).map_err(|e| {
        RunOutcome::no_mutation(format!(
            "{} does not denote the device sysfs lists as {disk}: {e} — refusing (fail-closed)",
            src.path.display()
        ))
    })?;
    let mut nodes = vec![src.path.to_string_lossy().into_owned()];
    let rd = std::fs::read_dir(&sysdir).map_err(|e| {
        RunOutcome::no_mutation(format!("cannot list {} to confirm no partition is in use: {e} — refusing (fail-closed)", sysdir.display()))
    })?;
    for entry in rd {
        let entry = entry.map_err(|e| {
            RunOutcome::no_mutation(format!("cannot list {} to confirm no partition is in use: {e} — refusing (fail-closed)", sysdir.display()))
        })?;
        let path = entry.path();
        // sysfs 里同层混着 uevent/size/holders 等文件与 queue 等目录；只对目录取 partition。
        // 对文件拼 /partition 得到的是 ENOTDIR，不是"没有该属性"
        match std::fs::metadata(&path) {
            Ok(m) if m.is_dir() => {}
            Ok(_) => continue,
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => {
                return Err(RunOutcome::no_mutation(format!(
                    "cannot stat {} to tell whether it is a partition: {e} — refusing (fail-closed)",
                    path.display()
                )));
            }
        }
        match std::fs::metadata(path.join("partition")) {
            Ok(_) => {
                let dev = format!("/dev/{}", entry.file_name().to_string_lossy());
                // 节点名从 sysfs 目录名推得，实际落点由设备号决定：同层存在另一台设备
                // 的同名节点时，探测会查错设备。按该条目自己的 sysfs 设备号核对
                crate::dev::ensure_node_is_device(Path::new(&dev), &path).map_err(|e| {
                    RunOutcome::no_mutation(format!(
                        "{dev} does not denote the partition sysfs lists under {}: {e} — refusing (fail-closed)",
                        sysdir.display()
                    ))
                })?;
                nodes.push(dev);
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(RunOutcome::no_mutation(format!(
                    "cannot stat {} to tell whether it is a partition: {e} — refusing (fail-closed)",
                    path.join("partition").display()
                )));
            }
        }
    }
    // 证据源只读一遍：整盘逐分区判定共用同一份 mountinfo 与 /proc/swaps
    let facts = crate::dev::collect_nodes(&nodes).map_err(|why| {
        OccupancyFact::Unreadable.outcome(format!(
            "cannot confirm {disk} is unused: {why} — refusing (fail-closed)"
        ))
    })?;
    for (node, facts) in nodes.into_iter().zip(facts) {
        match crate::dev::verdict(crate::dev::OpShape::Overwrite, &facts) {
            Occupancy::Idle => {}
            Occupancy::Mounted => {
                return Err(OccupancyFact::Occupied.outcome(format!(
                    "{node} is mounted — unmount it before rewriting the partition table"
                )));
            }
            Occupancy::ActiveSwap => {
                return Err(OccupancyFact::Occupied.outcome(format!(
                    "{node} is active swap — run swapoff before rewriting the partition table"
                )));
            }
            Occupancy::Held(holders) => {
                return Err(OccupancyFact::Occupied.outcome(format!(
                    "{node} is held by {} — deactivate that stack first (the new table would contradict the live stack)",
                    holders.join(", ")
                )));
            }
            Occupancy::LoopBacked { via, .. } => {
                return Err(OccupancyFact::Occupied.outcome(format!(
                    "{node} is the backing of {via}, which is in use — unmount/swapoff/deactivate that loop first \
                     (the new table would contradict the layout the loop reports)"
                )));
            }
            Occupancy::Member { detail } => return Err(OccupancyFact::Occupied.outcome(detail)),
        }
    }
    Ok(())
}

/// 非 Linux 存根。该平台没有块设备节点与 loop 子系统：挂载、swap、块设备栈持有这些
/// 占用来源根本不存在，不是"探测不到"——读不到等于已证明不存在，故这里不构成未知
#[cfg(not(target_os = "linux"))]
pub fn ensure_idle_before_write(_src: &FileSource, _part: u32, _start_bytes: u64, _kind: WriteKind) -> Result<(), crate::outcome::RunOutcome> {
    Ok(())
}

/// 非 Linux 存根：该平台上没有 loop 枚举与 /proc 挂载表，占用闸只存在于 Linux
#[cfg(not(target_os = "linux"))]
pub fn ensure_table_write_idle(_src: &FileSource) -> Result<(), crate::outcome::RunOutcome> {
    Ok(())
}

/// 内核记录的映射区间是否就是请求的 `[off, off + len)`
#[cfg(target_os = "linux")]
pub(crate) fn mapping_covers(map: &LoopMapping, off: u64, len: u64) -> bool {
    map.offset == off && map.sizelimit == len
}

/// attach 之后的自证：内核记录里的 backing、offset、sizelimit 必须与本次请求一致。
/// 只在这一读上成立——随后按节点路径执行的 FS/表工具会各自重开该节点，那一段不在
/// 本进程可控范围（见 [`attach_loop`] 的返回值说明）
#[cfg(target_os = "linux")]
pub(crate) fn verify_loop_mapping(src: &FileSource, loopdev: &str, off: u64, len: u64) -> Result<(), FsError> {
    use std::os::unix::fs::MetadataExt;
    let md = src.metadata().map_err(|e| {
        FsError::Io(io::Error::other(format!("cannot stat the open target to verify {loopdev}: {e}")))
    })?;
    // backing 判据与占用探测同一份：文件按 (dev, ino)、块设备按设备号
    let want = if src.is_block {
        BackingMatch::Block { rdev: md.rdev() }
    } else {
        BackingMatch::File { dev: md.dev(), ino: md.ino() }
    };
    let Some(map) = loop_mapping_of_node(Path::new(loopdev)).map_err(|e| {
        FsError::Io(io::Error::other(format!(
            "losetup reported {loopdev} but its kernel mapping cannot be read: {e}"
        )))
    })?
    else {
        return Err(FsError::CommandFailed(format!(
            "losetup reported {loopdev} but the kernel reports no backing file bound to it"
        )));
    };
    if !want.matches(&map.backing) {
        return Err(FsError::CommandFailed(format!(
            "{loopdev} is bound to a different object than the target this command opened \
             (kernel: backing device {} inode {} rdev {}) — it does not map the requested bytes",
            map.backing.dev, map.backing.ino, map.backing.rdev
        )));
    }
    // 两个数字都取自 `LOOP_GET_STATUS64` 的 lo_offset/lo_sizelimit，就是 losetup 传入
    // 的那一对；不等即本次映射的区间不是请求的区间（例如同名的旧 loop 未被替换）
    if !mapping_covers(&map, off, len) {
        return Err(FsError::CommandFailed(format!(
            "{loopdev} maps bytes {}..{} (limit {}) but the request was {off}..{} — \
             refusing to hand a differently mapped device to the filesystem tool",
            map.offset,
            map.offset.saturating_add(map.sizelimit),
            map.sizelimit,
            off.saturating_add(len)
        )));
    }
    Ok(())
}

/// losetup 映射 [off, off+len) 字节区间（镜像路径共用底层）。
/// --sizelimit 安全必需：缺省时 loop 延伸到镜像末端，resize 会越界写坏后续数据
/// （losetup(8)：--sizelimit size = 数据终点为起点之后不超过 size 字节）；
/// 非 512 扇区镜像必须一次带 --sector-size（内核 ≥4.14）：先以 512 映射虽能成功，
/// 但 loop 设备扇区几何错误，FS/表工具按错误扇区解析会写坏数据
pub(crate) fn attach_loop(src: &FileSource, off: u64, len: u64) -> Result<String, FsError> {
    // `--sizelimit` 的入参自证：losetup 对零长度与越界区间照单全收，映射出的设备
    // 直到写入时才暴露越界。先判，避免把"不代表该分区"的设备交给后续工具
    if len == 0 {
        return Err(FsError::invalid("refusing to map a zero-length range with losetup"));
    }
    let end = off
        .checked_add(len)
        .ok_or_else(|| FsError::invalid("loop range overflows the byte offset space"))?;
    if end > src.size {
        return Err(FsError::invalid(format!(
            "loop range {off}..{end} exceeds the target size {} — refusing",
            src.size
        )));
    }
    let base_args: Vec<String> = if src.sector_size != 512 {
        vec![
            "-o".into(), off.to_string(), "--sizelimit".into(), len.to_string(),
            "--sector-size".into(), src.sector_size.to_string(),
        ]
    } else {
        vec!["-o".into(), off.to_string(), "--sizelimit".into(), len.to_string()]
    };
    // 镜像用的是打开时刻由 fd 解析出的名字：命令返回后命令行路径可能已被改名或替换，
    // 按它 attach 得到的是另一个文件的映射，而写入仍走已打开的句柄
    // 块设备给节点本身：`off`/`len` 以该设备为坐标原点。loop 设备的数据路径是它的
    // backing 文件，而 backing 的字节坐标只在原 loop 无偏移时才与 loop 一致
    let img = if src.is_block { src.path.clone() } else { src.identity.data_path().to_path_buf() };
    // inode 比对只在 unix 上成立，且只对镜像有意义：块设备的指纹取自 backing（loop 设备）
    // 或全零（真块设备节点），与节点路径的 inode 不是同一个对象
    #[cfg(unix)]
    if !src.is_block {
        match src.fingerprint.still_names(&img) {
            Ok(Some(true)) => {}
            Ok(Some(false)) => {
                return Err(FsError::invalid(format!(
                    "{} no longer names the file this command opened — refusing to map a different file",
                    img.display()
                )))
            }
            // 无从判断等于没核对过：即将把路径交给 losetup，不拿"大概还是那个文件"去换
            Ok(None) => {
                return Err(FsError::invalid(format!(
                    "no file identity was recorded for {} — refusing to map it without confirming which file it is",
                    img.display()
                )))
            }
            Err(e) => return Err(FsError::invalid(format!("cannot re-check {} before mapping it: {e}", img.display()))),
        }
    }
    let mut show = tool_command("losetup")?
        .args(&base_args)
        .arg("-f")
        .arg("--show")
        .arg(&img)
        .stdin(Stdio::null())
        .output()
        .map_err(FsError::from)?;
    if !show.status.success() && src.sector_size == 512 {
        // offset 对齐异常等少数情况需要 --sector-size，512 路径再带该选项重试一次
        show = tool_command("losetup")?
            .args(["-o", &off.to_string(), "--sizelimit", &len.to_string(), "--sector-size", &src.sector_size.to_string(), "-f", "--show"])
            .arg(&img)
            .stdin(Stdio::null())
            .output()
            .map_err(FsError::from)?;
    }
    if !show.status.success() {
        return Err(FsError::command("losetup", &show));
    }
    // 返回的名字会被直接拼成 /dev 路径并交给格式化/搬运工具：只接受 losetup 的
    // 标准设备名。异常输出（空串、带附加说明、别的路径）说明本次映射不可定位，
    // 继续用等于把写入指向一个未经验证的节点
    let dev = String::from_utf8_lossy(&show.stdout).trim().to_string();
    let well_formed = dev
        .strip_prefix("/dev/loop")
        .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()));
    if !well_formed {
        return Err(FsError::CommandFailed(format!(
            "losetup --show returned {dev:?}, which is not a /dev/loopN device name — refusing to use it"
        )));
    }
    // 名字只是路径：按内核记录核对它此刻绑的是本次请求的对象与区间。不符即撤销
    // 这条映射再拒绝——交给后续工具等于按错误几何写盘。核对之后到工具自己重开节点
    // 之间仍有窗口，那一段只能由工具按路径解释，本进程无法消除
    #[cfg(target_os = "linux")]
    if let Err(e) = verify_loop_mapping(src, &dev, off, len) {
        release_loop(&dev);
        return Err(e);
    }
    Ok(dev)
}

/// 解除 loop 映射（losetup -d），随后等 udev 事件队列排空：
/// detach 引发的 remove 事件可能仍在处理中，紧接着复用同一 loop 设备会读到过期状态
/// （udevadm settle = 等待队列内事件处理完）。上限 5s：正常 detach 远快于此，超时只提示，
/// 不判 detach 失败（映射已解除，仅队列清空未确认）；udevadm 缺失（非 systemd 环境）忽略
///
/// 返回值只描述 `losetup -d` 的结局：调用点据此把"映射可能仍占着"记成进程级待办，
/// 而不是让它止步于一行 warning——那会让"FS 步成功 + loop 未解绑"以 0 退出
pub(crate) fn detach_loop(loopdev: &str) -> ReleaseRunOutcome {
    let release = match tool_command("losetup") {
        Err(e) => ReleaseRunOutcome::Unavailable(e.to_string()),
        Ok(mut cmd) => match cmd.arg("-d").arg(loopdev).status() {
            Ok(st) if st.success() => ReleaseRunOutcome::Released,
            Ok(st) => ReleaseRunOutcome::Failed(crate::fsops::describe_exit(&st)),
            Err(e) => ReleaseRunOutcome::Unavailable(e.to_string()),
        },
    };
    if let Ok(mut udevadm) = tool_command("udevadm") {
        match udevadm.args(["settle", "--timeout=5"]).status() {
            Ok(st) if !st.success() => eprintln!(
                "warning: udevadm settle timed out after 5s — {loopdev} is detached, but the udev event queue may not be empty"
            ),
            _ => {}
        }
    }
    release
}

/// 进程级资源释放失败清单。`Drop` 拿不到调用方的返回值，也未参与 `Result` 的构造，
/// 但它描述的"资源仍被占用"必须能影响退出码——调用链末端（`main`）统一取出并浮出，
/// 成功路径据此降级为"完成但有待办"（5）。只在进程内生效：本工具一次运行一个命令
static RELEASE_LEAKS: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

pub(crate) fn note_release_leak(msg: String) {
    RELEASE_LEAKS.lock().unwrap().push(msg);
}

/// 取出并清空本进程累积的资源释放待办。顺带打印，使 stderr 与退出码同源
pub(crate) fn take_release_leaks() -> Vec<String> {
    std::mem::take(&mut *RELEASE_LEAKS.lock().unwrap())
}

/// 释放动作的失败描述；`Released` 为 `None`
pub(crate) fn release_failure(outcome: ReleaseRunOutcome) -> Option<String> {
    match outcome {
        ReleaseRunOutcome::Released => None,
        ReleaseRunOutcome::Failed(why) | ReleaseRunOutcome::Unavailable(why) => Some(why),
    }
}
