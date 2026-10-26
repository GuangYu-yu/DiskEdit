//! 在线目标解析：分区节点、邻居与 BLKPG 调用。

use super::*;

pub(crate) struct OnlineTarget {
    pub(crate) disk_dev: PathBuf,
    pub(crate) part_dev: PathBuf,
    /// 挂载点。`None` 表示本目标不来自挂载点（PV 路径）：xfs / btrfs 的在线工具与 btrfs
    /// 缩容都以挂载点为参数，这类目标上没有可用值
    pub(crate) mnt: Option<PathBuf>,
    pub(crate) pno: u32,
    pub(crate) start_bytes: u64,
    pub(crate) part_len_bytes: u64,
    pub(crate) disk_cap_bytes: u64,
    pub(crate) logical_block: u64,
    pub(crate) others: Vec<(u64, u64)>, // 相邻分区 [start, len)（字节，本分区除外）
}

/// `/dev` 下的节点必须与 sysfs 记录的设备号同源（判据与实现在
/// [`crate::dev::ensure_node_is_device`]，fsops 与在线路径共用同一份）
pub(crate) use crate::dev::ensure_node_is_device;

pub(crate) fn sysfs_u64(path: &Path) -> io::Result<u64> {
    let s = fs::read_to_string(path)?;
    parse_sysfs_u64(&s).ok_or_else(|| io::Error::other(format!("unparsable sysfs value in {}", path.display())))
}

/// 收集盘上相邻分区的字节区间（sysfs 遍历），`skip` 命中的条目除外。
/// 两条在线路径（resolve_target / resolve_pv_target）共用的唯一实现；
/// 算不出真值的邻居意味着**无法证明不重叠**——按校验失败上抛，绝不静默跳过
pub(crate) fn collect_neighbours(sysroot: &Path, skip: impl Fn(&Path) -> bool) -> io::Result<Vec<(u64, u64)>> {
    let mut others = Vec::new();
    for entry in fs::read_dir(sysroot)? {
        let p = entry?.path();
        if skip(&p) {
            continue;
        }
        // 非目录条目（uevent/dev/size/stat 等 sysfs 属性文件）不可能是分区：对其拼
        // "partition" 会得到 ENOTDIR（os error 20），那是"这不是个目录"，与"分区属性
        // 读不出来"不同类，必须先排除，否则整条在线流程会被属性文件打挂
        if !p.is_dir() {
            continue;
        }
        // "partition 属性不存在"（NotFound）= 整盘目录（queue/power/holders 等），跳过；
        // 其余 stat 失败按校验失败上抛——exists() 把任何 I/O 错误都折叠成 false，会让
        // 算不出真值的邻居被当成"不是分区"静默放过，重叠检验就此失守
        match fs::metadata(p.join("partition")) {
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => return Err(io::Error::other(format!("cannot stat {}: {e}", p.join("partition").display()))),
        }
        let s = sysfs_u64(&p.join("start"))?;
        let l = sysfs_u64(&p.join("size"))?;
        let sb = s.checked_mul(512)
            .ok_or_else(|| io::Error::other(format!("neighbour {} start overflows u64 bytes", p.display())))?;
        let lb = l.checked_mul(512)
            .ok_or_else(|| io::Error::other(format!("neighbour {} size overflows u64 bytes", p.display())))?;
        others.push((sb, lb));
    }
    Ok(others)
}

/// mountpoint → /proc/self/mountinfo 匹配（挂载点已解码八进制转义，man
/// proc_pid_mountinfo(5)）→ (分区块设备节点, 规范化挂载点)。
/// multi-device/bind 挂载不支持
pub(crate) fn resolve_part_dev(mountpoint: &Path) -> io::Result<(PathBuf, PathBuf)> {
    let mnt = fs::canonicalize(mountpoint)?;
    // 本命名空间的挂载表：叠挂判定针对"此刻在这台机器上叠上去的层"，
    // 全命名空间视图会把同一挂载经 shared 传播进其他命名空间的副本也算进来
    let entries = crate::fsops::read_own_mounts()?;
    let mnt_str = mnt.to_string_lossy().into_owned();
    let raw_str = mountpoint.to_string_lossy().into_owned();
    // 同一挂载点多条记录 = 发生过叠挂（overmount）：mountinfo 里首条是被遮的下层，
    // 取首/取尾都在猜"实际生效的是哪一层文件系统"，保守拒绝。bind/multi-device
    // 共享挂载点同样落到这条或多条非块设备分支，各自的拒绝语义不变
    let matched: Vec<_> = entries.iter().filter(|e| e.mount_point == mnt_str || e.mount_point == raw_str).collect();
    if matched.len() > 1 {
        return Err(io::Error::other(format!(
            "{} is mounted multiple times (overmount) — which filesystem is in effect cannot be determined",
            mnt.display()
        )));
    }
    if let Some(e) = matched.first() {
        let dev = PathBuf::from(&e.source);
        if fs::metadata(&dev)?.file_type().is_block_device() {
            return Ok((dev, mnt));
        }
        return Err(io::Error::other(format!(
            "{} is not a block device node (multi-device/bind mounts unsupported online)",
            dev.display()
        )));
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        format!("mountpoint {} not found in /proc/self/mountinfo", mnt.display()),
    ))
}

