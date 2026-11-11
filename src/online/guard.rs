//! 在线路径的 sysfs 与挂载占用判定。

use super::*;

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

/// [`find_mountpoint`] 的命中位置
pub enum MountView {
    /// 本命名空间里的挂载点：在线流程随之在同一命名空间对它跑 FS 工具
    Here(PathBuf),
    /// 只在另一个挂载命名空间里挂着：本命名空间既不能用也不能卸。
    /// `pid` 是读到该记录时所在的进程（该命名空间首个可读进程），
    /// 取不到命名空间身份时为 `None`——调用方如实说取不到，不编一个用户查不到的路径
    Elsewhere { mount_point: String, pid: Option<u32> },
}

/// 目标盘上的分区是否正被挂载：扫描全部可读 mount namespace 的 mountinfo
/// （man proc_pid_mountinfo(5)），按字段 3 的 major:minor 与目标分区的 sysfs `dev` 比对。
///
/// 占用判定必须取全部可读命名空间：别的命名空间里的挂载同样在用这块盘，只读本命名
/// 空间会把那种占用读成"未挂载"，随后在仍被挂载的盘上写表或改文件系统。命中后只把
/// 本命名空间的挂载点交给调用方（[`MountView::Here`]）；只在别处挂着的返回
/// [`MountView::Elsewhere`]，连同该命名空间的可查标识（读到的 pid）一起，由调用方拒绝。
///
/// 读失败如实上抛：折叠成"未挂载"会让离线写表在挂载中的分区上开跑。
/// disk_name = 盘节点文件名（如 "sda"）；dm/mapper 等无法反解的设备不算命中
pub fn find_mountpoint(disk_name: &str, pno: u32) -> std::io::Result<Option<MountView>> {
    let target = partition_dev_no(disk_name, pno)?;
    let own = crate::fsops::read_current_namespace_mounts()?;
    for e in crate::fsops::read_mounts()? {
        if !mounts_target(&e, disk_name, pno, target)? {
            continue;
        }
        let here = own
            .iter()
            .find(|o| o.source == e.source && o.dev_no == e.dev_no)
            .map(|o| o.mount_point.clone());
        return Ok(Some(match here {
            Some(mount_point) => MountView::Here(PathBuf::from(mount_point)),
            None => MountView::Elsewhere { mount_point: e.mount_point, pid: e.pid },
        }));
    }
    Ok(None)
}

/// （盘名, 分区号）对应的设备号（sysfs `<disk>/<part>/dev`），与 mountinfo 字段 3 同源。
/// 非目录条目与没有 `partition` 属性的子目录都不是分区（内核 sysfs-block ABI）；
/// 属性存在却读不出来按环境故障上抛，不报成"没有该分区"
fn partition_dev_no(disk_name: &str, pno: u32) -> io::Result<(u64, u64)> {
    let sysroot = Path::new("/sys/block").join(disk_name);
    for entry in fs::read_dir(&sysroot)? {
        let p = entry?.path();
        if !p.is_dir() {
            continue;
        }
        match fs::metadata(p.join("partition")) {
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e),
        }
        if sysfs_u64(&p.join("partition"))? as u32 != pno {
            continue;
        }
        let s = fs::read_to_string(p.join("dev"))?;
        return parse_dev_attr(&s)
            .ok_or_else(|| io::Error::other(format!("unparsable sysfs dev value in {}", p.display())));
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        format!("partition {pno} not found under {}", sysroot.display()),
    ))
}

