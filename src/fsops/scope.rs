//! 分区设备范围解析：块设备节点、GPT/MBR 分区区间与 loop 兜底。

use super::*;

/// FS 操作的目标范围：常规 = 表内分区；superfloppy = 整盘（无表，FS 即盘）；
/// overlay = 分区内任意字节区间（OpenWrt RW 层，须经 offset loop 映射）
pub enum DeviceScope {
    Partition(u32),
    Whole,
    Range(u64, u64),
}

pub(crate) fn scope_byte_range(src: &FileSource, scope: &DeviceScope) -> Result<(u64, u64), FsError> {
    match scope {
        DeviceScope::Partition(p) => partition_byte_range(src, *p),
        DeviceScope::Whole => Ok((0, src.size)),
        DeviceScope::Range(off, len) => Ok((*off, *len)),
    }
}

/// 镜像上已有 loop 的放行口径。FS 步要求目标完全空闲；LVM PV 收尾（pvresize）
/// 允许 DM 持有者——它写的是 PV 元数据，持有该分区的 dm 栈不解析这部分字节
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ImageLoopPolicy {
    RefuseAny,
    #[cfg(target_os = "linux")]
    AllowDm,
}

/// 已建立的目标设备：loop 由它自己撤销。存在的意义是把"建立"和"撤销"绑成一个
/// 对象——写路径要在设备就绪之后、第一次不可回滚的写之前做决定（如 mkfs 的屏障），
/// 那一步必须发生在 `attach_loop` 之后、`f(&dev)` 之前，而不是在这个函数外面
pub(crate) struct ScopeDevice {
    path: String,
    is_loop: bool,
}

impl ScopeDevice {
    pub(crate) fn path(&self) -> &str {
        &self.path
    }
}

impl Drop for ScopeDevice {
    fn drop(&mut self) {
        if self.is_loop {
            release_loop(&self.path);
        }
    }
}

/// 撤销 loop 映射并把失败浮出：`Drop` 不能改返回值，故记入进程级清单，由 `main`
/// 在成功路径上把退出码降为"完成但有待办"。映射已解除的正常情形不产生任何输出
pub(crate) fn release_loop(loopdev: &str) {
    if let Some(why) = release_failure(detach_loop(loopdev)) {
        let msg = format!(
            "loop device {loopdev} was not detached ({why}) — the image may still be held by it; \
             release it with `losetup -d {loopdev}`"
        );
        eprintln!("warning: {msg}");
        note_release_leak(msg);
    }
}

/// 按范围执行 FS 操作的统一入口，with_partition_device 的公共底层
pub(crate) fn with_scope_device<T, E, F>(src: &FileSource, scope: &DeviceScope, f: F) -> Result<T, E>
where
    F: FnOnce(&str) -> Result<T, E>,
    E: From<FsError>,
{
    with_scope_device_policy(src, scope, ImageLoopPolicy::RefuseAny, f)
}

fn with_scope_device_policy<T, E, F>(
    src: &FileSource,
    scope: &DeviceScope,
    policy: ImageLoopPolicy,
    f: F,
) -> Result<T, E>
where
    F: FnOnce(&str) -> Result<T, E>,
    E: From<FsError>,
{
    require_linux()?;
    require_root()?;
    // 区间在此只求一次：分区起点同时作为节点匹配的 want_start
    let (off, len) = scope_byte_range(src, scope)?;
    with_scope_device_ranged_policy(src, scope, off, len, policy, f)
}

/// 区间已由调用方解析好时用它：调用方手里的其他判据（如 mkfs 的擦除区间）与本处的
/// 设备建立必须**同源**，各自再解析一次会在两次读表之间开出一个窗口
pub(crate) fn with_scope_device_ranged<T, E, F>(
    src: &FileSource,
    scope: &DeviceScope,
    off: u64,
    len: u64,
    f: F,
) -> Result<T, E>
where
    F: FnOnce(&str) -> Result<T, E>,
    E: From<FsError>,
{
    with_scope_device_ranged_policy(src, scope, off, len, ImageLoopPolicy::RefuseAny, f)
}

