//! 在线路径的 sysfs 与挂载占用判定。

use super::*;

/// sysfs 数值文件内容 → u64
pub fn parse_sysfs_u64(s: &str) -> Option<u64> {
    s.trim().parse().ok()
}

/// 分区 sysfs 目录名 → 盘名：先试去 "p<数字>" 后缀（nvme0n1p3/mmcblk0p2/loop0p1），
/// 再试去 "<数字>" 后缀（sda1/vda2），以 /sys/block/<盘名> 真实存在为准
pub fn disk_name_from_partition(part_name: &str, disk_exists: impl Fn(&str) -> bool) -> Option<String> {
    if let Some(pos) = part_name.rfind('p') {
        let (base, suffix) = part_name.split_at(pos);
        // rfind 命中 ⇒ suffix 非空且以 'p' 开头，只余"其余全为数字"待验
        if suffix[1..].bytes().all(|b| b.is_ascii_digit())
            && !base.is_empty() && disk_exists(base)
        {
            return Some(base.to_string());
        }
    }
    let digits_end = part_name.bytes().rposition(|b| !b.is_ascii_digit()).map(|i| i + 1);
    // i + 1 ≥ 1：Some(0) 不可达，"全数字"（盘名无分区号）由 None 表达
    match digits_end {
        None => None,
        Some(end) => {
            let base = &part_name[..end];
            // digits_end 分支里 base 至少含一个非数字字符，非空判据恒真，只验 disk_exists
            disk_exists(base).then(|| base.to_string())
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
/// 读失败如实上抛：折叠成"未挂载"会让离线写表在挂载中的分区上开跑。
/// disk_name = 盘节点文件名（如 "sda"）；dm/mapper 等无法反解的设备不算命中
pub fn find_mountpoint(disk_name: &str, pno: u32) -> std::io::Result<Option<PathBuf>> {
    let entries = crate::fsops::read_mounts()?;
    for e in &entries {
        if part_in_use(&e.source, disk_name, pno)? {
            return Ok(Some(PathBuf::from(&e.mount_point)));
        }
    }
    Ok(None)
}

/// 目标盘上的分区是否是活动 swap（/proc/swaps；man proc_swaps(5)：首行为表头，
/// 其后每行第一字段为设备路径。反解方式同 find_mountpoint）。
/// 读失败如实上抛——整体读失败与逐条目反解失败（part_in_use）都算：
/// 折叠成"无 swap"会让活动 swap 的锁下复核被一次读故障绕过（fail-open）
pub fn swap_active(disk_name: &str, pno: u32) -> std::io::Result<bool> {
    let s = fs::read_to_string("/proc/swaps")?;
    for l in s.lines().skip(1) {
        // 首行表头已跳过；其余行首字段为设备路径
        if let Some(d) = l.split_whitespace().next()
            && part_in_use(d, disk_name, pno)?
        {
            return Ok(true);
        }
    }
    Ok(false)
}

/// 命中 = 该设备是 disk_name 上的分区 pno。读失败上抛（fail-closed）：折叠成
/// "不在使用"会让锁下占用复核被一次读故障绕过。`metadata` 与 `partition` 属性的
/// NotFound 例外——前者是 mountinfo 里 pseudo-fs 条目（如 cgroup2，source 不是
/// 路径）的常态；后者是整盘目录的判别结果（无 partition 属性 = 整盘非分区）
pub(crate) fn part_in_use(dev: &str, disk_name: &str, pno: u32) -> std::io::Result<bool> {
    let meta = match fs::metadata(dev) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e),
    };
    if !meta.file_type().is_block_device() {
        return Ok(false);
    }
    let rdev = meta.st_rdev();
    // dev_t 分解主次设备号（man makedev(3) 的 major()/minor()）
    let sysdir = PathBuf::from(format!("/sys/dev/block/{}:{}", libc::major(rdev), libc::minor(rdev)));
    // <part>/partition：内核 sysfs-block ABI 属性，内容 = 分区号；整盘目录无此属性（据此区分二者）
    let part = match sysfs_u64(&sysdir.join("partition")) {
        Ok(p) => p,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e),
    };
    if part as u32 != pno {
        return Ok(false);
    }
    let link = sysdir.read_link()?;
    let name = link.file_name().and_then(|s| s.to_str()).ok_or_else(|| {
        std::io::Error::other(format!("unusable sysfs link name at {}", sysdir.display()))
    })?;
    Ok(disk_name_from_partition(name, |n| Path::new("/sys/block").join(n).exists())
        .is_some_and(|d| d == disk_name))
}