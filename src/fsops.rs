//! FS 层外部工具链调用：offset loop 映射 + e2fsck/resize2fs/mkfs。
//!
//! 严格 Linux。非 Linux 平台编译为显式拒绝存根。

use crate::dev::FileSource;
use crate::fsid::is_ext;
use std::ffi::{OsStr, OsString};
use std::io;
#[cfg(target_os = "linux")]
use std::path::Path;
use std::path::PathBuf;
use std::process::{Command, Stdio};

/// FS 层失败的分类。fsops 只回答"操作层面发生了什么"，"这该算拒绝还是故障"要结合
/// durable boundary 才能回答，属命令层语义（映射见 `outcome::From<FsError> for Fail`）。
///
/// 只有三类是**在发生处就能确定**的，故单列；其余一律 `Io`——环境故障是最保守的缺省，
/// 它不承诺"改参数重试有意义"。用 io::ErrorKind 反推这三类是行不通的：
/// kind 一旦成形就分不出"类型不认得"与"设备读不到"
#[derive(Debug)]
pub enum FsError {
    /// 该类型/该操作没有接线的工具（不认得的 FS、本工具不负责的组合）
    UnsupportedFs(String),
    /// 参数非法，或目标现状与请求不符（分区不存在/为空、容器分区、尺寸不是扇区倍数）
    InvalidArgument(String),
    /// PATH 里找不到必需的工具
    ToolMissing(String),
    /// 读写、权限、挂载态等环境故障
    Io(io::Error),
    /// 外部工具跑起来了但非零退出
    CommandFailed(String),
}

impl FsError {
    fn unsupported(msg: impl Into<String>) -> Self {
        Self::UnsupportedFs(msg.into())
    }

    fn invalid(msg: impl Into<String>) -> Self {
        Self::InvalidArgument(msg.into())
    }

    fn missing(msg: impl Into<String>) -> Self {
        Self::ToolMissing(msg.into())
    }

    /// 外部工具非零退出：`<tool> failed: <stderr 原文>`
    fn command(tool: &str, out: &std::process::Output) -> Self {
        Self::CommandFailed(format!("{tool} failed: {}", String::from_utf8_lossy(&out.stderr).trim()))
    }
}

impl std::fmt::Display for FsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnsupportedFs(m)
            | Self::InvalidArgument(m)
            | Self::ToolMissing(m)
            | Self::CommandFailed(m) => f.write_str(m),
            Self::Io(e) => write!(f, "{e}"),
        }
    }
}

impl From<io::Error> for FsError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

/// 越过 durable boundary 之后（`execute_*` 内部），`FsError` 的分类不再有出口语义：
/// 那里的任何失败对外都只能是 `Failed`（盘可能已改变）。压平是显式的，不是 From——
/// 需要区分分类的地方必须自己表态
impl From<FsError> for io::Error {
    fn from(e: FsError) -> Self {
        io::Error::other(e.to_string())
    }
}

pub fn require_linux() -> Result<(), FsError> {
    if cfg!(target_os = "linux") {
        Ok(())
    } else {
        Err(FsError::Io(io::Error::new(
            io::ErrorKind::Unsupported,
            "this operation requires Linux (build/runtime platform)",
        )))
    }
}

pub fn require_root() -> Result<(), FsError> {
    #[cfg(target_os = "linux")]
    {
        // geteuid = POSIX.1（man geteuid）实际 UID 判定，非有效权限位
        // SAFETY: geteuid 无参数、不访问内存，恒成功
        if unsafe { libc::geteuid() } != 0 {
            return Err(FsError::Io(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "root required (losetup/mkfs/resize need kernel privileges)",
            )));
        }
        Ok(())
    }
    #[cfg(not(target_os = "linux"))]
    {
        Err(FsError::Io(io::Error::new(io::ErrorKind::Unsupported, "not linux")))
    }
}

/// 工具存在性守卫：执行前 fail-fast 确认工具存在且可执行
fn find_tool(name: &str) -> Result<PathBuf, FsError> {
    let path = std::env::var_os("PATH")
        .ok_or_else(|| FsError::missing("PATH unset — cannot locate required tools"))?;
    for dir in std::env::split_paths(&path) {
        let p = dir.join(name);
        // 需可执行：存在但无 x 位时继续向后找，避免错误推迟到 spawn 才以裸 EACCES 冒出
        if p.is_file() && is_executable(&p) {
            return Ok(p);
        }
    }
    Err(FsError::missing(format!("required tool not found in PATH: {name}")))
}

/// 执行位判定：stat(2) 得到的 st_mode 中 S_IXUSR|S_IXGRP|S_IXOTH（0o111）任一置位即视为
/// 可执行（权限位语义见 chmod(2)）
#[cfg(unix)]
fn is_executable(p: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p).is_ok_and(|m| m.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(_p: &std::path::Path) -> bool {
    true
}

/// 执行外部工具。参数用 `AsRef<OsStr>` 收：绝大多数调用点传 `&[&str]` 即可，
/// 卷标这类任意字节的参数靠它原样透传（见 os_bytes），不经任何编码转换
pub(crate) fn run<S: AsRef<OsStr>>(tool: &str, args: &[S]) -> Result<std::process::Output, FsError> {
    let path = find_tool(tool)?;
    // 输出需按格式解析（resize2fs -P、dumpe2fs -h 等），固定 LC_ALL=C 防本地化翻译破坏解析
    Command::new(path)
        .args(args)
        .stdin(Stdio::null())
        .env("LC_ALL", "C")
        .output()
        .map_err(FsError::from)
}

/// 任意字节 → 命令行参数。unix 下按原字节构造：execve 的 argv 本就是字节串，无编码校验。
/// 非 unix 平台没有这条调用路径（mkswap 只存在于 Linux），那里退化为有损解码以保持可编译
#[cfg(unix)]
fn os_bytes(b: &[u8]) -> OsString {
    use std::os::unix::ffi::OsStrExt;
    std::ffi::OsStr::from_bytes(b).to_os_string()
}

#[cfg(not(unix))]
fn os_bytes(b: &[u8]) -> OsString {
    OsString::from(String::from_utf8_lossy(b).into_owned())
}

/// 同 run，但从 stdin 喂入脚本（sfdisk -N 的分区描述只走 stdin）
#[cfg(target_os = "linux")]
pub(crate) fn run_input(tool: &str, args: &[&str], input: &str) -> Result<std::process::Output, FsError> {
    let path = find_tool(tool)?;
    let mut child = Command::new(path)
        .args(args)
        .stdin(Stdio::piped())
        .env("LC_ALL", "C")
        .spawn()
        .map_err(FsError::from)?;
    use std::io::Write as _;
    let mut stdin = child.stdin.take().expect("stdin piped");
    if let Err(e) = stdin.write_all(input.as_bytes()) {
        // 写 stdin 失败时子进程可能还在等输入，显式终止防僵留
        #[allow(clippy::let_underscore_must_use)] // 主错误已上报；子进程回收失败只影响资源
        {
            let _ = child.kill();
            let _ = child.wait();
        }
        return Err(FsError::Io(e));
    }
    drop(stdin); // 关闭 stdin 让 sfdisk 看到输入结束
    child.wait_with_output().map_err(FsError::from)
}

/// e2fsck 退出码判定——resize 语境（man e2fsck EXIT CODE：各项按位或求和）。bit1（REBOOT）
/// 置位条件（e2fsprogs e2fsck/unix.c）：FS 被修改且 ctx->mount_flags & EXT2_MF_ISROOT——改了
/// root fs 需重启才能继续 resize，2/3 一律中断（未挂载镜像上通常不出现）。
/// 0/1 通过，4 = 有未修正错误即拒绝。
/// 2/3/4 归 `CommandFailed`：三者都说明工具**运行过**——2/3 自述改过文件系统、
/// 4 在 -p 下仍会自动修复过——"不能证明已写"不等于"已证明未写"，不足以支撑
/// `Io → Infra` 的"确定未写盘"承诺。`Failed` 的语义正是不对是否落盘作断言
fn check_e2fsck_for_resize(code: i32) -> Result<(), FsError> {
    match code {
        0 | 1 => Ok(()),
        2 | 3 => Err(FsError::CommandFailed(
            "e2fsck: root filesystem was modified, reboot required before resizing".into(),
        )),
        4 => Err(FsError::CommandFailed(
            "e2fsck: uncorrected errors (exit 4), refuse resize".into(),
        )),
        c => Err(FsError::CommandFailed(format!("e2fsck infrastructure failure (exit {c})"))),
    }
}

/// e2fsck 退出码判定——check 语境。检查命令的本职就是修复：0（干净）、1（已修复）
/// 是成功；2/3 = 修复完成 + REBOOT 位（该位来自内核的 root 挂载标记，只影响"能否
/// 立即继续 resize"，与修复成败无关），同为成功。仅 4（-fp 后仍有未修正错误）算
/// 检查失败；8/16 等为基础设施故障。不复用 resize 判据——那套拒绝 2/3 的理由在
/// check 上没有对应物，照搬会把"修复成功"报成 Failed(30)
fn check_e2fsck_for_check(code: i32) -> Result<(), FsError> {
    match code {
        0..=3 => Ok(()),
        4 => Err(FsError::CommandFailed(
            "e2fsck: uncorrected errors remain (exit 4)".into(),
        )),
        c => Err(FsError::CommandFailed(format!("e2fsck infrastructure failure (exit {c})"))),
    }
}

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
    let _fstype = rest.get(sep + 1)?;
    let source = rest.get(sep + 2)?;
    let (maj, min) = dev.split_once(':')?;
    Some(MountEntry {
        dev_no: (maj.parse().ok()?, min.parse().ok()?),
        mount_point: decode_octal(mnt),
        source: decode_octal(source),
    })
}

/// 读取 /proc/self/mountinfo；无法解析的行跳过
#[cfg(target_os = "linux")]
pub(crate) fn read_mounts() -> io::Result<Vec<MountEntry>> {
    let s = std::fs::read_to_string("/proc/self/mountinfo")?;
    Ok(s.lines().filter_map(parse_mountinfo).collect())
}

/// 设备占用探测结果（/proc/self/mountinfo + /proc/swaps，路径与 st_rdev 并集匹配）。
/// FS 操作要求分区未挂载、未作 swap 是本工具策略，内核并不强制（man resize2fs：
/// 内核支持在线扩容时可直接扩已挂载的 ext）；挂载态扩容走 xfs/btrfs 的临时挂载路径，
/// 不经此判定
#[cfg(target_os = "linux")]
enum Occupancy {
    /// 未挂载且非活动 swap
    Idle,
    /// 挂载表中命中
    Mounted,
    /// /proc/swaps 命中
    ActiveSwap,
}