fn with_scope_device_ranged_policy<T, E, F>(
    src: &FileSource,
    scope: &DeviceScope,
    off: u64,
    len: u64,
    policy: ImageLoopPolicy,
    f: F,
) -> Result<T, E>
where
    F: FnOnce(&str) -> Result<T, E>,
    E: From<FsError>,
{
    let dev = resolve_scope_device(src, scope, off, len, policy)?;
    f(dev.path())
}

/// 建立目标设备并确认它没有被别的栈占用；返回时设备已就绪，`drop` 即撤销。
/// 写路径需要在"设备已建立、还没写第一个字节"这个位置上做决定（如 mkfs 的屏障），
/// 那一步只有在这里拿得到设备句柄
pub(crate) fn resolve_scope_device(
    src: &FileSource,
    scope: &DeviceScope,
    off: u64,
    len: u64,
    policy: ImageLoopPolicy,
) -> Result<ScopeDevice, FsError> {
    require_linux()?;
    require_root()?;
    #[cfg(not(target_os = "linux"))]
    let _ = policy;
    if matches!(scope, DeviceScope::Partition(_)) {
        // 表项必须落在容器内。起点越过容量、或起点在盘内而末端越过盘尾，都不是
        // "内核视图尚未同步"：block/partitions/core.c 对后者保留分区但把长度截断到
        // 容量，原始表项与内核视图因此永远不等，重读分区表也不能让表项成立。
        // 用减法比较：`off + len` 在 u64 上会回绕
        if off > src.size {
            return Err(FsError::invalid(format!(
                "partition table entry starts at byte {off}, past the end of the device ({} bytes) \
                 — the entry lies outside the device and cannot be repaired by re-reading the partition table",
                src.size
            )));
        }
        if len > src.size - off {
            return Err(FsError::invalid(format!(
                "partition table entry spans {len} bytes from byte {off} but the device is {} bytes \
                 — the entry lies outside the device and cannot be repaired by re-reading the partition table",
                src.size
            )));
        }
    }
    if src.is_block {
        // 块设备：分区 = 定位分区节点（/sys/block/<disk>/<part>/start 匹配），不经 loop；
        // 整盘 = 盘节点本身；Range = 无"偏移节点"概念，必须 loop 映射（backing=整盘）
        return match scope {
            DeviceScope::Partition(p) => {
                let dev = find_block_partition_node(src, *p, off)?;
                // 内核分区长度与盘上表项必须同源：mkfs 的擦除区间取自表内长度，而
                // wipe_zero 按这个路径写核心里那个设备——节点比表项长时擦除越过表项末端。
                // 上面已排除越界，这里的不等只可能是内核视图尚未跟上表，重读分区表有解
                #[cfg(target_os = "linux")]
                {
                    let node = crate::dev::sysfs_node(Path::new(&dev))?;
                    let kernel_len = crate::dev::sysfs_capacity(&node)?;
                    if kernel_len != len {
                        return Err(FsError::invalid(format!(
                            "kernel reports {dev} as {kernel_len} bytes but the on-disk table says {len} bytes \
                             — kernel partition size differs from the table entry; make the kernel re-read the partition table first"
                        )));
                    }
                }
                require_unmounted(&dev)?;
                Ok(ScopeDevice { path: dev, is_loop: false })
            }
            DeviceScope::Whole => {
                let dev = src.path.to_string_lossy().into_owned();
                require_unmounted(&dev)?;
                Ok(ScopeDevice { path: dev, is_loop: false })
            }
            DeviceScope::Range(..) => {
                let loopdev = attach_loop(src, off, len)?;
                // require 失败也要先 detach 再返回错误，? 短路会漏掉 detach
                if let Err(e) = require_unmounted(&loopdev) {
                    release_loop(&loopdev);
                    return Err(e);
                }
                Ok(ScopeDevice { path: loopdev, is_loop: true })
            }
        };
    }
    // 镜像：占用只能按 backing 文件身份反查（LOOP_GET_STATUS64 的 lo_device/lo_inode，
    // 不依赖路径解析）——本工具新建的这个 loop 与用户自己映射的是两个设备，只探测
    // 新建的那个看不见用户那份挂载 / swap / 栈持有。探测本身只在 Linux 上存在，
    // 别的平台已由上面的 require_linux 拒绝
    #[cfg(target_os = "linux")]
    if let Some(occupant) = image_loop_occupancy(src)? {
        let why = match occupant {
            LoopOccupancy::Live(dev) => Some(format!(
                "{dev} is a loop onto this image and is in use — unmount/swapoff it first"
            )),
            LoopOccupancy::Held { loopdev: _, holders }
                if policy == ImageLoopPolicy::AllowDm && dm_only(&holders) =>
            {
                None
            }
            LoopOccupancy::Held { loopdev, holders } => Some(format!(
                "{loopdev} is a loop onto this image and is held by {} — deactivate that stack first",
                holders.join(", ")
            )),
            // 内核拒绝独占打开：本工具拿不到这条内核级证据，按"在用"处理
            LoopOccupancy::KernelBusy { loopdev } => Some(format!(
                "{loopdev} is a loop onto this image and the kernel refuses an exclusive open (EBUSY) — \
                 it is still in use (mounted, active swap, or claimed by another stack)"
            )),
            LoopOccupancy::Unclaimable { loopdev, why } => Some(format!(
                "cannot take an exclusive claim on {loopdev}, a loop onto this image: {why}"
            )),
        };
        if let Some(why) = why {
            return Err(FsError::Io(io::Error::other(why)));
        }
    }
    let loopdev = attach_loop(src, off, len)?;
    // require 失败也要先 detach 再返回错误，? 短路会漏掉 detach
    if let Err(e) = require_unmounted(&loopdev) {
        release_loop(&loopdev);
        return Err(e);
    }
    Ok(ScopeDevice { path: loopdev, is_loop: true })
}

