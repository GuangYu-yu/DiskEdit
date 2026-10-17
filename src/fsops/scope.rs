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

/// 按范围执行 FS 操作的统一入口，with_partition_device 的公共底层
pub(crate) fn with_scope_device<T, E, F>(src: &FileSource, scope: &DeviceScope, f: F) -> Result<T, E>
where
    F: FnOnce(&str) -> Result<T, E>,
    E: From<FsError>,
{
    require_linux()?;
    require_root()?;
    // 区间在此只求一次：分区起点同时作为节点匹配的 want_start
    let (off, len) = scope_byte_range(src, scope)?;
    with_scope_device_ranged(src, scope, off, len, f)
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
    require_linux()?;
    require_root()?;
    if src.is_block {
        // 块设备：分区 = 定位分区节点（/sys/block/<disk>/<part>/start 匹配），不经 loop；
        // 整盘 = 盘节点本身；Range = 无"偏移节点"概念，必须 loop 映射（backing=整盘）
        let dev = match scope {
            DeviceScope::Partition(p) => find_block_partition_node(src, *p, off)?,
            DeviceScope::Whole => src.path.to_string_lossy().into_owned(),
            DeviceScope::Range(..) => {
                let loopdev = attach_loop(src, off, len)?;
                // require 失败也要先 detach 再返回错误，? 短路会漏掉 detach
                let res = require_unmounted(&loopdev).map_err(Into::into).and_then(|()| f(&loopdev));
                detach_loop(&loopdev);
                return res;
            }
        };
        return require_unmounted(&dev).map_err(Into::into).and_then(|()| f(&dev));
    }
    // 镜像：占用只能按 backing 反查——本工具新建的这个 loop 与用户自己映射的是两个
    // 设备，只探测新建的那个看不见用户那份挂载 / swap
    if let Some(occupant) = image_loop_occupant(src).map_err(FsError::from)? {
        return Err(FsError::Io(io::Error::other(format!(
            "{occupant} is a loop onto this image and is in use — unmount/swapoff it first"
        ))).into());
    }
    let loopdev = attach_loop(src, off, len)?;
    // require 失败也要先 detach 再返回错误，? 短路会漏掉 detach
    let res = require_unmounted(&loopdev).map_err(Into::into).and_then(|()| f(&loopdev));
    detach_loop(&loopdev);
    res
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

/// 已打开块设备的**磁盘**名（sysfs `/sys/block/<name>` 下的名字）。
/// 按 fd 的 rdev 反查 `/sys/dev/block/<maj>:<min>`，再向上越过分区目录——不能用用户给出的
/// 路径字面量：`/dev/disk/by-id/...` 的文件名不是 sysfs 目录名，`/dev/mapper/x` 同理，
/// 按字面量拼 `/sys/block/<name>` 会直接读失败，让所有块设备 FS 步都以 Io 收场
#[cfg(target_os = "linux")]
pub(crate) fn block_disk_name(src: &FileSource) -> Result<String, FsError> {
    use std::os::unix::fs::MetadataExt;
    let rdev = src.file.metadata().map_err(FsError::from)?.rdev();
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