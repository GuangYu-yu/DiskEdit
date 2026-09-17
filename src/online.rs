//! 在线 resize（仅 Linux，须 root）：对挂载中的分区做持久化分区表缩放 + FS 工具扩缩。
//!
//! 两条路径（顺序依据 btrfs-filesystem(8)：先扩分区再扩 FS，先缩 FS 再缩分区）：
//! - grow：sfdisk 写表（持久化；GPT backup header 由 libfdisk 在 write 时修正到盘尾）→
//!   partx -u 同步内核 → FS 工具（ext=resize2fs 挂载设备 / xfs=xfs_growfs 挂载点 /
//!   btrfs=btrfs filesystem resize max 挂载点）
//! - shrink：仅 btrfs（SUSE 存储指南在线矩阵中唯一支持在线缩的 FS）→ FS 先缩 →
//!   sfdisk 写表 → partx -u
//!
//! 必须写盘上表而非纯 BLKPG：BLKPG_RESIZE_PARTITION 只改内核内存中的 bdev size
//! （resizepart(8)："doesn't manipulate partitions on a block device"），盘上表不更新，
//! 重启后分区回缩而 FS 已扩大。故先写盘上表，再让内核同步（partx -u；失败时退回
//! BLKPG ioctl 兜底）。
//!
//! 无 journal/checkpoint：在线场景内核与 FS 是共同写者，崩溃一致性归它们。
//! UAPI 依据：
//! include/uapi/linux/blkpg.h（BLKPG=_IO(0x12,105)、RESIZE=3、start/length 字节、pno 标识、
//! devname/volname 忽略）；block/ioctl.c（BLKPG 分支）：无 CAP_SYS_ADMIN → -EACCES；
//! 对分区 fd 调用、pno ≤ 0、range 非法/溢出、未按逻辑块对齐、超出盘容量 → -EINVAL，
//! 通过后才进入 BLKPG_ADD_PARTITION / BLKPG_RESIZE_PARTITION；内核不检查分区内 FS 占用。
//! block/partitions/core.c bdev_resize_partition：start 必须等于现值、相邻分区重叠 -EBUSY、
//! bdev_set_nr_sectors 对挂载中的分区立即生效。
//!
//! 在线能力矩阵（SUSE 存储指南 + 各工具 man）：ext2/3/4 在线仅 grow、xfs 仅 grow、
//! btrfs grow+shrink；ntfs/vfat/exfat/f2fs 工具要求未挂载，一律拒绝。

use std::fs;
use std::io;
use std::os::linux::fs::MetadataExt;
use std::os::unix::fs::FileTypeExt;
use std::path::{Path, PathBuf};

/// sysfs 数值文件内容 → u64
pub fn parse_sysfs_u64(s: &str) -> Option<u64> {
    s.trim().parse().ok()
}

/// 分区 sysfs 目录名 → 盘名：先试去 "p<数字>" 后缀（nvme0n1p3/mmcblk0p2/loop0p1），
/// 再试去 "<数字>" 后缀（sda1/vda2），以 /sys/block/<盘名> 真实存在为准
pub fn disk_name_from_partition(part_name: &str, disk_exists: impl Fn(&str) -> bool) -> Option<String> {
    if let Some(pos) = part_name.rfind('p') {
        let (base, suffix) = part_name.split_at(pos);
        if !suffix.is_empty() && suffix[1..].bytes().all(|b| b.is_ascii_digit())
            && !base.is_empty() && disk_exists(base)
        {
            return Some(base.to_string());
        }
    }
    let digits_end = part_name.bytes().rposition(|b| !b.is_ascii_digit()).map(|i| i + 1);
    match digits_end {
        Some(0) | None => None,
        Some(end) => {
            let base = &part_name[..end];
            (!base.is_empty() && disk_exists(base)).then(|| base.to_string())
        }
    }
}

