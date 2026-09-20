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

use crate::outcome::{Outcome, Pending, PendingKind};
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

/// sysfs `dev` 属性内容 "major:minor" → 设备号（Documentation/ABI: sysfs-block 的 dev）
pub fn parse_dev_attr(s: &str) -> Option<(u64, u64)> {
    let (maj, min) = s.trim().split_once(':')?;
    Some((maj.parse().ok()?, min.parse().ok()?))
}

/// 该 sysfs 目录是否就是目标设备本身。
/// 必须按**设备号**判：`/sys/dev/block/M:m` 是指向 `/sys/devices/.../block/<disk>/<part>`
/// 的符号链接，而按盘名遍历得到的是 `/sys/block/<disk>/<part>`——两者做词法比较永不相等，
/// 于是目标分区会被算进"相邻分区"，使在线扩缩容恒被"与相邻分区重叠"拒绝
pub fn is_same_device(dir: &Path, dev: (u64, u64)) -> bool {
    std::fs::read_to_string(dir.join("dev")).ok().and_then(|s| parse_dev_attr(&s)) == Some(dev)
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
/// （> 现分区 = grow；< 现分区 = 仅 btrfs 支持）。
/// 结果统一为 Outcome（退出码只在 outcome 模块映射）：10=拒绝（未动盘）/
/// 20=布局已写但后置条件或内核视图未跟上 / 30=执行失败
pub fn resize_online(mountpoint: &Path, size: Option<u64>) -> Outcome {
    imp::resize_online(mountpoint, size)
}

/// PV 在线扩容（仅分区层，无 FS 层）：sfdisk 写表 → partx 同步（BLKPG 兜底）→
/// 尺寸核验。PV 本身不挂载，但活动 LV 经 device-mapper 持有分区使 BLKRRPART
/// EBUSY，因此必须走与挂载分区相同的 partx/BLKPG 同步路径；pvresize/lvextend
/// 由调用方在分区尺寸生效后执行（pvresize(8) 支持已属 VG 且有活动 LV 的 PV）。
pub fn resize_pv_online(disk_name: &str, pno: u32, new_len_bytes: u64) -> Outcome {
    imp::resize_pv(disk_name, pno, new_len_bytes)
}

mod imp {
    use super::*;
    use crate::dev::FileSource;
    use crate::fsops::{run, run_input};

    // 在线路径的成对注入点：表是否已落盘全看这一对之间的那次 sfdisk
    crate::movepart::fault_points! {
        /// sfdisk 之前：此刻 abort，分区表确定未改
        fault_online_before_write() = "online-before-write";
        /// sfdisk 之后：此刻 abort，分区表可能已改
        fault_online_after_write() = "online-after-write";
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
        // queue/logical_block_size：内核 sysfs 只读属性，单位 = 字节
        let logical_block =
            sysfs_u64(&Path::new("/sys/block").join(&disk_name).join("queue/logical_block_size"))?;
        let disk_dev = PathBuf::from("/dev").join(&disk_name);
        if !disk_dev.exists() {
            return Err(io::Error::other(format!("disk node {} not found", disk_dev.display())));
        }

        // 相邻分区区间（本分区除外），fail-fast 前置自查；内核 -EBUSY 兜底
        let this_dev = (maj as u64, min as u64);
        let mut others = Vec::new();
        for entry in fs::read_dir(Path::new("/sys/block").join(&disk_name))? {
            let p = entry?.path();
            if is_same_device(&p, this_dev) || !p.join("partition").exists() {
                continue;
            }
            if let (Ok(s), Ok(l)) = (sysfs_u64(&p.join("start")), sysfs_u64(&p.join("size"))) {
                // 邻居区间进重叠自查，而自查正是"能否安全写入"的判据：回绕值会伪装成
                // 正常区间，算不出真值的邻居意味着**无法证明不重叠**——按校验失败处理，
                // 跳过它等于让一次写入可能覆盖邻居
                let sb = s.checked_mul(512)
                    .ok_or_else(|| io::Error::other(format!("neighbour {} start overflows u64 bytes", p.display())))?;
                let lb = l.checked_mul(512)
                    .ok_or_else(|| io::Error::other(format!("neighbour {} size overflows u64 bytes", p.display())))?;
                others.push((sb, lb));
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
        crate::ioctl::blkpg_resize_partition(&disk, t.start_bytes, new_len_bytes, t.pno)
    }

    /// part_resize 的失败按"是否知道表已落盘"三分——这不是措辞差异：
    /// - `NoWrite`：sfdisk 进程未启动，表确定未写
    /// - `Unknown`：sfdisk 执行了但报错。脚本被拒是未写、写入中途失败则可能已写，
    ///   两者在 stdout/stderr 上不可区分 → 不得断言未写
    /// - `Partial`：表已写，仅内核同步失败
    enum PartResizeError {
        NoWrite(io::Error),
        Unknown(io::Error),
        Partial(io::Error),
    }
    impl PartResizeError {
        fn detail(&self) -> &io::Error {
            match self {
                Self::NoWrite(e) | Self::Unknown(e) | Self::Partial(e) => e,
            }
        }
        /// 表未写 → Infra（确定未写盘，不带"盘可能已改变"的提示）；
        /// 无法断言 → Failed；表已写 → Applied（内核视图置为过期，退出码 20）。
        /// 具体原因（含 errno 提示）在发生处打印，契约摘要由 Outcome::report 统一输出
        fn into_outcome(self) -> Outcome {
            match classify_part_resize_error(&self) {
                ResizeFailClass::Infra(msg) => Outcome::infra(msg),
                ResizeFailClass::Failed(msg) => Outcome::failed(msg),
                ResizeFailClass::AppliedStaleKernel(msg) => {
                    eprintln!("warning: partition table written but kernel sync failed: {msg}");
                    Outcome::applied_stale_kernel()
                }
            }
        }
    }

    /// part_resize 失败的三态分类（纯函数核）：副作用（警告打印、Outcome 构造）留在
    /// into_outcome，分类依据本身可单测——"知道表写没写"决定退出码语义，这是契约级判断
    #[derive(Debug)]
    enum ResizeFailClass {
        /// 表确定未写（sfdisk 未启动）
        Infra(String),
        /// sfdisk 执行过但报错，表是否落盘不可断言
        Failed(String),
        /// 表已写，仅内核同步失败（布局生效、内核视图过期）
        AppliedStaleKernel(String),
    }

    fn classify_part_resize_error(e: &PartResizeError) -> ResizeFailClass {
        match e {
            PartResizeError::NoWrite(err) => ResizeFailClass::Infra(err.to_string()),
            PartResizeError::Unknown(err) => ResizeFailClass::Failed(err.to_string()),
            PartResizeError::Partial(err) => ResizeFailClass::AppliedStaleKernel(err.to_string()),
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
        // 在线路径的 durable boundary 就在这一次 sfdisk：之前 abort 表确定未写、
        // 之后 abort 表可能已写。成对注入才能把这条分界变成可断言的事实
        fault_online_before_write();
        let out = run_input(
            "sfdisk",
            &["--no-reread", "--force", "-N", &pno_str, &disk_str],
            &script,
        )
        .map_err(|e| PartResizeError::NoWrite(e.into()))?;
        fault_online_after_write();
        if !out.status.success() {
            return Err(PartResizeError::Unknown(io::Error::other(format!(
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
    /// xfs: xfs_growfs 挂载点；btrfs: resize max 挂载点）。
    /// 程序与参数只写这一处：补救提示复用同一份映射（fs_grow_hint），避免两处漂移
    fn fs_grow_cmd(t: &OnlineTarget, fstype: &str) -> Option<(String, Vec<String>)> {
        let mnt = t.mnt.to_string_lossy().into_owned();
        let dev = t.part_dev.to_string_lossy().into_owned();
        match fstype {
            f if crate::fsid::is_ext(f) => Some(("resize2fs".to_string(), vec![dev])),
            "xfs" => Some(("xfs_growfs".to_string(), vec![mnt])),
            "btrfs" => Some(("btrfs".to_string(), vec!["filesystem".into(), "resize".into(), "max".into(), mnt])),
            _ => None,
        }
    }

    /// fs_grow 的两态失败：**工具没跑起来**（找不到 / 无法执行）时 FS 未被触碰；
    /// **非零退出**说明工具可能改了一半。两者性质不同（infra / failed），
    /// 压成一个错误类型会让调用点无从区分
    enum FsGrowError {
        /// 工具没跑起来：`fsops::run` 的失败侧（工具缺失 / 执行失败）
        Spawn(crate::fsops::FsError),
        Exit(String),
    }

    impl FsGrowError {
        fn detail(&self) -> String {
            match self {
                FsGrowError::Spawn(e) => e.to_string(),
                FsGrowError::Exit(m) => m.clone(),
            }
        }
    }

    fn fs_grow(t: &OnlineTarget, fstype: &str) -> Result<(), FsGrowError> {
        let Some((prog, args)) = fs_grow_cmd(t, fstype) else {
            return Err(FsGrowError::Spawn(crate::fsops::FsError::ToolMissing(format!("{fstype} has no online grow"))));
        };
        let argv: Vec<&str> = args.iter().map(String::as_str).collect();
        let out = run(&prog, &argv).map_err(FsGrowError::Spawn)?;
        if !out.status.success() {
            return Err(FsGrowError::Exit(format!(
                "online FS grow failed: {}",
                String::from_utf8_lossy(&out.stderr)
            )));
        }
        Ok(())
    }

    /// 在线 FS 步的补救命令（与 fs_grow 共用命令映射；无在线工具时回落到离线提示）
    fn fs_grow_hint(t: &OnlineTarget, fstype: &str) -> String {
        match fs_grow_cmd(t, fstype) {
            Some((prog, args)) => format!("{prog} {}", args.join(" ")),
            None => crate::fsops::rescue_hint(fstype, &t.part_dev.to_string_lossy()),
        }
    }

    fn fstype_of(t: &OnlineTarget) -> io::Result<String> {
        // 挂载中的分区不能以 O_EXCL 读写打开（dev::FileSource::open 会失败），只读识别
        let src = FileSource::open_read_only(&t.part_dev)?;
        Ok(crate::fsid::identify(&src, 0, t.part_len_bytes)?.to_string())
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
        // 与 resolve_target 同一防线：sysfs 字节换算全部 checked（见那里的注释）
        let byte_of = |v: u64| v.checked_mul(512).ok_or_else(|| io::Error::other("sysfs sector value overflows u64 bytes"));
        let start_bytes = byte_of(sysfs_u64(&sysdir.join("start"))?)?;
        let part_len_bytes = byte_of(sysfs_u64(&sysdir.join("size"))?)?;
        let disk_cap_bytes = byte_of(sysfs_u64(&sysroot.join("size"))?)?;
        let logical_block = sysfs_u64(&sysroot.join("queue/logical_block_size"))?;
        let this_dev = std::fs::read_to_string(sysdir.join("dev")).ok().and_then(|s| parse_dev_attr(&s));
        let mut others = Vec::new();
        for entry in fs::read_dir(&sysroot)? {
            let p = entry?.path();
            // 与 resolve_target 用同一判据排除目标自身；本函数的 sysdir 由上面同一目录遍历得出，
            // 故 dev 属性缺失时退回路径比较（那种比较在**这个**函数里是成立的）
            let is_target = match this_dev {
                Some(d) => is_same_device(&p, d),
                None => p == sysdir,
            };
            if is_target || !p.join("partition").exists() {
                continue;
            }
            if let (Ok(s), Ok(l)) = (sysfs_u64(&p.join("start")), sysfs_u64(&p.join("size"))) {
                // 与 resolve_target 同一防线：算不出真值的邻居意味着无法证明不重叠
                let sb = s.checked_mul(512)
                    .ok_or_else(|| io::Error::other(format!("neighbour {} start overflows u64 bytes", p.display())))?;
                let lb = l.checked_mul(512)
                    .ok_or_else(|| io::Error::other(format!("neighbour {} size overflows u64 bytes", p.display())))?;
                others.push((sb, lb));
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

    /// 在线路径的独占所有权：设备已挂载 / 被 DM 持有，`O_EXCL` 不可能成功，但**同一把
    /// TargetLock** 仍必须持有——否则在线写表与离线 mutation 可以互相穿插。
    /// 粒度取**整盘**：分区表属于盘，且只有盘粒度才能与"以 `/dev/sdX` 为目标的离线
    /// 调用"落进同一个锁名；锁是 advisory 的，它约束的是本工具的所有入口，不是别人
    fn lock_disk(t: &OnlineTarget) -> Result<crate::targetlock::TargetLock, Outcome> {
        let ident = crate::dev::TargetIdentity::resolve(&t.disk_dev, true, t.disk_cap_bytes);
        // 取锁的失败按 `Fail` 的三个变体折叠成出口语义（唯一映射点在 outcome 模块），
        // 本处不借 `finish` 做转换——那是"结束一次操作"的入口，语义不同
        crate::targetlock::TargetLock::acquire(&ident).map_err(|f| f.into_outcome())
    }

    /// 新尺寸的几何前置检查：对齐 → 容量 → 邻接，两条在线写表路径共用。
    /// 缩容（仅 btrfs）也走这里：起点不变、区间只会更短，容量与邻接两条恒成立，
    /// 需要它们的是 grow；对齐对两个方向都必要——sfdisk 会把未对齐尺寸静默截到扇区界。
    /// 必须全部通过才允许 part_resize 落盘：写表之后这些条件已无法"事后退出"
    fn check_new_range(t: &OnlineTarget, new_len_bytes: u64) -> Result<(), String> {
        if new_len_bytes == 0 || !new_len_bytes.is_multiple_of(t.logical_block) {
            return Err(format!(
                "size {new_len_bytes} not aligned to logical block size {}",
                t.logical_block
            ));
        }
        match t.start_bytes.checked_add(new_len_bytes) {
            None => return Err("size overflow".to_string()),
            Some(end) if end > t.disk_cap_bytes => {
                return Err(format!("new end {end} exceeds disk capacity {}", t.disk_cap_bytes));
            }
            _ => {}
        }
        if !range_free(t.start_bytes, new_len_bytes, &t.others) {
            return Err("new size would overlap an adjacent partition".to_string());
        }
        Ok(())
    }

    /// PV 分区在线扩容：只动分区层，pvresize/lvextend 归调用方
    pub fn resize_pv(disk_name: &str, pno: u32, new_len_bytes: u64) -> Outcome {
        // 解析只读 sysfs / 路径，尚未写盘 → Infra
        let t = match resolve_pv_target(disk_name, pno) {
            Ok(t) => t,
            Err(e) => return Outcome::infra(e.to_string()),
        };
        // 此后每一步都是写盘（写表 / 内核重读），独占权从解析出目标起就取得
        let _owned = match lock_disk(&t) {
            Ok(l) => l,
            Err(o) => return o,
        };
        if new_len_bytes <= t.part_len_bytes {
            return Outcome::refused(format!(
                "PV partition can only grow here (current {} bytes); PV shrink needs the lvreduce/pvresize chain",
                t.part_len_bytes
            ));
        }
        if let Err(msg) = check_new_range(&t, new_len_bytes) {
            return Outcome::refused(msg);
        }
        if let Err(e) = part_resize(&t, new_len_bytes) {
            return e.into_outcome();
        }
        match verify_part_size(&t, new_len_bytes) {
            Ok(()) => Outcome::applied_with(Vec::new()),
            // 表已写、内核报的尺寸却不同 → 内核视图过期（后续 PV 链不得基于旧尺寸）
            Err(e) => {
                eprintln!("warning: partition table updated but kernel reports a different size: {e}");
                Outcome::applied_stale_kernel()
            }
        }
    }

    pub fn resize_online(mountpoint: &Path, size: Option<u64>) -> Outcome {
        // 解析与 FS 识别都是只读的，且先于 part_resize/sfdisk 的首次写盘 → Infra
        let t = match resolve_target(mountpoint) {
            Ok(t) => t,
            Err(e) => return Outcome::infra(e.to_string()),
        };
        // 独占权必须在 FS 步之前取得：本函数既可能写 FS（fs_grow）也可能写表
        // （part_resize），等到写表才取锁会让前面的 FS 写入落在锁外
        let _owned = match lock_disk(&t) {
            Ok(l) => l,
            Err(o) => return o,
        };
        let fstype = match fstype_of(&t) {
            Ok(f) => f,
            Err(e) => return Outcome::infra(e.to_string()),
        };
        let online_shrink = fstype == "btrfs";
        if size.is_some_and(|s| s < t.part_len_bytes) && !online_shrink {
            // xfs 离线同样不可缩（shrink_fs 兜底拒绝），单独说明以免用户改走离线路径
            if fstype == "xfs" {
                return Outcome::refused(
                    "xfs does not support shrinking (online or offline); only growing is possible".to_string(),
                );
            }
            return Outcome::refused(format!(
                "{fstype} cannot shrink online (only btrfs can); unmount and use the offline path where supported"
            ));
        }
        // FS 步未完成 → 一条 Pending（补救命令与 fs_grow 同源）
        let fs_pending = |detail: String| {
            Pending::new(t.pno, PendingKind::Fs, detail, fs_grow_hint(&t, &fstype))
        };

        match size {
            // 扩满现分区：分区不动，FS 工具直接吃满（内核视图无需变更）。
            // spawn 失败 = 什么都没动（infra）；非零退出 = 工具可能改了一半（failed）
            None => match fs_grow(&t, &fstype) {
                Ok(()) => Outcome::applied_with(Vec::new()),
                Err(e) => match e {
                    FsGrowError::Spawn(err) => Outcome::infra(err.to_string()),
                    FsGrowError::Exit(m) => Outcome::failed(m),
                },
            },
            Some(bytes) => {
                if let Err(msg) = check_new_range(&t, bytes) {
                    return Outcome::refused(msg);
                }
                if bytes > t.part_len_bytes {
                    // grow：先自查（内核 EBUSY 兜底），sfdisk 写表 → partx 同步 → FS 工具
                    if let Err(e) = part_resize(&t, bytes) {
                        return e.into_outcome();
                    }
                    if let Err(e) = verify_part_size(&t, bytes) {
                        // 两个正交事实同时成立：内核视图过期 + FS 步未做
                        eprintln!("warning: partition table updated but kernel reports a different size: {e}");
                        let mut o = Outcome::applied_with(vec![fs_pending(format!("partition resized but FS grow skipped: {e}"))]);
                        o.mark_kernel_stale();
                        return o;
                    }
                    match fs_grow(&t, &fstype) {
                        Ok(()) => Outcome::applied_with(Vec::new()),
                        // 分区已扩：无论 spawn 失败还是非零退出，FS 步都是一条未满足后置条件
                        Err(e) => Outcome::applied_with(vec![fs_pending(format!("partition resized but FS grow skipped: {}", e.detail()))]),
                    }
                } else if bytes < t.part_len_bytes {
                    // shrink（仅 btrfs）：FS 先缩（内核校验占用与 256MiB 下限），持久化缩分区
                    let mnt = t.mnt.to_string_lossy().into_owned();
                    match run("btrfs", &["filesystem", "resize", &bytes.to_string(), &mnt]) {
                        // spawn 失败（工具缺失等）发生在本次首次写盘之前：FS 未缩、表未写 → Infra
                        Err(e) => return Outcome::infra(e.to_string()),
                        Ok(out) if !out.status.success() => {
                            return Outcome::failed(format!("btrfs shrink failed: {}", String::from_utf8_lossy(&out.stderr)));
                        }
                        Ok(_) => {}
                    }
                    // FS 已缩：此后任何失败都不再是"未动盘"，表未写的情形只能报 Failed
                    match part_resize(&t, bytes) {
                        Ok(()) => {}
                        // 表已写、仅内核同步失败：FS 已缩 + 内核视图过期，两条正交事实都记上
                        Err(e @ PartResizeError::Partial(_)) => return e.into_outcome(),
                        // 表未写或无法断言：FS 已缩，本次已改变盘上内容 → 不得报"未动盘"
                        Err(e) => {
                            return Outcome::failed(format!(
                                "btrfs shrunk but partition resize skipped: {}",
                                e.detail()
                            ));
                        }
                    }
                    match verify_part_size(&t, bytes) {
                        Ok(()) => Outcome::applied_with(Vec::new()),
                        Err(e) => {
                            eprintln!("warning: partition table shrunk but kernel reports a different size: {e}");
                            Outcome::applied_stale_kernel()
                        }
                    }
                } else {
                    // bytes == 现分区：与 None 分支同性质（没有别的写盘步骤）
                    match fs_grow(&t, &fstype) {
                        Ok(()) => Outcome::applied_with(Vec::new()),
                        Err(e) => match e {
                            FsGrowError::Spawn(err) => Outcome::infra(err.to_string()),
                            FsGrowError::Exit(m) => Outcome::failed(m),
                        },
                    }
                }
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// 三态分类：结论随"表写没写"这一事实走，原始错误信息原样透传
        #[test]
        fn part_resize_error_classification() {
            let cls = |e: &PartResizeError| classify_part_resize_error(e);
            assert!(matches!(cls(&PartResizeError::NoWrite(io::Error::other("spawn"))), ResizeFailClass::Infra(_)));
            assert!(matches!(cls(&PartResizeError::Unknown(io::Error::other("x"))), ResizeFailClass::Failed(_)));
            assert!(matches!(cls(&PartResizeError::Partial(io::Error::other("x"))), ResizeFailClass::AppliedStaleKernel(_)));

            // 错误详情透传（发生处打印的是它，分类不得吞掉或改写）
            match cls(&PartResizeError::Unknown(io::Error::other("sfdisk said no"))) {
                ResizeFailClass::Failed(m) => assert!(m.contains("sfdisk said no"), "{m}"),
                other => panic!("expected Failed, got {other:?}"),
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
    fn dev_attr_parsing() {
        assert_eq!(parse_dev_attr("8:1\n"), Some((8, 1)));
        assert_eq!(parse_dev_attr(" 259:0 "), Some((259, 0)));
        assert_eq!(parse_dev_attr("8"), None);
        assert_eq!(parse_dev_attr("sda"), None);
        assert_eq!(parse_dev_attr("8:"), None);
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