/// 探测设备节点的占用状态。Err = 探测本身失败——调用方一律 fail-closed，
/// "确认不了空闲"不得当成"空闲"
#[cfg(target_os = "linux")]
fn probe_occupancy(dev: &str) -> io::Result<Occupancy> {
    use std::os::unix::fs::MetadataExt;
    let m = std::fs::metadata(dev).map_err(|e| {
        io::Error::other(format!("cannot stat {dev} to confirm it is unmounted: {e}"))
    })?;
    let rdev = m.rdev();
    let dev_no = (libc::major(rdev) as u64, libc::minor(rdev) as u64);
    let entries = read_mounts()?;
    if entries.iter().any(|e| e.dev_no == dev_no) {
        return Ok(Occupancy::Mounted);
    }
    let swaps = std::fs::read_to_string("/proc/swaps")?;
    for line in swaps.lines().skip(1) {
        if let Some(field) = line.split_whitespace().next() {
            // 同一设备的两种判据取并集：路径相同，或 st_rdev 相同。后者 stat 失败时
            // 不构成"确认安全"，但路径相等这条仍能命中，避免漏检
            if field == dev || std::fs::metadata(field).is_ok_and(|fm| fm.rdev() == rdev) {
                return Ok(Occupancy::ActiveSwap);
            }
        }
    }
    Ok(Occupancy::Idle)
}

/// FS 步的占用闸：占用或探测失败都拒绝（fail-closed——探测失败若被当作
/// "未挂载"，会对已挂载的 FS 执行 resize，那是数据损坏）
#[cfg(target_os = "linux")]
fn require_unmounted(dev: &str) -> Result<(), FsError> {
    match probe_occupancy(dev) {
        Ok(Occupancy::Idle) => Ok(()),
        Ok(o) => Err(FsError::Io(io::Error::other(match o {
            Occupancy::Mounted => format!("{dev} is mounted — unmount/deactivate first (FS operations require an unmounted partition)"),
            _ => format!("{dev} is active as swap — unmount/deactivate first (FS operations require an unmounted partition)"),
        }))),
        Err(e) => Err(FsError::Io(io::Error::other(format!(
            "cannot confirm {dev} is unmounted: {e} — refusing (fail-closed)"
        )))),
    }
}
#[cfg(not(target_os = "linux"))]
fn require_unmounted(_dev: &str) -> Result<(), FsError> {
    Ok(())
}

/// 写表路径的占用前置闸：分区在首次落盘前须空闲。调用点在 prepare 层（movepart 的
/// prepare_resize / prepare_apply）与 MBR resize 命令层——那是任何写盘的唯一必经关口，
/// 把"表已写、FS 步被占"的 PARTIAL(20) 变成 REFUSED(10)：占用是请求与现状不匹配，
/// 不是盘故障。块设备另有整盘 O_EXCL 独占打开这道内核级防线（任一分区被挂载或作
/// swap 时 open 即 EBUSY），本闸对其是纵深防御；镜像是本工具内唯一没有占用判据的
/// 目标（不存在分区节点可查），直接放行（经 loop 挂载的镜像由 FS 步的设备级检查兜底）。
/// `start_bytes` = 分区起始字节（与 find_block_partition_node 的换算基准一致）
#[cfg(target_os = "linux")]
pub fn ensure_idle_before_write(src: &FileSource, part: u32, start_bytes: u64) -> Result<(), crate::outcome::Fail> {
    use crate::outcome::Fail;
    if !src.is_block {
        return Ok(());
    }
    let node = find_block_partition_node(src, part, start_bytes).map_err(Fail::from)?;
    match probe_occupancy(&node) {
        Ok(Occupancy::Idle) => Ok(()),
        Ok(Occupancy::Mounted) => Err(Fail::refused(format!(
            "{node} is mounted — unmount before resizing (moving a mounted partition's extents invalidates the live mount)"
        ))),
        Ok(Occupancy::ActiveSwap) => Err(Fail::refused(format!("{node} is active swap — run swapoff first"))),
        Err(e) => Err(Fail::infra(format!(
            "cannot confirm {node} is unmounted: {e} — refusing (fail-closed)"
        ))),
    }
}
#[cfg(not(target_os = "linux"))]
pub fn ensure_idle_before_write(_src: &FileSource, _part: u32, _start_bytes: u64) -> Result<(), crate::outcome::Fail> {
    Ok(())
}

/// losetup 映射 [off, off+len) 字节区间（镜像路径共用底层）。
/// --sizelimit 安全必需：缺省时 loop 延伸到镜像末端，resize 会越界写坏后续数据
/// （losetup(8)：--sizelimit size = 数据终点为起点之后不超过 size 字节）；
/// 非 512 扇区镜像必须一次带 --sector-size（内核 ≥4.14）：先以 512 映射虽能成功，
/// 但 loop 设备扇区几何错误，FS/表工具按错误扇区解析会写坏数据
fn attach_loop(src: &FileSource, off: u64, len: u64) -> Result<String, FsError> {
    let losetup = find_tool("losetup")?;
    let base_args: Vec<String> = if src.sector_size != 512 {
        vec![
            "-o".into(), off.to_string(), "--sizelimit".into(), len.to_string(),
            "--sector-size".into(), src.sector_size.to_string(),
        ]
    } else {
        vec!["-o".into(), off.to_string(), "--sizelimit".into(), len.to_string()]
    };
    let args_str: Vec<&str> = base_args.iter().map(|s| s.as_str()).collect();
    let img = src.path.clone();
    let mut show = Command::new(&losetup)
        .args(&args_str)
        .arg("-f")
        .arg("--show")
        .arg(&img)
        .stdin(Stdio::null())
        .output()
        .map_err(FsError::from)?;
    if !show.status.success() && src.sector_size == 512 {
        // offset 对齐异常等少数情况需要 --sector-size，512 路径再带该选项重试一次
        show = Command::new(&losetup)
            .args(["-o", &off.to_string(), "--sizelimit", &len.to_string(), "--sector-size", &src.sector_size.to_string(), "-f", "--show"])
            .arg(&img)
            .stdin(Stdio::null())
            .output()
            .map_err(FsError::from)?;
    }
    if !show.status.success() {
        return Err(FsError::command("losetup", &show));
    }
    Ok(String::from_utf8_lossy(&show.stdout).trim().to_string())
}

/// 解除 loop 映射（losetup -d），随后等 udev 事件队列排空：
/// detach 引发的 remove 事件可能仍在处理中，紧接着复用同一 loop 设备会读到过期状态
/// （udevadm settle = 等待队列内事件处理完）。上限 5s：正常 detach 远快于此，超时只提示，
/// 不判 detach 失败（映射已解除，仅队列清空未确认）；udevadm 缺失（非 systemd 环境）忽略
fn detach_loop(loopdev: &str) {
    if let Ok(losetup) = find_tool("losetup") {
        // 资源释放失败不该阻断业务（数据与布局已正确），但循环设备泄漏会让后续
        // losetup 找不到空闲设备——属于用户需要知道的状态，故告警而非静默丢弃
        match Command::new(&losetup).arg("-d").arg(loopdev).status() {
            Ok(st) if st.success() => {}
            Ok(st) => eprintln!(
                "warning: `losetup -d {loopdev}` exited with {} — the loop device may still be attached",
                st.code().unwrap_or(-1)
            ),
            Err(e) => eprintln!("warning: cannot run `losetup -d {loopdev}`: {e} — the loop device may still be attached"),
        }
    }
    if let Ok(udevadm) = find_tool("udevadm") {
        match Command::new(&udevadm).args(["settle", "--timeout=5"]).status() {
            Ok(st) if !st.success() => eprintln!(
                "warning: udevadm settle timed out after 5s — {loopdev} is detached, but the udev event queue may not be empty"
            ),
            _ => {}
        }
    }
}

/// FS 操作的目标范围：常规 = 表内分区；superfloppy = 整盘（无表，FS 即盘）；
/// overlay = 分区内任意字节区间（OpenWrt RW 层，须经 offset loop 映射）
pub enum DeviceScope {
    Partition(u32),
    Whole,
    Range(u64, u64),
}

fn scope_byte_range(src: &FileSource, scope: &DeviceScope) -> Result<(u64, u64), FsError> {
    match scope {
        DeviceScope::Partition(p) => partition_byte_range(src, *p),
        DeviceScope::Whole => Ok((0, src.size)),
        DeviceScope::Range(off, len) => Ok((*off, *len)),
    }
}

/// 按范围执行 FS 操作的统一入口，with_partition_device 的公共底层
fn with_scope_device<F>(src: &FileSource, scope: &DeviceScope, f: F) -> Result<(), FsError>
where
    F: FnOnce(&str) -> Result<(), FsError>,
{
    require_linux()?;
    require_root()?;
    if src.is_block {
        // 块设备：分区 = 定位分区节点（/sys/block/<disk>/<part>/start 匹配），不经 loop；
        // 整盘 = 盘节点本身；Range = 无"偏移节点"概念，必须 loop 映射（backing=整盘）。
        // 区间在此只求一次：分区起点同时作为节点匹配的 want_start
        let (off, len) = scope_byte_range(src, scope)?;
        let dev = match scope {
            DeviceScope::Partition(p) => find_block_partition_node(src, *p, off)?,
            DeviceScope::Whole => src.path.to_string_lossy().into_owned(),
            DeviceScope::Range(..) => {
                let loopdev = attach_loop(src, off, len)?;
                let res = require_unmounted(&loopdev).and_then(|()| f(&loopdev));
                detach_loop(&loopdev);
                return res;
            }
        };
        return require_unmounted(&dev).and_then(|()| f(&dev));
    }
    let (off, len) = scope_byte_range(src, scope)?;
    let loopdev = attach_loop(src, off, len)?;
    let res = require_unmounted(&loopdev).and_then(|()| f(&loopdev));
    detach_loop(&loopdev);
    res
}

/// 在分区上执行操作的统一入口。
/// - 镜像文件：`losetup -o <off> --sizelimit <len> [-S ss] -f --show <img>`
/// - 块设备：直接定位分区设备节点（/sys/block/<disk>/<part>/start 匹配），不经 loop
pub fn with_partition_device<F>(src: &FileSource, part: u32, f: F) -> Result<(), FsError>
where
    F: FnOnce(&str) -> Result<(), FsError>,
{
    with_scope_device(src, &DeviceScope::Partition(part), f)
}