/// 新区间 [start, start+len) 与既有区间（本分区除外）无重叠；溢出视为不通过
pub fn range_free(start: u64, len: u64, others: &[(u64, u64)]) -> bool {
    let Some(end) = start.checked_add(len) else { return false };
    others.iter().all(|&(s, l)| end <= s || s.checked_add(l).is_some_and(|e| e <= start))
}

/// 目标盘上的分区是否正被挂载：扫描 /proc/self/mountinfo（man proc_pid_mountinfo(5)），
/// 逐设备经 sysfs maj:min 反解出（盘名, 分区号）与入参比对，命中返回首个挂载点（已解码）。
/// disk_name = 盘节点文件名（如 "sda"）；dm/mapper 等无法反解的设备返回 None
pub fn find_mountpoint(disk_name: &str, pno: u32) -> Option<PathBuf> {
    let entries = crate::fsops::read_mounts().ok()?;
    entries.iter().find_map(|e| {
        part_in_use(&e.source, disk_name, pno).then(|| PathBuf::from(&e.mount_point))
    })
}

/// 目标盘上的分区是否是活动 swap（/proc/swaps；man proc_swaps(5)：首行为表头，
/// 其后每行第一字段为设备路径。反解方式同 find_mountpoint）
pub fn swap_active(disk_name: &str, pno: u32) -> bool {
    let Ok(s) = fs::read_to_string("/proc/swaps") else { return false };
    s.lines()
        .skip(1) // 首行表头
        .filter_map(|l| l.split_whitespace().next().map(|d| d.to_string()))
        .any(|d| part_in_use(&d, disk_name, pno))
}

fn part_in_use(dev: &str, disk_name: &str, pno: u32) -> bool {
    let Ok(meta) = fs::metadata(dev) else { return false };
    if !meta.file_type().is_block_device() {
        return false;
    }
    let rdev = meta.st_rdev();
    // dev_t 分解主次设备号（man makedev(3) 的 major()/minor()）
    let sysdir = PathBuf::from(format!("/sys/dev/block/{}:{}", libc::major(rdev), libc::minor(rdev)));
    // <part>/partition：内核 sysfs-block ABI 属性，内容 = 分区号；整盘目录无此属性（据此区分二者）
    let Ok(part) = imp::sysfs_u64(&sysdir.join("partition")) else { return false };
    if part as u32 != pno {
        return false;
    }
    let Ok(link) = sysdir.read_link() else { return false };
    let Some(name) = link.file_name().and_then(|s| s.to_str()) else { return false };
    disk_name_from_partition(name, |n| Path::new("/sys/block").join(n).exists())
        .is_some_and(|d| d == disk_name)
}

/// 在线 resize 总入口。`size`：None = 扩满现分区；Some(字节) = 绝对新尺寸
/// （> 现分区 = grow；< 现分区 = 仅 btrfs 支持）。返回 (退出码, 消息)：
/// 10=拒绝（未动盘）/ 20=部分完成 / 30=基础设施失败
pub fn resize_online(mountpoint: &Path, size: Option<u64>) -> Result<(), (u8, String)> {
    imp::resize_online(mountpoint, size)
}

/// PV 在线扩容（仅分区层，无 FS 层）：sfdisk 写表 → partx 同步（BLKPG 兜底）→
/// 尺寸核验。PV 本身不挂载，但活动 LV 经 device-mapper 持有分区使 BLKRRPART
/// EBUSY，因此必须走与挂载分区相同的 partx/BLKPG 同步路径；pvresize/lvextend
/// 由调用方在分区尺寸生效后执行（pvresize(8) 支持已属 VG 且有活动 LV 的 PV）。
pub fn resize_pv_online(disk_name: &str, pno: u32, new_len_bytes: u64) -> Result<(), (u8, String)> {
    imp::resize_pv(disk_name, pno, new_len_bytes)
}

mod imp {
    use super::*;
    use crate::dev::FileSource;
    use crate::fsops::{run, run_input};
    use std::os::fd::AsRawFd;

    const BLKPG: u64 = 0x1269; // _IO(0x12,105)
    const BLKPG_RESIZE_PARTITION: i32 = 3;