/// 在分区上执行操作的统一入口。
/// - 镜像文件：`losetup -o <off> --sizelimit <len> [-S ss] -f --show <img>`
/// - 块设备：直接定位分区设备节点（/sys/block/<disk>/<part>/start 匹配），不经 loop
pub fn with_partition_device<T, E, F>(src: &FileSource, part: u32, f: F) -> Result<T, E>
where
    F: FnOnce(&str) -> Result<T, E>,
    E: From<FsError>,
{
    with_scope_device(src, &DeviceScope::Partition(part), f)
}

/// 镜像上跑 LVM 收尾（pvresize/lvextend）的入口：与 [`with_partition_device`] 相同，
/// 区别只在镜像已有 loop 被 DM 持有时放行——pvresize 写 PV 元数据所在的扇区，持有该
/// 分区的 dm 栈按自己的映射读逻辑字节、不解析 PV 元数据；表项只向右增长也不改变已
/// 映射的扇区（与 [`ensure_idle_before_write`] 的 Extend 放行同一判据）。挂载与活动
/// swap 仍拒绝：经栈顶看到的是被改写的同一段字节。非 DM 持有者（MD、loop 等）没有
/// 这种增量语义，同样拒绝
#[cfg(target_os = "linux")]
pub fn with_partition_device_for_lvm<T, E, F>(src: &FileSource, part: u32, f: F) -> Result<T, E>
where
    F: FnOnce(&str) -> Result<T, E>,
    E: From<FsError>,
{
    with_scope_device_policy(src, &DeviceScope::Partition(part), ImageLoopPolicy::AllowDm, f)
}