/// 在 /sys/block/<disk>/<part>/ 按 start 匹配分区设备节点（块设备路径用）。
/// want_start = 分区起始字节，调用方已由 partition_byte_range 求得。
/// start/size 属性为内核 sysfs-block ABI（Documentation/ABI/testing/sysfs-block，
/// 单位恒为 512 字节扇区，与设备逻辑块大小无关），换算字节偏移须用 512 而非 src.sector_size
fn find_block_partition_node(src: &FileSource, part: u32, want_start: u64) -> Result<String, FsError> {
    #[cfg(target_os = "linux")]
    {
        let disk = src.path.file_name()
            .ok_or_else(|| FsError::invalid("no disk name"))?
            .to_string_lossy().to_string();
        let sys = Path::new("/sys/block").join(&disk);
        for entry in std::fs::read_dir(&sys).map_err(FsError::from)? {
            let entry = entry.map_err(FsError::from)?;
            let start_file = entry.path().join("start");
            let Ok(txt) = std::fs::read_to_string(&start_file) else { continue };
            // 解析失败等于"这个候选不成立"，跳过而非折叠成 0——0 不会匹配任何
            // 真实分区，但一个假装合法的数值比跳过更难排查
            let Ok(start_sectors) = txt.trim().parse::<u64>() else { continue };
            // sysfs 是外部输入：回绕出的假字节偏移可能撞上别的分区节点，乘法必须 checked
            if start_sectors.checked_mul(512) != Some(want_start) {
                continue;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            return Ok(format!("/dev/{name}"));
        }
        Err(FsError::invalid(format!("partition node for part {part} not found under /sys/block/{disk}")))
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (src, part, want_start);
        Err(FsError::Io(io::Error::new(io::ErrorKind::Unsupported, "not linux")))
    }
}

fn partition_byte_range(src: &FileSource, part: u32) -> Result<(u64, u64), FsError> {
    // 唯一实现在 gpt_policy::partition_bytes（GPT 优先、MBR 兜底，换算用表自身的 ss）。
    // 出口语义由 FsError 的既有映射承担：Refused ⇒ 请求与现状不符(10)，环境/盘内容故障 ⇒
    // Io(30)。本层不把分类重解释一遍——那会让"同一事实的第二处判据"重新长出来
    crate::gpt_policy::partition_bytes(src, part).map_err(|f| match f {
        crate::outcome::Fail::Refused(m) => FsError::invalid(m),
        crate::outcome::Fail::Infra(m) | crate::outcome::Fail::Failed(m) => FsError::Io(io::Error::other(m)),
    })
}

/// ext 最小尺寸估算：resize2fs -P 的最小块数 × dumpe2fs -h 的块大小；
/// 输出格式锚定 man 页示例（"Estimated minimum size of the filesystem: N" / "Block size: N"）。
/// 其余 FS 无已验证的输出格式，返回 None，由工具自身在缩容前拒绝
/// （shrink_fs 先于任何数据搬移执行，失败即安全终止）
pub fn fs_min_bytes(src: &FileSource, part: u32, fstype: &str) -> Result<Option<u64>, FsError> {
    if !is_ext(fstype) {
        return Ok(None);
    }
    let mut result: Option<io::Result<u64>> = None;
    with_partition_device(src, part, |dev| {
        result = Some(min_bytes_ext(dev));
        Ok(())
    })?;
    match result {
        Some(r) => r.map(Some).map_err(FsError::from),
        None => Ok(None),
    }
}

fn parse_num_field(text: &str, prefix: &str) -> io::Result<u64> {
    text.lines()
        .find_map(|l| l.trim().strip_prefix(prefix)?.trim().parse().ok())
        .ok_or_else(|| io::Error::other(format!("unexpected tool output, missing {prefix:?}")))
}

/// ext 最小尺寸（字节）= -P 估计块数 × 块大小。
/// 取值靠解析 CLI 输出，非稳定机器接口（输出格式随版本可能变化，故解析失败即报错、
/// 不猜默认值）：resize2fs -P 打印 -M 缩到最小的块数估计（man resize2fs OPTIONS；
/// 该 man 的 KNOWN BUGS 指出 1K/2K 块文件系统上估计值可能不准），
/// 块大小取自 dumpe2fs -h 的 "Block size:" 行（man dumpe2fs：-h 只读超级块）
fn min_bytes_ext(dev: &str) -> io::Result<u64> {
    let out = run("resize2fs", &["-P", dev])?;
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    if !out.status.success() {
        return Err(io::Error::other(format!("resize2fs -P failed: {}", text.trim())));
    }
    let blocks = parse_num_field(&text, "Estimated minimum size of the filesystem:")?;
    let out2 = run("dumpe2fs", &["-h", dev])?;
    let text2 = format!("{}{}", String::from_utf8_lossy(&out2.stdout), String::from_utf8_lossy(&out2.stderr));
    if !out2.status.success() {
        return Err(io::Error::other(format!("dumpe2fs -h failed: {}", text2.trim())));
    }
    let bs = parse_num_field(&text2, "Block size:")?;
    // 两个因子都出自外部工具的 stdout：回绕的"最小尺寸"会放行本应拒绝的缩容请求
    //（分区末端切进 FS 元数据），方向危险，溢出按工具输出异常上抛
    blocks.checked_mul(bs)
        .ok_or_else(|| io::Error::other(format!("resize2fs -P × dumpe2fs block size overflows ({blocks} × {bs})")))
}

/// swap 重建：mkswap -U <uuid> [-L <label>]，保持 UUID/卷标以维持 fstab 兼容
/// （-U/-L 见 man mkswap：UUID 存取同序、无端转换，全零视为未设置走随机生成）。
/// 卷标按字节透传：sws_volume 是不做编码校验的固定宽度字段，转成 String 再写回必然失真。
/// 失败由调用方降级为日志（swap 内容可弃，但 fstab 指向的 UUID 需人工 mkswap 恢复）
pub fn recreate_swap(src: &FileSource, part: u32, identity: (Option<[u8; 16]>, Option<Vec<u8>>)) -> Result<(), FsError> {
    with_partition_device(src, part, |dev| {
        let mut args: Vec<OsString> = Vec::new();
        if let Some(u) = &identity.0 {
            // swap UUID 按原字节序列化为标准 8-4-4-4-12（mkswap 存取同序，无混合端转换）
            let hex: String = u.iter().map(|b| format!("{b:02x}")).collect();
            let s = format!("{}-{}-{}-{}-{}", &hex[0..8], &hex[8..12], &hex[12..16], &hex[16..20], &hex[20..32]);
            args.extend([OsString::from("-U"), OsString::from(s)]);
        }
        if let Some(l) = &identity.1 {
            args.extend([OsString::from("-L"), os_bytes(l)]);
        }
        args.push(OsString::from(dev));
        let out = run("mkswap", &args)?;
        if !out.status.success() {
            return Err(FsError::command("mkswap", &out));
        }
        Ok(())
    })
}

/// 下取整到 unit 的整数倍
///
/// 用 div_euclid 而非 `/`：Rust 的整数除法对负数是向零取整，而这里要的是 floor
/// （如 pos = part_len−1MiB 相对桶界的回退），两者在负值上不同
fn floor_to_unit(v: i64, unit: i64) -> i64 {
    v.div_euclid(unit) * unit
}

/// 上取整到 unit 的整数倍（取整方向说明见 floor_to_unit）
fn ceil_to_unit(v: i64, unit: i64) -> i64 {
    let f = floor_to_unit(v, unit);
    if f == v {
        f
    } else {
        f + unit
    }
}

/// mkfs 前残留签名擦除：分区内固定区间整段写零，一张表覆盖已知备份超级块，
/// 不依赖旧 FS 识别。区间表布局取自 GParted erase_filesystem_signatures()
/// （GParted_Core.cc:4108-4120），是该工具的自定策略而非 FS/磁盘规范。
/// offset 为负 = 距分区尾，rounding = 对齐粒度；起点下取整到扇区界、末端上取整到
/// 扇区界，两端裁剪到分区界内，空区间丢弃、与已入表末项相同的相邻区间跳过。
fn erase_ranges(part_len: u64, ss: u64) -> Vec<(u64, u64)> {
    const K: u64 = 1024;
    const M: u64 = 1024 * K;
    const G: u64 = 1024 * M;
    const P: u64 = 1024 * G;
    // (offset, rounding, length)
    let raw: &[(i64, u64, u64)] = &[
        (0, 1, 512 * K),           // 头部：ZFS L0/L1 及全部前部超级块
        (64 * M as i64, 1, 4 * K), // btrfs 镜像超级块
        (256 * G as i64, 1, 4 * K),
        (P as i64, 1, 4 * K),
        (-(3087 * 512), 1, 512), // Promise FastTrack RAID
        (-(M as i64), 128 * K, 4 * K), // bcachefs 备份超级块（4 种桶对齐）
        (-(M as i64), 256 * K, 4 * K),
        (-(M as i64), 512 * K, 4 * K),
        (-(M as i64), M, 4 * K),
        (-(512 * K as i64), 256 * K, 768 * K), // 尾部：ZFS L2/L3、md、ATARAID、nilfs2
    ];
    let ss_i = ss as i64;
    let part_end = part_len as i64; // 实际容量远小于 2^63，i64 足以容纳负的中间量
    let mut out: Vec<(u64, u64)> = Vec::new();
    for &(off, rounding, len) in raw {
        // 起点：负偏移从分区尾回退，先按该行 rounding 下取整（bcachefs 备份超级块在
        // -1MiB 的桶对齐处，须先取整才能命中），再落到扇区界；越界部分由下方裁剪兜住
        let start = if off >= 0 {
            floor_to_unit(off, ss_i)
        } else {
            floor_to_unit(floor_to_unit(part_end + off, rounding as i64), ss_i)
        };
        // 末端上取整到整扇区（写入以扇区为单位；非正的末端经下方裁剪必然落空）
        let end = ceil_to_unit(start + len as i64, ss_i);
        let s = start.clamp(0, part_end) as u64;
        let e = end.clamp(0, part_end) as u64;
        if s < e && out.last() != Some(&(s, e - s)) {
            out.push((s, e - s));
        }
    }
    out
}

/// 对分区设备执行擦除：逐区间写零（4KiB 块），每步 sync
fn wipe_zero(dev: &str, ranges: &[(u64, u64)]) -> io::Result<()> {
    use std::io::{Seek, Write};
    let mut f = std::fs::OpenOptions::new().write(true).open(dev)?;
    let zero = [0u8; 4096];
    for &(off, len) in ranges {
        f.seek(std::io::SeekFrom::Start(off))?;
        let mut left = len;
        while left > 0 {
            let n = (left as usize).min(zero.len());
            f.write_all(&zero[..n])?;
            left -= n as u64;
        }
        f.sync_data()?;
    }
    Ok(())
}

/// fstype → 外部工具与参数。**唯一**一份支持列表：`mkfs` 与命令层的"先问再做"都读它，
/// 免得"能不能做"与"怎么做"两份清单分叉。
///
/// - ext 系直调 mke2fs -t extN（mkfs.extN 只是它的符号链接，man mke2fs）
/// - swap 用 mkswap（裸设备遇已有盘标头须 -f，分区节点不受影响，man mkswap）
/// - btrfs/f2fs/xfs 检测到已有文件系统时默认拒绝写入，须 -f 覆盖
///   （man mkfs.btrfs / mkfs.f2fs / mkfs.xfs）
/// - `-f` 语义各工具不类推：mkntfs 的 -f 是 fast 格式化而非 force（force 为 -F），
///   故 ntfs 不传 -f，让工具自身拒绝已有文件系统（man mkntfs）
struct MkfsTool<'a> {
    program: String,
    force: bool,
    /// `mke2fs -t <ext_type>`；生命周期跟着调用方给的 fstype
    ext_type: Option<&'a str>,
}