pub(crate) fn resolve_target(mountpoint: &Path) -> io::Result<OnlineTarget> {
    let (part_dev, mnt) = resolve_part_dev(mountpoint)?;
    let meta = fs::metadata(&part_dev)?;
    let rdev = meta.st_rdev();
    // dev_t 分解主次设备号（man makedev(3) 的 major()/minor()）
    let (maj, min) = (libc::major(rdev), libc::minor(rdev));

    let sysdir = PathBuf::from(format!("/sys/dev/block/{maj}:{min}"));
    // 内核 sysfs-block ABI：存在 partition 属性即说明该设备是分区，内容为分区号
    let pno = sysfs_u64(&sysdir.join("partition")).map_err(|_| {
        io::Error::other(format!("{} is not a partition (no sysfs partition attribute)", part_dev.display()))
    })? as u32;
    let start_sectors = sysfs_u64(&sysdir.join("start"))?;
    let size_sectors = sysfs_u64(&sysdir.join("size"))?;
    // 内核 sysfs-block ABI：start/size 以 512 字节扇区计（与设备逻辑块大小
    // queue/logical_block_size 无关，两者不可混用）。sysfs 值理论到不了 u64 溢出，
    // 但这是外部输入：回绕出来的"字节偏移"会伪装成一个正常分区，乘法必须 checked
    let start_bytes = start_sectors.checked_mul(512).ok_or_else(|| io::Error::other("partition start overflows u64 bytes"))?;
    let part_len_bytes = size_sectors.checked_mul(512).ok_or_else(|| io::Error::other("partition size overflows u64 bytes"))?;

    let part_name = sysdir
        .read_link()?
        .file_name()
        .ok_or_else(|| io::Error::other("sysfs link without basename"))?
        .to_string_lossy()
        .into_owned();
    let disk_exists = |n: &str| Path::new("/sys/block").join(n).exists();
    let disk_name = disk_name_from_partition(&part_name, disk_exists).ok_or_else(|| {
        io::Error::other(format!("cannot derive disk name from partition {part_name}"))
    })?;
    let disk_cap_bytes = sysfs_u64(&Path::new("/sys/block").join(&disk_name).join("size"))?
        .checked_mul(512)
        .ok_or_else(|| io::Error::other("disk capacity overflows u64 bytes"))?;
    // queue/logical_block_size：内核 sysfs 只读属性，单位 = 字节；
    // 0 是异常输入，后续尺寸换算（字节 ÷ 逻辑块）会除零，在此拦截
    let logical_block =
        sysfs_u64(&Path::new("/sys/block").join(&disk_name).join("queue/logical_block_size"))?;
    if logical_block == 0 {
        return Err(io::Error::other("logical_block_size reported as 0 — sysfs value invalid"));
    }
    let disk_dev = PathBuf::from("/dev").join(&disk_name);
    // 盘名由分区 sysfs 目录名推得，/dev 下的同名节点必须真是这台设备：容量、
    // logical_block_size、邻居区间全部取自 sysfs，而 sfdisk / partx / BLKPG / pvresize
    // 只拿到这个路径——名字指向另一台设备时"几何来自 A、写入落在 B"
    ensure_node_is_device(&disk_dev, &Path::new("/sys/block").join(&disk_name))?;

    // 相邻分区区间（本分区除外），fail-fast 前置自查；内核 -EBUSY 兜底
    let this_dev = (maj as u64, min as u64);
    let others = collect_neighbours(&Path::new("/sys/block").join(&disk_name), |p| is_same_device(p, this_dev))?;

    Ok(OnlineTarget {
        disk_dev,
        part_dev,
        mnt: Some(mnt),
        pno,
        start_bytes,
        part_len_bytes,
        disk_cap_bytes,
        logical_block,
        others,
    })
}

/// BLKPG_RESIZE_PARTITION：对整盘 fd 调用，pno 定位分区，start 固定为现值
pub(crate) fn blkpg_resize(t: &OnlineTarget, new_len_bytes: u64) -> io::Result<()> {
    let disk = fs::OpenOptions::new().read(true).write(true).open(&t.disk_dev)?;
    crate::ioctl::blkpg_resize_partition(&disk, t.start_bytes, new_len_bytes, t.pno)
}