    #[repr(C)]
    struct BlkpgPartition {
        start: i64,  // 字节
        length: i64, // 字节
        pno: i32,
        devname: [u8; 64], // 内核忽略
        volname: [u8; 64], // 内核忽略
    }

    #[repr(C)]
    struct BlkpgIoctlArg {
        op: i32,
        flags: i32,
        datalen: i32,
        data: *mut BlkpgPartition,
    }

    struct OnlineTarget {
        disk_dev: PathBuf,
        part_dev: PathBuf,
        mnt: PathBuf,
        pno: u32,
        start_bytes: u64,
        part_len_bytes: u64,
        disk_cap_bytes: u64,
        logical_block: u64,
        others: Vec<(u64, u64)>, // 相邻分区 [start, len)（字节，本分区除外）
    }

    pub(super) fn sysfs_u64(path: &Path) -> io::Result<u64> {
        let s = fs::read_to_string(path)?;
        parse_sysfs_u64(&s).ok_or_else(|| io::Error::other(format!("unparsable sysfs value in {}", path.display())))
    }

    /// mountpoint → /proc/self/mountinfo 匹配（挂载点已解码八进制转义，man
    /// proc_pid_mountinfo(5)）→ (分区块设备节点, 规范化挂载点)。
    /// multi-device/bind 挂载不支持
    fn resolve_part_dev(mountpoint: &Path) -> io::Result<(PathBuf, PathBuf)> {
        let mnt = fs::canonicalize(mountpoint)?;
        let entries = crate::fsops::read_mounts()?;
        let mnt_str = mnt.to_string_lossy().into_owned();
        let raw_str = mountpoint.to_string_lossy().into_owned();
        // 取首个匹配挂载点：bind/multi-device 会共享挂载点，此时 source 不是块设备节点 → 拒绝
        if let Some(e) = entries.iter().find(|e| e.mount_point == mnt_str || e.mount_point == raw_str) {
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

    fn resolve_target(mountpoint: &Path) -> io::Result<OnlineTarget> {
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
        // 内核 sysfs-block ABI：start/size 以 512-byte sectors 表示（与设备逻辑块大小
        // queue/logical_block_size 无关，二者不可混用）
        let start_bytes = start_sectors * 512;
        let part_len_bytes = size_sectors * 512;

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
        let disk_cap_bytes = sysfs_u64(&Path::new("/sys/block").join(&disk_name).join("size"))? * 512;
        // queue/logical_block_size：内核 sysfs 只读属性，单位 = 字节
        let logical_block =
            sysfs_u64(&Path::new("/sys/block").join(&disk_name).join("queue/logical_block_size"))?;
        let disk_dev = PathBuf::from("/dev").join(&disk_name);
        if !disk_dev.exists() {
            return Err(io::Error::other(format!("disk node {} not found", disk_dev.display())));
        }

        // 相邻分区区间（本分区除外），fail-fast 前置自查；内核 -EBUSY 兜底
        let mut others = Vec::new();
        for entry in fs::read_dir(Path::new("/sys/block").join(&disk_name))? {
            let p = entry?.path();
            if p == sysdir || !p.join("partition").exists() {
                continue;
            }
            if let (Ok(s), Ok(l)) = (sysfs_u64(&p.join("start")), sysfs_u64(&p.join("size"))) {
                others.push((s * 512, l * 512));
            }
        }

        Ok(OnlineTarget {
            disk_dev,
            part_dev,
            mnt,
            pno,
            start_bytes,
            part_len_bytes,
            disk_cap_bytes,
            logical_block,
            others,
        })
    }

    /// BLKPG_RESIZE_PARTITION：对整盘 fd 调用，pno 定位分区，start 固定为现值
    fn blkpg_resize(t: &OnlineTarget, new_len_bytes: u64) -> io::Result<()> {
        let disk = fs::OpenOptions::new().read(true).write(true).open(&t.disk_dev)?;
        let mut part = BlkpgPartition {
            start: t.start_bytes as i64,
            length: new_len_bytes as i64,
            pno: t.pno as i32,
            devname: [0; 64],
            volname: [0; 64],
        };
        let arg = BlkpgIoctlArg {
            op: BLKPG_RESIZE_PARTITION,
            flags: 0,
            datalen: size_of::<BlkpgPartition>() as i32,
            data: &mut part,
        };
        // SAFETY: arg/part 均为合法 repr(C) 栈对象、调用期间指针有效；BLKPG 编号与手写 blkpg_ioctl_arg 布局匹配（见模块头 UAPI 注释）
        let r = unsafe { libc::ioctl(disk.as_raw_fd() as libc::c_int, BLKPG as libc::Ioctl, &arg) };
        if r == 0 {
            return Ok(());
        }
        let code = io::Error::last_os_error().raw_os_error().unwrap_or(0);
        // errno 语义（内核 block/ioctl.c）：EACCES = 缺 CAP_SYS_ADMIN；EINVAL = 对分区 fd
        // 调用 / pno ≤ 0 / range 非法或溢出 / 未按逻辑块对齐 / 超出盘容量；
        // EBUSY = 与相邻分区重叠（block/partitions/core.c）
        let hint = match code {
            libc::EACCES => "requires root (CAP_SYS_ADMIN)",
            libc::EBUSY => "kernel rejected resize: overlaps another partition",
            libc::EINVAL => "kernel rejected resize (invalid pno/range, misaligned, or beyond capacity)",
            _ => "kernel rejected resize",
        };
        Err(io::Error::other(format!(
            "BLKPG_RESIZE_PARTITION failed: {hint} (errno {code})"
        )))
    }

    /// part_resize 失败语义两段式：sfdisk 未落盘 = Infra（无事发生）；
    /// 已写表但内核同步失败 = Partial（表已改、内核未同步）——调用方据此区分 30/20 退出码
    enum PartResizeError {
        Infra(io::Error),
        Partial(io::Error),
    }
    impl PartResizeError {
        fn into_exit(self) -> (u8, String) {
            match self {
                Self::Infra(e) => (crate::EXIT_INFRA, e.to_string()),
                Self::Partial(e) => (crate::EXIT_PARTIAL, format!("partition table written but kernel sync failed: {e}")),
            }
        }
    }
    impl std::fmt::Display for PartResizeError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            match self {
                Self::Infra(e) | Self::Partial(e) => e.fmt(f),
            }
        }
    }