fn mkfs_tool(fstype: &str) -> Result<MkfsTool<'_>, FsError> {
    if fstype == "lvm2_pv" {
        return Err(FsError::unsupported(
            "cannot mkfs an LVM2 PV — to (re)create the PV use pvcreate(8), to wipe it use wipefs(8)",
        ));
    }
    let (program, force, ext_type) = match fstype {
        // mkfs.extN 只是 mke2fs 等价 -t extN 的符号链接（man mke2fs），直调 mke2fs 免依赖链接布局
        "ext2" | "ext3" | "ext4" => ("mke2fs".to_string(), false, Some(fstype)),
        "vfat" | "exfat" | "ntfs" => (format!("mkfs.{fstype}"), false, None),
        "xfs" | "btrfs" | "f2fs" => (format!("mkfs.{fstype}"), true, None),
        "swap" => ("mkswap".to_string(), false, None),
        other => {
            return Err(FsError::unsupported(format!(
                "unsupported fstype {other} (supported: ext2/3/4, xfs, btrfs, f2fs, vfat, exfat, ntfs, swap)"
            )));
        }
    };
    Ok(MkfsTool { program, force, ext_type })
}

/// 命令层"先问再做"的入口：这个 FS 类型**具备被创建的能力**吗——类型有没有接线、
/// 对应的外部工具在不在 PATH 且可执行。两问都必须在这里答完
///
/// 必须先问再开事务：拒绝的语义是"什么都没写"，而 mkfs 一旦开了事务就先落一条不可回滚
/// 屏障——一个拼错的类型名、或一个没装的工具包，都不该把目标锁在"未收尾"状态里等 `abandon`
pub fn mkfs_capability(fstype: &str) -> Result<(), FsError> {
    let tool = mkfs_tool(fstype)?;
    find_tool(&tool.program).map(|_| ()).map_err(|e| match e {
        FsError::ToolMissing(_) => FsError::missing(format!(
            "cannot mkfs {fstype}: requires `{}` (package: {}) — not found in PATH",
            tool.program,
            tool_package(&tool.program)
        )),
        e => e,
    })
}

/// mkfs：破坏分区数据，调用方须先取确认；执行前先擦残留签名（见 erase_ranges），
/// 防旧 btrfs/ZFS/RAID 备份超级块残留被 blkid 误认
pub fn mkfs(src: &FileSource, part: u32, fstype: &str) -> Result<(), FsError> {
    let tool = mkfs_tool(fstype)?;
    let (_, part_len) = partition_byte_range(src, part)?;
    let ss = src.sector_size;
    with_partition_device(src, part, |dev| {
        wipe_zero(dev, &erase_ranges(part_len, ss))?;
        let mut args: Vec<&str> = Vec::new();
        if tool.force {
            args.push("-f");
        }
        if let Some(t) = tool.ext_type {
            args.push("-t");
            args.push(t);
        }
        args.push(dev);
        let out = run(&tool.program, &args)?;
        if !out.status.success() {
            return Err(FsError::command(&tool.program, &out));
        }
        Ok(())
    })
}

/// btrfs 多设备拒绝：主 superblock @分区起点+0x10000 的 num_devices 字段（偏移 0x88，
/// u64 LE，内核 fs/btrfs ctree.h 字段序）。>1 时 resize/max 按 devid 作用于所映射的
/// 单个 member，"分区扩满即 FS 扩满"前提不成立，直接拒绝，多设备布局交用户手动处理
fn refuse_btrfs_multi_device_at(src: &FileSource, off: u64) -> Result<(), FsError> {
    let mut raw = [0u8; 8];
    src.read_at(off + 0x10000 + 0x88, &mut raw).map_err(FsError::from)?;
    if u64::from_le_bytes(raw) > 1 {
        return Err(FsError::unsupported(
            "btrfs filesystem spans multiple devices (num_devices > 1) — resize manually per btrfs-filesystem(8)",
        ));
    }
    Ok(())
}

/// FS 的 resize 支持情况——三态而非布尔，"不适用"与"不支持"必须分开：
/// 前者是本工具的契约边界（不该因此报错），后者是我们认得却做不了（必须事前拒绝）
enum ToolSupport {
    /// 需要这些工具；任一缺失即事前拒绝
    Tools(&'static [&'static str]),
    /// 契约上不负责：裸分区无 FS 可扩；LVM PV 的空间生效走 pvresize/lvextend 链
    NotApplicable,
    /// 认得出来但本操作不接线。**理由随变体携带**：笼统的"未接线"对用户无从下手，
    /// 而各调用点各写一句理由（lvm2_pv 要 lvreduce 链 / unknown 会写坏数据 / 其余未接线）
    /// 正是"同一事实多处来源"——那样每加一个类型都要改多处
    Unsupported(&'static str),
}

fn grow_support(fstype: &str) -> ToolSupport {
    use ToolSupport::*;
    match fstype {
        f if is_ext(f) => Tools(&["e2fsck", "resize2fs"]),
        "ntfs" => Tools(&["ntfsresize"]),
        "f2fs" => Tools(&["fsck.f2fs", "resize.f2fs"]),
        "xfs" => Tools(&["xfs_growfs"]),
        "btrfs" => Tools(&["btrfs"]),
        "vfat" => Tools(&["fatresize"]),
        // swap 不搬内容：扩后按原 UUID/卷标重建（recreate_swap → mkswap）
        "swap" => Tools(&["mkswap"]),
        "unknown" | "lvm2_pv" => NotApplicable,
        _ => Unsupported("not wired to a tool — pass --no-fs to change the partition only"),
    }
}

fn shrink_support(fstype: &str) -> ToolSupport {
    use ToolSupport::*;
    match fstype {
        f if is_ext(f) => Tools(&["e2fsck", "resize2fs"]),
        "ntfs" => Tools(&["ntfsresize"]),
        "btrfs" => Tools(&["btrfs"]),
        // LVM PV 缩容要求新末端之后没有已分配的 extent，需经 lvreduce/pvresize 链，本工具不做
        "lvm2_pv" => Unsupported(
            "requires the lvreduce/pvresize chain (not implemented here; see pvresize(8))",
        ),
        // 类型认不出来就无法先缩 FS：缩分区后 FS 越界写坏数据
        "unknown" => Unsupported(
            "filesystem type unrecognized — shrinking the partition without resizing the FS first would corrupt data",
        ),
        // 其余认得却不会缩的类型：唯一安全路径是先由该 FS 自己的工具缩。
        // 提示不能是 grow 的 "pass --no-fs"：--no-fs 与缩容互斥，那样等于给出错误指引
        _ => Unsupported(
            "not wired to a tool; the filesystem must be shrunk by its own tool first, and --no-fs cannot stand in (the new partition end would cut into filesystem metadata)",
        ),
    }
}

/// 工具所属包名（Debian 系）。写进错误信息是为了让自动化脚本能识别缺失项
/// 并自动安装后重试，而不必靠人读日志
fn tool_package(tool: &str) -> &'static str {
    match tool {
        "e2fsck" | "resize2fs" => "e2fsprogs",
        "ntfsresize" => "ntfs-3g",
        "fsck.f2fs" | "resize.f2fs" => "f2fs-tools",
        "xfs_growfs" => "xfsprogs",
        "btrfs" => "btrfs-progs",
        "fatresize" => "fatresize",
        "mkswap" | "swaplabel" => "util-linux",
        // mkfs 的各工具（mkfs_tool 的 program）：由 mkfs_capability 的缺工具提示引用
        "mke2fs" => "e2fsprogs",
        "mkfs.vfat" => "dosfstools",
        "mkfs.exfat" => "exfatprogs",
        "mkfs.ntfs" => "ntfs-3g",
        "mkfs.xfs" => "xfsprogs",
        "mkfs.btrfs" => "btrfs-progs",
        "mkfs.f2fs" => "f2fs-tools",
        _ => "unknown",
    }
}

fn check_support(verb: &str, fstype: &str, support: ToolSupport) -> Result<(), FsError> {
    match support {
        ToolSupport::NotApplicable => Ok(()),
        ToolSupport::Unsupported(reason) => Err(FsError::unsupported(format!("cannot {verb} {fstype}: {reason}"))),
        ToolSupport::Tools(tools) => {
            for &t in tools {
                if find_tool(t).is_err() {
                    return Err(FsError::missing(format!(
                        "cannot {verb} {fstype}: requires `{t}` (package: {}) — not found in PATH",
                        tool_package(t)
                    )));
                }
            }
            Ok(())
        }
    }
}

/// 扩容前置检查（纯只读、不写盘）。返回 Err 即"事前拒绝"。
/// 调用方**必须在首次写盘之前**执行，否则会留下"分区已改、FS 未扩"的中间态——
/// 这正是本检查存在的意义：把可预见的失败挡在动手之前
pub fn check_grow(fstype: &str) -> Result<(), FsError> {
    check_support("grow", fstype, grow_support(fstype))
}