/// 在 /sys/block/<disk>/<part>/ 按 start 匹配分区设备节点（块设备路径用）。
/// want_start = 分区起始字节，调用方已由 partition_byte_range 求得。
/// start/size 属性为内核 sysfs-block ABI（Documentation/ABI/testing/sysfs-block，
/// 单位恒为 512 字节扇区，与设备逻辑块大小无关），换算字节偏移须用 512 而非 src.sector_size
pub(crate) fn find_block_partition_node(src: &FileSource, part: u32, want_start: u64) -> Result<String, FsError> {
    #[cfg(target_os = "linux")]
    {
        let disk = block_disk_name(src)?;
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
            // 分区号交叉校验：只有一个维度相同时，内核视图与盘上表可能已在编号上分叉，
            // 只按 start 认节点会把操作落到另一个分区节点上。号也要读得出来且相符
            let Ok(num_txt) = std::fs::read_to_string(entry.path().join("partition")) else { continue };
            let Ok(num) = num_txt.trim().parse::<u32>() else { continue };
            if num != part {
                continue;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            let dev = format!("/dev/{name}");
            // 节点名由 sysfs 目录名推得，只是路径；实际落点由设备号决定。同名节点指向
            // 另一台设备时"几何来自 A、写入落在 B"，调用方拿到的路径无从自行发现
            crate::dev::ensure_node_is_device(Path::new(&dev), &entry.path()).map_err(FsError::from)?;
            return Ok(dev);
        }
        Err(FsError::invalid(format!("partition node for part {part} not found under /sys/block/{disk}")))
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (src, part, want_start);
        Err(FsError::Io(io::Error::new(io::ErrorKind::Unsupported, "not linux")))
    }
}

/// 已打开块设备的**磁盘**名（sysfs `/sys/block/<name>` 下的名字）。
/// 按 fd 的 rdev 反查 `/sys/dev/block/<maj>:<min>`，再向上越过分区目录——不能用用户给出的
/// 路径字面量：`/dev/disk/by-id/...` 的文件名不是 sysfs 目录名，`/dev/mapper/x` 同理，
/// 按字面量拼 `/sys/block/<name>` 会直接读失败，让所有块设备 FS 步都以 Io 收场
#[cfg(target_os = "linux")]
pub(crate) fn block_disk_name(src: &FileSource) -> Result<String, FsError> {
    use std::os::unix::fs::MetadataExt;
    let rdev = src.metadata().map_err(FsError::from)?.rdev();
    if rdev == 0 {
        return Err(FsError::Io(io::Error::other("open target reports no device number")));
    }
    let sysdev = Path::new("/sys/dev/block").join(format!("{}:{}", libc::major(rdev), libc::minor(rdev)));
    let mut node = std::fs::canonicalize(&sysdev)
        .map_err(|e| FsError::Io(io::Error::other(format!("no sysfs node for {sysdev:?}: {e}"))))?;
    // 分区目录带 `partition` 属性；整盘目录没有。目标是分区节点时也要落到它的父盘
    while node.join("partition").exists() {
        node = node.parent()
            .ok_or_else(|| FsError::Io(io::Error::other("sysfs partition node has no parent disk")))?
            .to_path_buf();
    }
    node.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .ok_or_else(|| FsError::Io(io::Error::other("sysfs disk node has no name")))
}

pub(crate) fn partition_byte_range(src: &FileSource, part: u32) -> Result<(u64, u64), FsError> {
    // 唯一实现在 gpt_policy::partition_bytes（GPT 优先、MBR 兜底，换算用表自身的 ss）。
    // 出口语义由 FsError 的既有映射承担：Refused ⇒ 请求与现状不符(10)，环境/盘内容故障 ⇒
    // Io(30)。本层不把分类重解释一遍——那会让"同一事实的第二处判据"重新长出来
    crate::gpt_policy::partition_bytes(src, part).map_err(|f| match f {
        crate::outcome::Fail::Refused(m) => FsError::invalid(m),
        crate::outcome::Fail::Infra(m) | crate::outcome::Fail::Failed(m) => FsError::Io(io::Error::other(m)),
    })
}