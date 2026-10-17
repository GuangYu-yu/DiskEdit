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

/// 读取 /proc/self/mountinfo。任何一行解析失败都整体报错：调用方的契约是
/// "确认不了空闲即不算空闲"（见 probe_occupancy），跳过坏行会把目标设备恰好
/// 落在那一行上的情形误判成未挂载——宁可整个操作拒做
#[cfg(target_os = "linux")]
pub(crate) fn read_mounts() -> io::Result<Vec<MountEntry>> {
    let s = std::fs::read_to_string("/proc/self/mountinfo")?;
    s.lines().map(|l| {
        parse_mountinfo(l)
            .ok_or_else(|| io::Error::other(format!("unparseable /proc/self/mountinfo line: {l}")))
    }).collect()
}

/// 设备占用探测结果（/proc/self/mountinfo + /proc/swaps，路径与 st_rdev 并集匹配）。
/// FS 操作要求分区未挂载、未作 swap 是本工具策略，内核并不强制（man resize2fs：
/// 内核支持在线扩容时可直接扩已挂载的 ext）；挂载态扩容走 xfs/btrfs 的临时挂载路径，
/// 不经此判定
#[cfg(target_os = "linux")]
pub(crate) enum Occupancy {
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
pub(crate) fn probe_occupancy(dev: &str) -> io::Result<Occupancy> {
    use std::os::unix::fs::MetadataExt;
    let m = std::fs::metadata(dev).map_err(|e| {
        io::Error::other(format!("cannot stat {dev} to confirm it is unmounted: {e}"))
    })?;
    let rdev = m.rdev();
    let dev_no = (libc::major(rdev) as u64, libc::minor(rdev) as u64);
    let entries = read_mounts()?;
    // 双判据取并集：dev_no（st_rdev）之外比对 source 路径——btrfs 成员挂载在 mountinfo
    // 字段 3 报匿名设备号（anon bdev，与成员分区 st_rdev 必不相同），但 source 仍是
    // 成员设备路径，单靠 dev_no 会漏检
    if entries.iter().any(|e| e.dev_no == dev_no || e.source == dev) {
        return Ok(Occupancy::Mounted);
    }
    let swaps = std::fs::read_to_string("/proc/swaps")?;
    for line in swaps.lines().skip(1) {
        if let Some(field) = line.split_whitespace().next() {
            // 同一设备的两种判据取并集：路径相同，或 st_rdev 相同。stat 失败时两项都
            // 无法确认，按本函数的 fail-closed 口径报错——当成"这一项不是目标"就是
            // 把"确认不了"读成"空闲"
            if field == dev {
                return Ok(Occupancy::ActiveSwap);
            }
            let fm = std::fs::metadata(field).map_err(|e| {
                io::Error::other(format!(
                    "cannot stat swap entry {field} to confirm {dev} is not active as swap: {e}"
                ))
            })?;
            if fm.rdev() == rdev {
                return Ok(Occupancy::ActiveSwap);
            }
        }
    }
    Ok(Occupancy::Idle)
}

/// FS 步的占用闸：占用或探测失败都拒绝（fail-closed——探测失败若被当作
/// "未挂载"，会对已挂载的 FS 执行 resize，那是数据损坏）
#[cfg(target_os = "linux")]
pub(crate) fn require_unmounted(dev: &str) -> Result<(), FsError> {
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
pub(crate) fn require_unmounted(_dev: &str) -> Result<(), FsError> {
    Ok(())
}

/// dev_no（major, minor）所属 loop 设备的 backing 文件；不是 loop 或解析不出来 ⇒ None
#[cfg(target_os = "linux")]
pub(crate) fn loop_backing_of(dev_no: (u64, u64)) -> Option<std::path::PathBuf> {
    let real = std::fs::canonicalize(
        Path::new("/sys/dev/block").join(format!("{}:{}", dev_no.0, dev_no.1)),
    )
    .ok()?;
    // 设备 kobject 路径上出现 `loopN` 即说明是 loop（或它的分区）：分区名形如 loop0p1，
    // 尾部不是纯数字，故只认首个 `loop` + 全数字的组件
    let loop_name = real
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .find(|s| s.len() > 4 && s.starts_with("loop") && s[4..].bytes().all(|b| b.is_ascii_digit()))?;
    let raw = std::fs::read_to_string(
        Path::new("/sys/block").join(&loop_name).join("loop/backing_file"),
    )
    .ok()?;
    let raw = raw.trim();
    // backing 已被删除时内核在路径后附 " (deleted)"
    let raw = raw.strip_suffix(" (deleted)").unwrap_or(raw);
    if raw.is_empty() {
        return None;
    }
    Some(std::fs::canonicalize(raw).unwrap_or_else(|_| std::path::PathBuf::from(raw)))
}

/// 指向同一镜像的 loop 中，哪一个正被占用（挂载或活动 swap），返回它的设备路径。
///
/// 镜像是本工具内唯一没有分区节点可查的目标：本工具要经 loop 才能碰它的分区，而那个
/// loop 与用户自己映射的是**两个设备**——只探测新建的那个看不见用户那份挂载。故反向
/// 解析：把每个已挂载 / 已做 swap 的设备解析到它所属的 loop，再看那个 loop 的 backing
/// 是不是本镜像
#[cfg(target_os = "linux")]
pub(crate) fn image_loop_occupant(src: &FileSource) -> io::Result<Option<String>> {
    use std::os::unix::fs::MetadataExt;
    let want = std::fs::canonicalize(&src.path).map_err(|e| {
        io::Error::other(format!(
            "cannot resolve {} to check for loops onto it: {e}",
            src.path.display()
        ))
    })?;
    for e in read_mounts()? {
        if loop_backing_of(e.dev_no).is_some_and(|b| b == want) {
            return Ok(Some(e.source));
        }
    }
    for line in std::fs::read_to_string("/proc/swaps")?.lines().skip(1) {
        let Some(field) = line.split_whitespace().next() else { continue };
        // stat 不到就不是设备节点，也就不可能是本镜像的 loop（那个必然是设备节点）
        let Some(rdev) = std::fs::metadata(field).ok().map(|m| m.rdev()) else { continue };
        if loop_backing_of((libc::major(rdev) as u64, libc::minor(rdev) as u64)).is_some_and(|b| b == want) {
            return Ok(Some(field.to_string()));
        }
    }
    Ok(None)
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn image_loop_occupant(_src: &FileSource) -> io::Result<Option<String>> {
    Ok(None)
}

/// 写表路径的占用前置闸：分区在首次落盘前须空闲。调用点在 prepare 层（movepart 的
/// prepare_resize / prepare_apply）与 MBR resize 命令层——那是任何写盘的唯一必经关口，
/// 把"表已写、FS 步被占"的 PARTIAL(20) 变成 REFUSED(10)：占用是请求与现状不匹配，
/// 不是盘故障。块设备另有整盘 O_EXCL 独占打开这道内核级防线，但其保护面要说准：
/// 它挡的是挂载 / swap / 他人的 O_EXCL 打开（open 即 EBUSY），**不挡**不守约定的
/// 普通并发写——本闸对其是纵深防御。镜像没有分区节点可查，也没有 O_EXCL 这道内核
/// 防线，占用只能按"有没有另一个 loop 指着它且在用"判定（[`image_loop_occupant`]）；
/// 那一步只覆盖已经映射出去的 loop，不覆盖别的东西按镜像路径直接写字节。
/// `start_bytes` = 分区起始字节（与 find_block_partition_node 的换算基准一致）
#[cfg(target_os = "linux")]
pub fn ensure_idle_before_write(src: &FileSource, part: u32, start_bytes: u64) -> Result<(), crate::outcome::Fail> {
    use crate::outcome::Fail;
    if !src.is_block {
        return match image_loop_occupant(src) {
            Ok(None) => Ok(()),
            Ok(Some(dev)) => Err(Fail::refused(format!(
                "{dev} is a loop onto this image and is in use — unmount/swapoff it first (editing the image changes a live filesystem)"
            ))),
            Err(e) => Err(Fail::infra(format!(
                "cannot confirm no loop has this image in use: {e} — refusing (fail-closed)"
            ))),
        };
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
pub(crate) fn attach_loop(src: &FileSource, off: u64, len: u64) -> Result<String, FsError> {
    let losetup = find_tool("losetup")?;
    let base_args: Vec<String> = if src.sector_size != 512 {
        vec![
            "-o".into(), off.to_string(), "--sizelimit".into(), len.to_string(),
            "--sector-size".into(), src.sector_size.to_string(),
        ]
    } else {
        vec!["-o".into(), off.to_string(), "--sizelimit".into(), len.to_string()]
    };
    let img = src.path.clone();
    let mut show = Command::new(&losetup)
        .args(&base_args)
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
pub(crate) fn detach_loop(loopdev: &str) {
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