/// 缩容前置检查，语义同上
pub fn check_shrink(fstype: &str) -> Result<(), FsError> {
    check_support("shrink", fstype, shrink_support(fstype))
}

/// 该 FS 的扩容/缩容应交给用户的补救命令（用于 PARTIAL 时的提示）
pub fn rescue_hint(fstype: &str, dev: &str) -> String {
    match fstype {
        f if is_ext(f) => format!("e2fsck -fp {dev} && resize2fs {dev}"),
        "ntfs" => format!("ntfsresize -f -f {dev}"),
        "f2fs" => format!("fsck.f2fs {dev} && resize.f2fs {dev}"),
        "xfs" => format!("mount {dev} <mnt> && xfs_growfs <mnt>"),
        "btrfs" => format!("mount {dev} <mnt> && btrfs filesystem resize max <mnt>"),
        "vfat" => format!("fatresize -s max {dev}"),
        "swap" => format!("mkswap --uuid <uuid> {dev}"),
        _ => String::new(),
    }
}

/// 本次扩容要动的**那一段**与它里面的文件系统：`scope` 是既有的范围表达（普通 FS = 分区
/// 本身；OpenWrt overlay 的 RW 层 = 分区内的一段），`fstype` 是 identify 口径的名字。
/// 判断与执行因此同源——调用点拿到它之后只把区间交给 [`GrowTarget::resize_fs`]，
/// 不自己推算 overlay 的偏移
pub struct GrowTarget {
    pub scope: DeviceScope,
    pub fstype: &'static str,
}

impl GrowTarget {
    /// 按判断期的同一份结论执行扩容
    pub fn resize_fs(&self, src: &FileSource) -> Result<(), FsError> {
        resize_fs_in(src, &self.scope, self.fstype)
    }
}