    /// 持久化分区缩放：sfdisk 改写该分区表项（start 不变，仅 size）→ partx -u
    /// 同步内核；partx 失败以 BLKPG 兜底。挂载中分区的 BLKRRPART 重读必失败，
    /// 故 --no-reread；边界与重叠由调用方预检，--force 关闭 sfdisk 一致性检查。
    fn part_resize(t: &OnlineTarget, new_len_bytes: u64) -> Result<(), PartResizeError> {
        let pno_str = t.pno.to_string();
        let disk_str = t.disk_dev.to_string_lossy().into_owned();
        // sfdisk 脚本里的裸数字按"设备扇区"解释（libfdisk/src/script.c parse_size_value），
        // 即设备逻辑扇区大小（4Kn = 4096B，非恒 512B），故按 t.logical_block 换算。
        // -N：只改指定分区、未指定字段保持原值（空 start/size 继承现值，sfdisk(8)）
        let script = format!(",{}", new_len_bytes / t.logical_block);
        let out = run_input(
            "sfdisk",
            &["--no-reread", "--force", "-N", &pno_str, &disk_str],
            &script,
        )
        .map_err(PartResizeError::Infra)?;
        if !out.status.success() {
            return Err(PartResizeError::Infra(io::Error::other(format!(
                "sfdisk resize failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ))));
        }
        let partx_ok = run("partx", &["-u", "--nr", &pno_str, &disk_str])
            .map(|o| o.status.success())
            .unwrap_or(false);
        if partx_ok {
            return Ok(());
        }
        blkpg_resize(t, new_len_bytes).map_err(PartResizeError::Partial)
    }

    /// 内核同步最终判据：/sys/dev/block/<maj>:<min>/size 须等于期望值。
    /// 该 sysfs-block ABI 属性以 512-byte sectors 表示（与逻辑块大小无关，故除 512，
    /// 与 part_resize 按逻辑扇区换算不同）
    fn verify_part_size(t: &OnlineTarget, new_len_bytes: u64) -> io::Result<()> {
        let meta = fs::metadata(&t.part_dev)?;
        let (maj, min) = (libc::major(meta.st_rdev()), libc::minor(meta.st_rdev()));
        let got = sysfs_u64(&Path::new("/sys/dev/block").join(format!("{maj}:{min}")).join("size"))?;
        let want = new_len_bytes / 512;
        if got != want {
            return Err(io::Error::other(format!(
                "kernel reports partition size {got} sectors, expected {want}"
            )));
        }
        Ok(())
    }

    /// FS 在线 grow 到整分区（ext: resize2fs 对挂载设备无 size = 扩满分区，man resize2fs；
    /// xfs: xfs_growfs 挂载点；btrfs: resize max 挂载点）
    fn fs_grow(t: &OnlineTarget, fstype: &str) -> io::Result<()> {
        let mnt = t.mnt.to_string_lossy().into_owned();
        let dev = t.part_dev.to_string_lossy().into_owned();
        let out = match fstype {
            "ext" | "ext2" | "ext3" | "ext4" => run("resize2fs", &[&dev])?,
            "xfs" => run("xfs_growfs", &[&mnt])?,
            "btrfs" => run("btrfs", &["filesystem", "resize", "max", &mnt])?,
            _ => return Err(io::Error::other(format!("{fstype} has no online grow"))),
        };
        if !out.status.success() {
            return Err(io::Error::other(format!(
                "online FS grow failed: {}",
                String::from_utf8_lossy(&out.stderr)
            )));
        }
        Ok(())
    }

    fn fstype_of(t: &OnlineTarget) -> io::Result<String> {
        // 挂载中的分区不能以 O_EXCL 读写打开（dev::FileSource::open 会失败），只读识别
        let src = FileSource::open_read_only(&t.part_dev)?;
        let ss = src.sector_size;
        Ok(crate::fsid::identify(&src, 0, t.part_len_bytes / ss)?.to_string())
    }

    /// 从（盘名, 分区号）解析 OnlineTarget（PV 路径无挂载点，mnt 置空不用）
    fn resolve_pv_target(disk_name: &str, pno: u32) -> io::Result<OnlineTarget> {
        let disk_dev = PathBuf::from("/dev").join(disk_name);
        if !disk_dev.exists() {
            return Err(io::Error::other(format!("disk node {} not found", disk_dev.display())));
        }
        let sysroot = Path::new("/sys/block").join(disk_name);
        let mut sysdir = None;
        for entry in fs::read_dir(&sysroot)? {
            let p = entry?.path();
            if !p.join("partition").exists() {
                continue;
            }
            if sysfs_u64(&p.join("partition"))? as u32 == pno {
                sysdir = Some(p);
                break;
            }
        }
        let sysdir = sysdir
            .ok_or_else(|| io::Error::other(format!("partition {pno} not found under {}", sysroot.display())))?;
        let name = sysdir
            .file_name()
            .ok_or_else(|| io::Error::other("sysfs entry without basename"))?
            .to_string_lossy()
            .into_owned();
        let part_dev = PathBuf::from("/dev").join(&name);
        if !part_dev.exists() {
            return Err(io::Error::other(format!("partition node {} not found", part_dev.display())));
        }
        let start_bytes = sysfs_u64(&sysdir.join("start"))? * 512;
        let part_len_bytes = sysfs_u64(&sysdir.join("size"))? * 512;
        let disk_cap_bytes = sysfs_u64(&sysroot.join("size"))? * 512;
        let logical_block = sysfs_u64(&sysroot.join("queue/logical_block_size"))?;
        let mut others = Vec::new();
        for entry in fs::read_dir(&sysroot)? {
            let p = entry?.path();
            if p == sysdir || !p.join("partition").exists() {
                continue;
            }
            if let (Ok(s), Ok(l)) = (sysfs_u64(&p.join("start")), sysfs_u64(&p.join("size"))) {
                others.push((s * 512, l * 512));
            }
        }
        Ok(OnlineTarget {
            disk_dev,
            part_dev,
            mnt: PathBuf::new(),
            pno,
            start_bytes,
            part_len_bytes,
            disk_cap_bytes,
            logical_block,
            others,
        })
    }

    /// PV 分区在线扩容：只动分区层，pvresize/lvextend 归调用方
    pub fn resize_pv(disk_name: &str, pno: u32, new_len_bytes: u64) -> Result<(), (u8, String)> {
        let refuse = |m: String| (crate::EXIT_REFUSED, m);
        let t = resolve_pv_target(disk_name, pno).map_err(|e| (crate::EXIT_INFRA, e.to_string()))?;
        if new_len_bytes <= t.part_len_bytes {
            return Err(refuse(format!(
                "PV partition can only grow here (current {} bytes); PV shrink needs the lvreduce/pvresize chain",
                t.part_len_bytes
            )));
        }
        // 与 resize_online 同款对齐拒绝：未按逻辑块对齐会被 sfdisk 静默截断到扇区界
        if !new_len_bytes.is_multiple_of(t.logical_block) {
            return Err(refuse(format!(
                "size {new_len_bytes} not aligned to logical block size {}",
                t.logical_block
            )));
        }
        match t.start_bytes.checked_add(new_len_bytes) {
            None => return Err(refuse("size overflow".into())),
            Some(end) if end > t.disk_cap_bytes => {
                return Err(refuse(format!("new end {end} exceeds disk capacity {}", t.disk_cap_bytes)));
            }
            _ => {}
        }
        if !range_free(t.start_bytes, new_len_bytes, &t.others) {
            return Err(refuse("new size would overlap an adjacent partition".into()));
        }
        part_resize(&t, new_len_bytes).map_err(PartResizeError::into_exit)?;
        verify_part_size(&t, new_len_bytes)
            .map_err(|e| (crate::EXIT_PARTIAL, format!("partition table updated but size mismatch: {e}")))
    }

    pub fn resize_online(mountpoint: &Path, size: Option<u64>) -> Result<(), (u8, String)> {
        let refuse = |m: String| (crate::EXIT_REFUSED, m);
        let partial = |m: String| (crate::EXIT_PARTIAL, m);

        let t = resolve_target(mountpoint).map_err(|e| (crate::EXIT_INFRA, e.to_string()))?;
        let fstype = fstype_of(&t).map_err(|e| (crate::EXIT_INFRA, e.to_string()))?;
        let online_shrink = fstype == "btrfs";
        if size.is_some_and(|s| s < t.part_len_bytes) && !online_shrink {
            // xfs 离线同样不可缩（shrink_fs 兜底拒绝），单独说明以免用户改走离线路径
            if fstype == "xfs" {
                return Err(refuse(
                    "xfs does not support shrinking (online or offline); only growing is possible".to_string(),
                ));
            }
            return Err(refuse(format!(
                "{fstype} cannot shrink online (only btrfs can); unmount and use the offline path where supported"
            )));
        }

        match size {
            // 扩满现分区：分区不动，FS 工具直接吃满
            None => fs_grow(&t, &fstype).map_err(|e| (crate::EXIT_INFRA, e.to_string())),
            Some(bytes) => {
                if bytes == 0 || bytes % t.logical_block != 0 {
                    return Err(refuse(format!(
                        "size {bytes} not aligned to logical block size {}",
                        t.logical_block
                    )));
                }
                if bytes > t.part_len_bytes {
                    // grow：先自查（内核 EBUSY 兜底），sfdisk 写表 → partx 同步 → FS 工具
                    let new_end = t.start_bytes.checked_add(bytes);
                    match new_end {
                        None => return Err(refuse("size overflow".into())),
                        Some(end) if end > t.disk_cap_bytes => {
                            return Err(refuse(format!(
                                "new end {end} exceeds disk capacity {}",
                                t.disk_cap_bytes
                            )));
                        }
                        _ => {}
                    }
                    if !range_free(t.start_bytes, bytes, &t.others) {
                        return Err(refuse("new size would overlap an adjacent partition".into()));
                    }
                    part_resize(&t, bytes).map_err(PartResizeError::into_exit)?;
                    if let Err(e) = verify_part_size(&t, bytes) {
                        return Err(partial(format!(
                            "partition table updated but size mismatch, FS grow skipped: {e}"
                        )));
                    }
                    fs_grow(&t, &fstype)
                        .map_err(|e| partial(format!("partition resized but FS grow skipped: {e}")))
                } else if bytes < t.part_len_bytes {
                    // shrink（仅 btrfs）：FS 先缩（内核校验占用与 256MiB 下限），持久化缩分区
                    let mnt = t.mnt.to_string_lossy().into_owned();
                    let out = run("btrfs", &["filesystem", "resize", &bytes.to_string(), &mnt])
                        .map_err(|e| (crate::EXIT_INFRA, e.to_string()))?;
                    if !out.status.success() {
                        return Err((crate::EXIT_INFRA, format!("btrfs shrink failed: {}", String::from_utf8_lossy(&out.stderr))));
                    }
                    part_resize(&t, bytes)
                        .map_err(|e| partial(format!("btrfs shrunk but partition resize skipped: {e}")))?;
                    verify_part_size(&t, bytes)
                        .map_err(|e| partial(format!("partition table shrunk but size mismatch: {e}")))
                } else {
                    fs_grow(&t, &fstype).map_err(|e| (crate::EXIT_INFRA, e.to_string()))
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;


    #[test]
    fn parse_sysfs_u64_trims() {
        assert_eq!(parse_sysfs_u64("4096\n"), Some(4096));
        assert_eq!(parse_sysfs_u64("  3\n"), Some(3));
        assert_eq!(parse_sysfs_u64("x\n"), None);
    }

    #[test]
    fn disk_name_strip_variants() {
        let exists = |n: &str| matches!(n, "nvme0n1" | "sda" | "loop0" | "mmcblk0");
        assert_eq!(disk_name_from_partition("nvme0n1p3", exists), Some("nvme0n1".into()));
        assert_eq!(disk_name_from_partition("sda1", exists), Some("sda".into()));
        assert_eq!(disk_name_from_partition("loop0p1", exists), Some("loop0".into()));
        assert_eq!(disk_name_from_partition("mmcblk0p2", exists), Some("mmcblk0".into()));
        // 盘名不存在 → None（防误切）
        assert_eq!(disk_name_from_partition("sda9", |_n| false), None);
        // 纯数字目录名不是分区
        assert_eq!(disk_name_from_partition("123", exists), None);
    }

    #[test]
    fn range_free_detects_overlap() {
        let others = vec![(2048u64, 4096u64)]; // [2048, 6144)
        assert!(range_free(6144, 512, &others));
        assert!(range_free(0, 2048, &others));
        assert!(!range_free(6000, 512, &others));
        assert!(!range_free(1024, 4096, &others));
        assert!(!range_free(6100, 100, &others));
    }

    #[test]
    fn range_free_overflow_is_rejection() {
        // 既有区间终点 s+l 溢出：视为重叠，不得通过
        let others = vec![(u64::MAX - 100, 1000u64)];
        assert!(!range_free(u64::MAX - 50, 10, &others));
        // 新区间自身溢出同样拒绝
        assert!(!range_free(u64::MAX - 10, 100, &[]));
    }
}