/// 这条挂载记录是否指向目标分区。字段 3 与目标分区的 sysfs `dev` 同源，相等即命中，
/// 不需要解析 source。字段 3 报匿名设备号（major 0：btrfs 等多设备文件系统）时与
/// 目标必不相等，只能按 source 的设备身份反解；source 不是绝对路径时它不是设备节点
/// （tmpfs / proc 等伪文件系统），是绝对路径却读不出来时无法证明它不是目标，按
/// "确认不了"上抛
fn mounts_target(
    e: &crate::fsops::MountEntry,
    disk_name: &str,
    pno: u32,
    target: (u64, u64),
) -> io::Result<bool> {
    if e.dev_no == target {
        return Ok(true);
    }
    if e.dev_no.0 != 0 || !e.source.starts_with('/') {
        return Ok(false);
    }
    match fs::metadata(&e.source) {
        Ok(_) => part_in_use(&e.source, disk_name, pno, false),
        Err(err) => Err(io::Error::new(
            err.kind(),
            format!(
                "the mount source {} cannot be resolved ({err}) — whether partition {pno} of \
                 {disk_name} is mounted cannot be confirmed",
                e.source
            ),
        )),
    }
}

/// 目标盘上的分区是否是活动 swap（/proc/swaps；man proc_swaps(5)：首行为表头，
/// 其后每行第一字段为设备路径。反解方式同 find_mountpoint）。
/// 读失败如实上抛——整体读失败与逐条目反解失败（part_in_use）都算：
/// 折叠成"无 swap"会让活动 swap 的锁下复核被一次读故障绕过（fail-open）
pub fn swap_active(disk_name: &str, pno: u32) -> std::io::Result<bool> {
    let s = fs::read_to_string("/proc/swaps")?;
    swap_active_in(&s, disk_name, pno)
}

/// 表内容的判定（与文件读取分开，便于用构造的表做确定性测试）。
/// 逐条目反解失败**不是**"未占用"：那条 swap 的设备可能正是目标分区，反解不出来就没有
/// 证据证明目标空闲。错误点名是哪一条解析不了，避免与"整表读失败"混淆，也不让操作者
/// 以为目标确实在 swap 上
pub(crate) fn swap_active_in(swaps: &str, disk_name: &str, pno: u32) -> std::io::Result<bool> {
    for l in swaps.lines().skip(1) {
        let Some(d) = l.split_whitespace().next() else { continue };
        let hit = part_in_use(d, disk_name, pno, false).map_err(|e| {
            std::io::Error::other(format!(
                "cannot resolve the /proc/swaps entry {d}: {e} — the target's swap state cannot be confirmed"
            ))
        })?;
        if hit {
            return Ok(true);
        }
    }
    Ok(false)
}

/// 命中 = 该设备是 disk_name 上的分区 pno。读失败上抛（fail-closed）：折叠成
/// "不在使用"会让锁下占用复核被一次读故障绕过。
///
/// `allow_missing`：NotFound 是否折叠为"不是目标"（`true`）还是上抛（`false`）。
/// `/proc/swaps` 的首字段恒为设备路径，传 `false`——读不到意味着这条 swap 条目无法
/// 反解，不能当成"未占用"。`true` 只对"source 指不到任何设备节点等于已证明不存在"的
/// 来源成立（伪文件系统名、已被删除的路径不可能是本目标的分区节点），调用方须自行
/// 确认这一前提。
pub(crate) fn part_in_use(dev: &str, disk_name: &str, pno: u32, allow_missing: bool) -> std::io::Result<bool> {
    let meta = match fs::metadata(dev) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && allow_missing => return Ok(false),
        Err(e) => return Err(e),
    };
    if !meta.file_type().is_block_device() {
        return Ok(false);
    }
    let rdev = meta.st_rdev();
    let sysdir = PathBuf::from(format!("/sys/dev/block/{}:{}", libc::major(rdev), libc::minor(rdev)));
    // <part>/partition：内核 sysfs-block ABI 属性，内容 = 分区号；整盘目录无此属性（据此区分二者）
    let part = match sysfs_u64(&sysdir.join("partition")) {
        Ok(p) => p,
        // 整盘设备的 sysfs 目录没有 `partition` 属性：这说明该记录指向的是整盘而不是分区，
        // 按分区号匹配的目标不可能命中它——读不到等于已证明不存在，不是证据缺失
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
    let candidate = disk_name_from_partition(name, |n| Path::new("/sys/block").join(n).exists())
        .ok_or_else(|| std::io::Error::other(format!("cannot derive disk name from partition {name}")))?;
    Ok(candidate == disk_name)
}