/// [`grow_target_at`] 的结论。三种结果对调用方的含义不同，故不塌缩成"能不能扩"一个布尔：
/// 有活要干的只有 `Target`；另两种都没有可扩的文件系统，区别在那块空间会不会被用上——
/// 未初始化的 overlay RW 层由首次挂载的 fstools 建满（分区层到此即完成），空区域 / LVM PV
/// 则什么都不会发生（FS 层命令把它当成功，就是报出一件没做过的事）
pub enum Growable {
    /// 有可扩的一段：普通分区就是它自己，OpenWrt 的只读根取分区尾部的 RW 层
    Target(GrowTarget),
    /// 尾部 RW 层尚未格式化（OpenWrt 首启）：空间由首次挂载时的 fstools 按尾部区域建满，
    /// 本次没有可写的后置条件
    OverlayPending,
    /// 区域里没有文件系统（空区域、LVM PV）：携带识别出的类型，供调用方分辨情形
    /// （如 PV 指路到 `resize --grow-lv`）
    NoFilesystem(&'static str),
}

/// 这段区域里能扩的文件系统是哪个 —— **唯一的判据**。写盘前的 preflight（能不能做、工具是否
/// 齐备）与写盘后的收尾（真正执行）都取它的结论；两处各按自己掌握的区间调用：写盘前是旧条目
/// 的区间，写盘后是新条目的区间，差别只在区间，不在规则。结论见 [`Growable`]；
/// `Err` 是"认得出却不接线"（含只有只读根、没有尾部 RW 层的 squashfs/erofs）或设备读不出来
pub fn grow_target_at(src: &FileSource, part: u32, base: u64, len: u64) -> Result<Growable, FsError> {
    let fstype = crate::fsid::identify(src, base, len).map_err(FsError::from)?;
    if fstype == "squashfs" || fstype == "erofs" {
        // OpenWrt combined 布局：同分区头部只读根 + 尾部 RW overlay（fstools rootdisk.c）。
        // fstools 只把 RW 层造成 ext4/f2fs，故内层可扩集合就是这两者
        let rel = crate::fsid::overlay_offset_at(src, base)
            .map_err(FsError::from)?
            .ok_or_else(|| FsError::unsupported(format!(
                "{fstype} rootfs without a trailing RW overlay layer — nothing to grow"
            )))?;
        if rel >= len {
            return Err(FsError::invalid("overlay offset beyond partition end"));
        }
        // 区间量本就是字节，直接传给按字节区间识别的 identify
        let inner = crate::fsid::identify(src, base + rel, len - rel).map_err(FsError::from)?;
        if !(is_ext(inner) || inner == "f2fs") {
            // 尚未格式化与"认得出但不接线"是两回事：前者那块空间会在首次挂载时被 fstools
            // 建满，后者要用户自己处理
            return if inner == "unknown" {
                Ok(Growable::OverlayPending)
            } else {
                Err(FsError::unsupported(format!(
                    "overlay layer identified as {inner} — only ext/f2fs overlays are growable"
                )))
            };
        }
        if src.is_block {
            // Range 路径走 loop，循环节点自身的挂载检查探不到底层分区——底层分区
            // 挂载态在此显式守卫
            let node = find_block_partition_node(src, part, base)?;
            require_unmounted(&node)?;
        }
        return Ok(Growable::Target(GrowTarget { scope: DeviceScope::Range(base + rel, len - rel), fstype: inner }));
    }
    // 其余按 FS 自身的可扩性分流：三态与工具清单都取自 grow_support，不在此另列一份
    match grow_support(fstype) {
        ToolSupport::Tools(_) => Ok(Growable::Target(GrowTarget { scope: DeviceScope::Partition(part), fstype })),
        ToolSupport::NotApplicable => Ok(Growable::NoFilesystem(fstype)),
        ToolSupport::Unsupported(reason) => Err(FsError::unsupported(format!("cannot grow {fstype}: {reason}"))),
    }
}

/// superfloppy（无分区表，FS 即整盘）扩容：无表可写，纯 FS grow
pub fn resize_fs_whole(src: &FileSource, fstype: &str) -> Result<(), FsError> {
    resize_fs_in(src, &DeviceScope::Whole, fstype)
}

/// resize 分发（扩容到给定范围末端）。范围由 [`grow_target_at`] 判定，本层只管执行：
/// - ext2/3/4：先 `e2fsck -fp` 修复，再 `resize2fs <dev>` 扩满分区（离线）。
///   -fp = 强制检查 + 自动修复；退出码按位或：0-1 通过、2/3 改了 root fs 须重启、
///   4 有未修正错误即拒绝（man e2fsck EXIT CODE）。resize2fs 缺省 size = 扩到分区末端（man resize2fs）
/// - ntfs：`ntfsresize -f -f <dev>`（双 force 跳过两道确认；size 缺省 = 设备大小，man ntfsresize）
/// - f2fs：`resize.f2fs <dev>`（离线；-t 指定目标扇区数，未给定时行为以工具实现为准，man resize.f2fs）
/// - xfs：只支持挂载态扩容（man xfs_growfs）→ 临时 mount → `xfs_growfs <mnt>` → umount
/// - btrfs：临时 mount → `btrfs filesystem resize max <mnt>`（max = 占满、须挂载态，man btrfs-filesystem）→ umount
/// - vfat：`fatresize -s max <dev>`（扩满设备，man fatresize）
/// - 其余（exfat/swap…）：无已接线的扩容工具，显式拒绝
fn resize_fs_in(src: &FileSource, scope: &DeviceScope, fstype: &str) -> Result<(), FsError> {
    match fstype {
        // fsid 识别只给 0xEF53，区分不出 2/3/4；resize2fs 对三者通用（man resize2fs）
        f if is_ext(f) => with_scope_device(src, scope, |dev| {
            let out = run("e2fsck", &["-fp", dev])?;
            check_e2fsck_for_resize(out.status.code().unwrap_or(-1))?;
            let out = run("resize2fs", &[dev])?;
            if !out.status.success() {
                return Err(FsError::command("resize2fs", &out));
            }
            Ok(())
        }),
        "ntfs" => with_scope_device(src, scope, |dev| {
            // 先 --no-action 演练，成功才真改，失败零副作用
            let out = run("ntfsresize", &["-f", "-f", "--no-action", dev])?;
            if !out.status.success() {
                return Err(FsError::CommandFailed(format!(
                    "ntfsresize simulation failed: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                )));
            }
            let out = run("ntfsresize", &["-f", "-f", dev])?;
            if !out.status.success() {
                return Err(FsError::command("ntfsresize", &out));
            }
            Ok(())
        }),
        "f2fs" => with_scope_device(src, scope, |dev| {
            // 预检：fsck.f2fs 无参 = 仅检查不修复（修复须显式 -f/-p；-a 只在内核报告 bug
            // 时才检查且默认禁用，man fsck.f2fs）。退出码负数表示失败（-1 表现为 255），
            // 非 0 即拒绝 resize
            let out = run("fsck.f2fs", &[dev])?;
            if !out.status.success() {
                return Err(FsError::CommandFailed(format!(
                    "fsck.f2fs failed (exit {}), refuse resize",
                    out.status.code().unwrap_or(-1)
                )));
            }
            let out = run("resize.f2fs", &[dev])?;
            if !out.status.success() {
                return Err(FsError::command("resize.f2fs", &out));
            }
            Ok(())
        }),
        "xfs" => with_scope_device(src, scope, |dev| with_mount(dev, |mnt| {
            let out = run("xfs_growfs", &[mnt])?;
            if !out.status.success() {
                return Err(FsError::command("xfs_growfs", &out));
            }
            Ok(())
        })),
        "btrfs" => {
            let (off, _) = scope_byte_range(src, scope)?;
            refuse_btrfs_multi_device_at(src, off)?;
            with_scope_device(src, scope, |dev| with_mount(dev, |mnt| {
                let out = run("btrfs", &["filesystem", "resize", "max", mnt])?;
                if !out.status.success() {
                    return Err(FsError::command("btrfs resize", &out));
                }
                Ok(())
            }))
        }
        // fatresize -s max = 扩满设备（man fatresize EXAMPLES；-s max 自 1.0.4 起支持）。
        // max 可能报 "Can't have overlapping partitions"，此时降级为显式字节（分区大小-1）；
        // 属经验性 workaround，非工具规范保证。
        // FAT32 <512MB 由工具自身拒绝（man BUGS，Windows 限制）
        "vfat" => {
            let (_, dev_len) = scope_byte_range(src, scope)?;
            with_scope_device(src, scope, |dev| {
                let mut out = run("fatresize", &["-s", "max", dev])?;
                if !out.status.success() {
                    // 兜底的字节数 = 设备长 − 1（见上）：长度为 0 时减一会回绕成
                    // u64::MAX，把"无从格式化"伪装成扩到天文数字
                    let want = dev_len.checked_sub(1).ok_or_else(|| {
                        FsError::invalid(format!("vfat device length {dev_len} — cannot derive fallback size"))
                    })?;
                    out = run("fatresize", &["-s", &want.to_string(), dev])?;
                }
                if !out.status.success() {
                    return Err(FsError::command("fatresize", &out));
                }
                Ok(())
            })
        }
        // squashfs/erofs 自己不可扩：能变大的是分区尾部的 RW overlay 层，那段由
        // grow_target_at 定位后交内层的分支执行。落到这里说明这个范围不是分区
        // （superfloppy），其内部没有 RW 层可扩
        "squashfs" | "erofs" => Err(FsError::unsupported(format!(
            "{fstype} needs a trailing RW overlay layer inside a partition — nothing to grow here"
        ))),
        other => Err(FsError::unsupported(format!(
            "no resize tool wired for {other} (supported: ext2/3/4, ntfs, f2fs, xfs, btrfs, vfat; squashfs/erofs via their trailing RW overlay layer)"
        ))),
    }
}

/// 本次调用的临时挂载点（纯函数：只用 PID 与进程内序号，不碰文件系统）。
///
/// 挂载点必须唯一：只用 PID 命名时，崩溃残留的挂载点会让下一次挂载叠在同一个目录上，
/// 而 PID 复用会直接撞上别人的残留。序号用进程内自增（与 atomic_write_ckpt 的临时名同法）
fn mount_point() -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    std::env::temp_dir().join(format!(
        "diskedit.mnt.{}.{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ))
}

/// 临时挂载 → 回调 → 卸载（xfs/btrfs 只支持挂载态扩容）。挂载点用后即删
fn with_mount<F>(dev: &str, f: F) -> Result<(), FsError>
where
    F: FnOnce(&str) -> Result<(), FsError>,
{
    let mnt = mount_point();
    std::fs::create_dir_all(&mnt).map_err(FsError::from)?;
    let mount = find_tool("mount")?;
    let out = Command::new(mount).arg(dev).arg(&mnt).stdin(Stdio::null()).output().map_err(FsError::from)?;
    if !out.status.success() {
        // mount 没成功就谈不上 umount：对从未挂载的目录卸载必然失败，
        // 那条"may remain"的告警只会把用户引向一个不存在的残留挂载点。
        // 临时目录仍要清——它是本次调用建的，mount 失败不代表它可以留下
        crate::dev::best_effort_rmdir(&mnt);
        return Err(FsError::command("mount", &out));
    }
    let res = f(&mnt.to_string_lossy());
    match find_tool("umount") {
        Ok(u) => {
            let out = Command::new(u).arg(&mnt).stdin(Stdio::null()).output();
            // 卸载失败不静默：残留挂载点会占用分区，提示用户手动处理
            if out.as_ref().map(|o| !o.status.success()).unwrap_or(true) {
                let why = match out {
                    Ok(o) => String::from_utf8_lossy(&o.stderr).trim().to_string(),
                    Err(e) => e.to_string(),
                };
                eprintln!("warning: umount {} failed: {why} — temp mount point may remain", mnt.display());
            }
        }
        // umount 缺失同样不能静默：挂载中的目录 rmdir 必然失败，"用后即删"就此失效
        // 而用户毫不知情——与"卸载失败"同一告警口径
        Err(e) => eprintln!(
            "warning: umount not found ({e}) — temp mount point {} may remain mounted",
            mnt.display()
        ),
    }
    crate::dev::best_effort_rmdir(&mnt);
    res
}

/// FS 缩容到指定字节数（调用方保证 ≤ 当前 FS 大小；先于分区边界收缩执行）
pub fn shrink_fs(src: &FileSource, part: u32, fstype: &str, new_bytes: u64) -> Result<(), FsError> {
    match fstype {
        f if is_ext(f) => with_partition_device(src, part, |dev| {
        let out = run("e2fsck", &["-fp", dev])?;
        check_e2fsck_for_resize(out.status.code().unwrap_or(-1))?;
            // resize2fs 裸数字单位是"文件系统块数"而非字节（man resize2fs）；
            // 's' 后缀 = 512 字节扇区。分区尺寸必为 sector_size(≥512) 整数倍
            if !new_bytes.is_multiple_of(512) {
                return Err(FsError::invalid("shrink size must be a multiple of 512"));
            }
            let sectors = format!("{}s", new_bytes / 512);
            let out = run("resize2fs", &[dev, &sectors])?;
            if !out.status.success() {
                return Err(FsError::CommandFailed(format!(
                    "resize2fs shrink failed: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                )));
            }
            Ok(())
        }),
        "ntfs" => with_partition_device(src, part, |dev| {
            // -s 无后缀 = 字节，k/M/G = 10³/10⁶/10⁹（man ntfsresize OPTIONS）；本工具只传裸字节
            let out = run("ntfsresize", &["-f", "-f", "-s", &new_bytes.to_string(), dev])?;
            if !out.status.success() {
                return Err(FsError::CommandFailed(format!(
                    "ntfsresize shrink failed: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                )));
            }
            Ok(())
        }),
        "btrfs" => {
            let (off, _) = partition_byte_range(src, part)?;
            refuse_btrfs_multi_device_at(src, off)?;
            with_partition_device(src, part, |dev| with_mount(dev, |mnt| {
                // 裸数字 = 绝对字节数、必须挂载态（man btrfs-filesystem resize）
                let out = run("btrfs", &["filesystem", "resize", &new_bytes.to_string(), mnt])?;
                if !out.status.success() {
                    return Err(FsError::CommandFailed(format!(
                        "btrfs shrink failed: {} (minimum size via `btrfs inspect-internal min-dev-size` on a mount)",
                        String::from_utf8_lossy(&out.stderr).trim()
                    )));
                }
                Ok(())
            }))
        }
        other => Err(FsError::unsupported(format!("fs {other} cannot shrink"))),
    }
}

/// FS 一致性检查（不修改分区边界；是否写盘取决于各工具的只读/修复开关）。
/// 各工具与开关：e2fsck -fp（man e2fsck）、ntfsfix -d/--clear-dirty（能修复并成功
/// 挂载时清除 NTFS dirty 标志，否则保持/置位 dirty 交 Windows 下次检查；ntfsfix 非
/// chkdsk，检查语义偏弱，man ntfsfix）、fsck.f2fs 无参 = 仅检查（man fsck.f2fs）、
/// xfs_repair -n = no modify（man xfs_repair）、btrfs check = 默认只读（man btrfs-check）、
/// fsck.vfat -n = no-operation 只读（man fsck.fat）、fsck.exfat -n = read-only 不修复（man fsck.exfat）
pub fn check_fs(src: &FileSource, part: u32, fstype: &str) -> Result<(), FsError> {
    if fstype == "lvm2_pv" {
        return Err(FsError::unsupported(
            "target is an LVM2 PV, not a filesystem — PV metadata is checked with pvck(8), not fsck",
        ));
    }
    let cmd: (&str, Vec<&str>) = match fstype {
        f if is_ext(f) => ("e2fsck", vec!["-fp"]),
        "ntfs" => ("ntfsfix", vec!["-d"]),
        "f2fs" => ("fsck.f2fs", vec![]),
        "xfs" => ("xfs_repair", vec!["-n"]),
        "btrfs" => ("btrfs", vec!["check"]),
        "vfat" => ("fsck.vfat", vec!["-n"]),
        // fsck.exfat man 未定义无参默认行为（存在 -r 交互修复），显式 -n = 仅检查不修复
        "exfat" => ("fsck.exfat", vec!["-n"]),
        other => return Err(FsError::unsupported(format!("no check tool wired for {other}"))),
    };
    with_partition_device(src, part, |dev| {
        let mut args = cmd.1;
        args.push(dev);
        let out = run(cmd.0, &args)?;
        if cmd.0 == "e2fsck" {
            return check_e2fsck_for_check(out.status.code().unwrap_or(-1));
        }
        if !out.status.success() {
            return Err(FsError::command(cmd.0, &out));
        }
        Ok(())
    })
}

/// 设置 FS label。值原样传给各 FS 官方工具。
/// 来源：tune2fs -L（man tune2fs）、xfs_admin -L ≤12 字符（man xfs_admin）、
/// btrfs filesystem label ≤256 字符（man btrfs-filesystem）、ntfslabel（man ntfslabel）、
/// fatlabel ≤11 字节（man fatlabel）、exfatlabel（man exfatlabel）、
/// swaplabel -L（man swaplabel，标签写进 swap 头的 volume_name[16]）
pub fn set_label(src: &FileSource, part: u32, fstype: &str, label: &str) -> Result<(), FsError> {
    with_partition_device(src, part, |dev| {
        let (tool, args): (&str, Vec<String>) = match fstype {
            f if is_ext(f) => ("tune2fs", vec!["-L".into(), label.into(), dev.into()]),
            // XFS 标签上限 12 字节（superblock s_fname[12]，man xfs_admin "twelve
            // characters"）：超长时 xfs_admin 静默截断，故提前拒绝
            "xfs" => {
                if label.len() > 12 {
                    return Err(FsError::invalid(format!(
                        "xfs label exceeds 12 bytes (got {}); xfs_admin would silently truncate",
                        label.len()
                    )));
                }
                ("xfs_admin", vec!["-L".into(), label.into(), dev.into()])
            }
            "btrfs" => ("btrfs", vec!["filesystem".into(), "label".into(), dev.into(), label.into()]),
            "ntfs" => ("ntfslabel", vec![dev.into(), label.into()]),
            "vfat" => ("fatlabel", vec![dev.into(), label.into()]),
            "exfat" => ("exfatlabel", vec![dev.into(), label.into()]),
            "swap" => ("swaplabel", vec!["-L".into(), label.into(), dev.into()]),
            other => return Err(FsError::unsupported(format!("no label tool wired for {other}"))),
        };
        let argrefs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
        let out = run(tool, &argrefs)?;
        if !out.status.success() {
            return Err(FsError::command(tool, &out));
        }
        Ok(())
    })
}

/// 一次 UUID 设置请求的两种形式。**不含"目标能不能接受"**——那是 [`uuid_support`] 回答的，
/// 由调用方比对后决定拒还是做（命令层是唯一能给出"改参数也许有解"式拒绝的地方）
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UuidRequest {
    /// 写入调用方给定的值
    Explicit(String),
    /// 要求目标生成一个新的随机值
    NewRandom,
}

/// 该 FS 能接受什么样的 UUID 设置请求（三态，不折成布尔）：
/// - `Yes`：值由调用方给定（tune2fs -U / xfs_admin -U / btrfstune -U / swaplabel -U）
/// - `RandomOnly`：只支持"生成新随机值"。ntfs 唯一可改的标识是 `ntfslabel --new-serial`
///   生成的 serial，而它**不是** Windows volume UUID（man ntfslabel）——用户给的具体值
///   无从落实，静默丢弃正是要防的漂移，故必须让调用方显式拒绝
/// - `No`：本工具不为这种类型接 UUID 工具
pub enum UuidSupport {
    Yes,
    RandomOnly,
    No(&'static str),
}

pub fn uuid_support(fstype: &str) -> UuidSupport {
    if is_ext(fstype) || matches!(fstype, "xfs" | "btrfs" | "swap") {
        UuidSupport::Yes
    } else if fstype == "ntfs" {
        UuidSupport::RandomOnly
    } else {
        UuidSupport::No("not wired to a tool — use the filesystem's own utility")
    }
}

/// 设置 FS UUID。来源：tune2fs -U（man tune2fs）、xfs_admin -U（man xfs_admin）、
/// ntfslabel --new-serial 无值=随机 serial（man ntfslabel）、
/// btrfstune -f -U（-f：change fsid 属 dangerous changes，man btrfstune）、
/// swaplabel -U（man swaplabel）。
/// 请求形式与 FS 能力的匹配由调用方先按 [`uuid_support`] 判定；此处只做工具映射，
/// 落不到工具的组合同样拒绝，不静默降级
pub fn set_uuid(src: &FileSource, part: u32, fstype: &str, req: &UuidRequest) -> Result<(), FsError> {
    with_partition_device(src, part, |dev| {
        let no_tool = |what: &str| {
            FsError::unsupported(format!("no uuid tool wired for {fstype}{what}"))
        };
        let (tool, args): (&str, Vec<String>) = match req {
            UuidRequest::Explicit(u) => match fstype {
                f if is_ext(f) => ("tune2fs", vec!["-U".into(), u.clone(), dev.into()]),
                "xfs" => ("xfs_admin", vec!["-U".into(), u.clone(), dev.into()]),
                "btrfs" => ("btrfstune", vec!["-f".into(), "-U".into(), u.clone(), dev.into()]),
                "swap" => ("swaplabel", vec!["-U".into(), u.clone(), dev.into()]),
                _ => return Err(no_tool("")),
            },
            // 只有 ntfs 的工具提供"生成新值"这一用法
            UuidRequest::NewRandom => match fstype {
                "ntfs" => ("ntfslabel", vec!["--new-serial".into(), dev.into()]),
                _ => return Err(no_tool(" that generates a random value")),
            },
        };
        let argrefs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
        let out = run(tool, &argrefs)?;
        if !out.status.success() {
            return Err(FsError::command(tool, &out));
        }
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::{check_e2fsck_for_check, check_e2fsck_for_resize, erase_ranges, grow_target_at, parse_num_field, partition_byte_range, uuid_support, DeviceScope, Growable, UuidSupport};
    use super::FsError;
    use crate::dev::FileSource;

    /// e2fsck 2/3/4 归 `CommandFailed`：三者都说明工具运行过——"确定未写盘"
    /// 的 Infra 承诺不成立。exit 2 的人工复现（e2fsck -fp 改过 root fs）无需真实环境：
    /// 判定是纯函数，逐码断言变体即可
    #[test]
    fn e2fsck_exit_codes_do_not_claim_no_write() {
        assert!(check_e2fsck_for_resize(0).is_ok());
        assert!(check_e2fsck_for_resize(1).is_ok());
        for c in [2, 3, 4] {
            assert!(matches!(check_e2fsck_for_resize(c), Err(FsError::CommandFailed(_))), "exit {c}");
        }
        // 基础设施故障（8/16 等）同样走 CommandFailed，与 2/3/4 同口径
        assert!(matches!(check_e2fsck_for_resize(8), Err(FsError::CommandFailed(_))));
    }

    /// check 语境的判定独立于 resize：修复成功（0..=3）是 check 的本职成果，
    /// 不得照搬 resize 的 REBOOT 拒绝——照搬会把修复成功报成 Failed(30)。
    /// 仅 4（未修正错误）与基础设施故障是失败
    #[test]
    fn e2fsck_check_context_treats_repair_as_success() {
        for ok in [0, 1, 2, 3] {
            assert!(check_e2fsck_for_check(ok).is_ok(), "exit {ok} must pass");
        }
        assert!(matches!(check_e2fsck_for_check(4), Err(FsError::CommandFailed(_))));
        assert!(matches!(check_e2fsck_for_check(8), Err(FsError::CommandFailed(_))));
    }

    /// swap 的 label/uuid 走 swaplabel（util-linux）：能力判定归 Yes（只收显式值，
    /// `--random` 由命令层按 Yes+NewRandom 拒绝）。这条接线若回退，set 会退回
    /// "no label tool wired for swap" 的拒绝
    #[test]
    fn swap_label_and_uuid_are_wired_to_swaplabel() {
        assert!(matches!(uuid_support("swap"), UuidSupport::Yes));
        assert!(matches!(uuid_support("vfat"), UuidSupport::No(_)));
    }

    /// `grow_target_at` 是"这段区域里能扩的是什么"的唯一判据：普通分区取 FS 自己，
    /// OpenWrt 的只读根取分区尾部的 RW 层。三种结论必须分开——**尚未格式化的 RW 层与
    /// 空区域 / PV 都"没有可扩的文件系统"**，但前者的空间会被首次挂载的 fstools 建满，
    /// 后者什么都不会发生；认得出却不接线仍是拒绝。混为一谈要么让 FS 层命令报出一件
    /// 没做过的事，要么让首启现场报假失败
    #[test]
    fn grow_target_at_separates_nothing_to_do_from_unsupported() {
        // squashfs 的 bytes_used → RW 层起点（上对齐 64K）；区域长 128K，内层自 64K 起
        const LEN: u64 = 128 * 1024;
        const REL: u64 = 64 * 1024;
        let squashfs_head = |inner: &[u8]| {
            let mut d = vec![0u8; LEN as usize];
            d[0..4].copy_from_slice(b"hsqs"); // SQUASHFS_MAGIC @0
            d[0x28..0x30].copy_from_slice(&4096u64.to_le_bytes()); // bytes_used @0x28
            d[REL as usize..REL as usize + inner.len()].copy_from_slice(inner);
            d
        };
        let inner_ext = {
            let mut f = vec![0u8; 4096];
            f[0x438..0x43A].copy_from_slice(&0xEF53u16.to_le_bytes()); // s_magic @sb+0x38
            f
        };

        // 取 `Target`：其余结论一律视为用例失败，免去每处写一遍三臂 match
        let target = |r: Result<Growable, FsError>| match r {
            Ok(Growable::Target(t)) => t,
            Ok(Growable::OverlayPending) => panic!("expected a grow target, got OverlayPending"),
            Ok(Growable::NoFilesystem(ft)) => panic!("expected a grow target, got NoFilesystem({ft})"),
            Err(e) => panic!("expected a grow target, got {e}"),
        };

        // 内层 ext：可扩，且 scope 指向内层那段（不是整个分区）
        let t = target(grow_target_at(&fs_fixture("ovl_ext", squashfs_head(&inner_ext)), 3, 0, LEN));
        assert_eq!(t.fstype, "ext");
        assert!(matches!(t.scope, DeviceScope::Range(off, len) if off == REL && len == LEN - REL));

        // 内层还是零（首启尚未格式化）：没有可扩的文件系统，但那块空间会被首次挂载的
        // fstools 建满——与下面"区域里压根没有 FS"是两种事
        assert!(matches!(
            grow_target_at(&fs_fixture("ovl_raw", squashfs_head(&[])), 3, 0, LEN),
            Ok(Growable::OverlayPending)
        ));

        // 内层是认得出却不接线的类型：拒绝，并说清是内层的类型
        let mut xfs = vec![0u8; 4096];
        xfs[0..4].copy_from_slice(b"XFSB");
        match grow_target_at(&fs_fixture("ovl_xfs", squashfs_head(&xfs)), 3, 0, LEN) {
            Err(FsError::UnsupportedFs(m)) => assert!(m.contains("overlay layer identified as xfs"), "{m}"),
            Err(e) => panic!("expected the inner layer to be named in the refusal, got {e}"),
            Ok(_) => panic!("xfs overlay layer must be refused"),
        }

        // 只有只读根、没有尾部 RW 层（bytes_used = 0）：认得却无法扩
        let mut no_layer = vec![0u8; LEN as usize];
        no_layer[0..4].copy_from_slice(b"hsqs");
        assert!(matches!(
            grow_target_at(&fs_fixture("ovl_none", no_layer), 3, 0, LEN),
            Err(FsError::UnsupportedFs(m)) if m.contains("without a trailing RW overlay layer")
        ));

        // 普通分区：可扩目标就是分区自己
        let mut plain = vec![0u8; 64 * 1024];
        plain[0x438..0x43A].copy_from_slice(&0xEF53u16.to_le_bytes());
        let t = target(grow_target_at(&fs_fixture("plain_ext", plain), 2, 0, 64 * 1024));
        assert_eq!(t.fstype, "ext");
        assert!(matches!(t.scope, DeviceScope::Partition(2)));

        // 空区域与 LVM PV：没有文件系统，且不会有——类型原样报出，供调用方分辨
        // （PV 有别的出路）
        assert!(matches!(
            grow_target_at(&fs_fixture("plain_zero", vec![0u8; 64 * 1024]), 2, 0, 64 * 1024),
            Ok(Growable::NoFilesystem("unknown"))
        ));
        let mut pv = vec![0u8; 64 * 1024];
        pv[512..520].copy_from_slice(b"LABELONE"); // PV label 在第 2 扇区，同 fsid 的识别口径
        pv[536..544].copy_from_slice(b"LVM2 001");
        assert!(matches!(
            grow_target_at(&fs_fixture("plain_pv", pv), 2, 0, 64 * 1024),
            Ok(Growable::NoFilesystem("lvm2_pv"))
        ));

        // 认得出却不接线的类型（exfat）：拒绝
        let mut exfat = vec![0u8; 64 * 1024];
        exfat[3..11].copy_from_slice(b"EXFAT   ");
        assert!(matches!(
            grow_target_at(&fs_fixture("plain_exfat", exfat), 2, 0, 64 * 1024),
            Err(FsError::UnsupportedFs(m)) if m.contains("cannot grow exfat")
        ));
    }

    fn fs_fixture(tag: &str, data: Vec<u8>) -> FileSource {
        let mut tmp = std::env::temp_dir();
        tmp.push(format!("diskedit_fsops_{tag}_{}.img", std::process::id()));
        std::fs::write(&tmp, &data).unwrap();
        let f = std::fs::OpenOptions::new().read(true).write(true).open(&tmp).unwrap();
        let size = data.len() as u64;
        FileSource {
            identity: crate::dev::TargetIdentity::resolve_image(&tmp),
            file: f,
            path: tmp,
            sector_size: 512,
            size,
            is_block: false,
            journal: None,
            ownership: None,
            fingerprint: Default::default(),
            loop_mapping: None,
        }
    }

    /// 分区字节范围：GPT 与 MBR 两种表来源；空槽位/越界编号/容器分区/无表均拒绝
    #[test]
    fn partition_byte_range_gpt_and_mbr() {
        // GPT：按解析时的扇区大小换算
        let mut src = fs_fixture("pbr_gpt", vec![0u8; 2 * 1024 * 1024]);
        crate::table::create_gpt(&mut src, 512, None).unwrap();
        crate::gpt_policy::add_entry(&mut src, 2048, 3000, "p", crate::table::LINUX_FS_TYPE_GUID).unwrap();
        assert_eq!(partition_byte_range(&src, 1).unwrap(), (2048 * 512, (3000 - 2048 + 1) * 512));
        assert!(partition_byte_range(&src, 2).is_err(), "empty slot must be rejected");
        assert!(partition_byte_range(&src, 99).is_err(), "out-of-range number must be rejected");
        drop(src);

        // MBR：容器分区（0x05/0x0F/0x85）无法做 FS 操作
        let mut src = fs_fixture("pbr_mbr", vec![0u8; 2 * 1024 * 1024]);
        crate::table::create_mbr(&mut src).unwrap();
        crate::table::add_mdos_entry(&mut src, 63, 200, 0x83).unwrap();
        crate::table::add_mdos_entry(&mut src, 201, 300, 0x05).unwrap();
        assert_eq!(partition_byte_range(&src, 1).unwrap(), (63 * 512, (200 - 63 + 1) * 512));
        let err = partition_byte_range(&src, 2).unwrap_err();
        assert!(err.to_string().contains("container"), "{err}");
        drop(src);

        // 无表
        let src = fs_fixture("pbr_none", vec![0u8; 2 * 1024 * 1024]);
        assert!(partition_byte_range(&src, 1).is_err());
    }

    // mountinfo 解析器仅 Linux 编译（与调用方同门控），故测试同样门控
    #[cfg(target_os = "linux")]
    #[test]
    fn mountinfo_fields_and_octal_escapes() {
        use super::{decode_octal, parse_mountinfo};
        // 固定字段 1..6，其后 optional fields 数量可变，以单独的 "-" 分界
        // （man proc_pid_mountinfo(5)）
        let e = parse_mountinfo("36 25 8:1 / /mnt/data rw,relatime shared:1 - ext4 /dev/sda1 rw")
            .expect("parse");
        assert_eq!(e.dev_no, (8, 1));
        assert_eq!(e.mount_point, "/mnt/data");
        assert_eq!(e.source, "/dev/sda1");
        // 挂载点/源路径的八进制转义须先解码才能与真实路径比对（空格 = \040）
        let e = parse_mountinfo("37 25 8:2 / /mnt/my\\040data rw - ext4 /dev/sda2 rw").expect("parse");
        assert_eq!(e.mount_point, "/mnt/my data");
        assert_eq!(decode_octal("/a\\011b"), "/a\tb");
        assert_eq!(decode_octal("/a\\xb"), "/a\\xb"); // 非八进制转义原样保留
        // 无 "-" 分界 / 字段不足 → 拒绝
        assert!(parse_mountinfo("36 25 8:1 / / rw").is_none());
        assert!(parse_mountinfo("").is_none());
    }

    /// 挂载点必须逐次唯一：只用 PID 命名时，崩溃残留的挂载点会让下一次挂载叠在同一个
    /// 目录上，PID 复用还会直接撞上别人的残留。此处只断言纯函数部分，不必真挂载
    #[test]
    fn temp_mount_points_are_unique() {
        let a = super::mount_point();
        let b = super::mount_point();
        assert_ne!(a, b, "two calls in one process must not share a mount point");
        assert_eq!(a.parent(), b.parent(), "both live directly under the temp dir");
    }

    #[test]
    fn erase_range_table_layout() {
        // 2 PiB 分区：原始表 10 行，其中 4 条 bcachefs 桶行取整后同址，被相邻重复项去重，
        // 无越界时产出 7 个区间
        let big = 2 * 1024 * 1024 * 1024 * 1024 * 1024u64; // 2 PiB
        let rs = erase_ranges(big, 512);
        assert_eq!(rs.len(), 7);
        assert_eq!(rs[0], (0, 512 * 1024));
        assert_eq!(rs[1], (64 * 1024 * 1024, 4096));
        // FastTrack：起点 = 末尾 -3087 扇区，长度 512B 恰整扇区
        assert_eq!(rs[4], (big - 3087 * 512, 512));
        // 尾部 768KiB 区间不越界
        let tail = rs.last().unwrap();
        assert_eq!(tail.1 % 512, 0);
        assert!(tail.0 + tail.1 <= big);
        // 8MiB 分区：64MiB/256GiB/1PiB 越界裁掉，头部+FT+bcachefs+尾部 = 4 区间
        let small = erase_ranges(8 * 1024 * 1024, 512);
        assert_eq!(small.len(), 4);
        assert_eq!(small[0], (0, 512 * 1024));
        assert_eq!(small[1], (8 * 1024 * 1024 - 3087 * 512, 512));
        // 尾部区间裁剪后长度 = 768K−(768K−512K) = 512K（末端到分区界）
        let tail = small.last().unwrap();
        assert_eq!(tail.0 + tail.1, 8 * 1024 * 1024);
        // 256GiB 分区：头部+64MiB+FT+bcachefs+尾部 = 5 区间
        let mid = erase_ranges(256 * 1024 * 1024 * 1024u64, 512);
        assert_eq!(mid.len(), 5);
        assert_eq!(mid[1], (64 * 1024 * 1024, 4096));
        // bcachefs 桶对齐：-1MiB 先按 rounding 下取整再落扇区界
        // （part_len−1MiB 非桶对齐时命中更低位）
        let odd = erase_ranges(8288 * 1024, 512);
        assert_eq!(odd[2], (7168 * 1024, 4096)); // floor(7288K, 128K) = 7168K
    }

    #[test]
    fn erase_ranges_boundaries() {
        const K: u64 = 1024;
        const M: u64 = 1024 * K;
        // part_len 恰为 rounding（bcachefs 128K 桶）整数倍：pos = part_len−1M 落桶界，无需取整
        // （FT 行起点算成负数后被裁剪丢弃，bcachefs 是第 2 个产出区间）
        let exact = erase_ranges(M + 128 * K, 512);
        assert_eq!(exact[1], (128 * K, 4 * K));
        // 多 1 字节：floor 后命中同一桶界
        let plus1 = erase_ranges(M + 128 * K + 1, 512);
        assert_eq!(plus1[1], (128 * K, 4 * K));
        // 1MiB 分区：头部 512K + 尾部 512K——尾部行（-512K, 768K）跨尾裁剪保留半段
        // （不整段跳过），bcachefs 四行起点被 floor 到 0、产出的 (0,4K) 与头部不同址故保留
        // （重复写零幂等，只去重相邻的完全相同项，不做区间合并）
        let tiny = erase_ranges(M, 512);
        assert!(tiny.contains(&(0, 512 * K)));
        assert!(tiny.contains(&(512 * K, 512 * K)));
        assert!(tiny.iter().all(|&(s, l)| s + l <= M && s < M));
        // part_len = 1536K：尾部区间（from_end=512K）起点 = 1M（256K 取整后恰对齐），与头部不重叠
        let tail_fit = erase_ranges(1536 * K, 512);
        assert_eq!(tail_fit.last().unwrap(), &(M, 512 * K));
        // 所有区间：整扇区、落在分区界内（不同 rounding 的桶对齐不保证产出有序，
        // 未排序——wipe_zero 逐区间写零与顺序无关）
        for len in [M, 768 * K + 1, M + 128 * K + 1, 8 * M + 1, 8288 * K] {
            for &(s, l) in erase_ranges(len, 512).iter() {
                assert!(s % 512 == 0 && l % 512 == 0 || s + l == len, "tail clip may end unaligned at part end");
                assert!(s + l <= len && s < len);
            }
        }
    }

    #[test]
    fn e2fsck_exit_code_semantics() {
        // man e2fsck 按位或语义（bit1 = 改 root fs 须重启）：resize 判据 0/1 可继续，
        // 2/3/4 拒绝，更高位为基础设施失败；check 判据 0..=3 成功
        for ok in [0, 1] {
            assert!(check_e2fsck_for_resize(ok).is_ok(), "exit {ok} must pass");
        }
        for refuse in [2, 3, 4, 8, 16] {
            assert!(check_e2fsck_for_resize(refuse).is_err(), "exit {refuse} must fail");
        }
        for ok in [0, 1, 2, 3] {
            assert!(check_e2fsck_for_check(ok).is_ok(), "check exit {ok} must pass");
        }
        for refuse in [4, 8, 16] {
            assert!(check_e2fsck_for_check(refuse).is_err(), "check exit {refuse} must fail");
        }
    }

    #[test]
    fn parses_tool_output_formats() {
        // resize2fs -P / dumpe2fs -h 实际输出格式样本
        let a = "Estimated minimum size of the filesystem: 12345\n";
        assert_eq!(parse_num_field(a, "Estimated minimum size of the filesystem:").unwrap(), 12345);
        let b = "meta-data=/dev/loop0\nBlock size:               4096\n";
        assert_eq!(parse_num_field(b, "Block size:").unwrap(), 4096);
        assert!(parse_num_field("nothing here", "Block size:").is_err());
    }
}