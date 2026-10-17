//! 设备拓扑探测：sysfs 属性、loop 映射与设备键。

use super::*;

/// 从 sysfs 节点定位承载它的整设备节点：`partition` 属性在 ⇒ 当前是分区，
/// 逐级上溯直到非分区节点。层级深度随设备类型变化（loop0p1 与 NVMe 分区的父目录
/// 不同），故按属性判断而不硬编码 `../..`。链断了如实报错，不退化成"就是当前节点"
#[cfg(target_os = "linux")]
pub(crate) fn block_device_at(node: &Path) -> io::Result<PathBuf> {
    let mut cur = node.to_path_buf();
    loop {
        if read_sysfs_attr(&cur.join("partition"))?.is_none() {
            return Ok(cur);
        }
        cur = cur
            .parent()
            .ok_or_else(|| io::Error::other(format!("partition node {} has no parent device", node.display())))?
            .to_path_buf();
    }
}

/// 目标若是 loop 支撑的（含 loop 设备上的分区），取 `loop/backing_file`。
/// 分区节点自身不继承 loop 属性，属性在它上溯到的整设备节点上。
/// 读失败上抛：读不出来不能等同于"不是 loop"
#[cfg(target_os = "linux")]
pub(crate) fn loop_backing_at(node: &Path) -> io::Result<Option<PathBuf>> {
    let owner = block_device_at(node)?;
    Ok(read_sysfs_attr(&owner.join("loop/backing_file"))?
        .filter(|raw| !raw.is_empty())
        .map(PathBuf::from))
}

/// 读 loop 映射事实：backing_file 在 ⇒ 这台设备是 loop，offset / sizelimit 与它
/// 同批引入（Documentation/ABI/testing/sysfs-block-devloop），缺失即读失败如实上抛。
/// sizelimit 0 = 无限制（losetup(8)），按原值记录，比对也按原值
/// 已解析出 sysfs 节点时用它：打开路径按 fd 的 rdev 定位节点，不再二次 stat 路径
#[cfg(target_os = "linux")]
pub(crate) fn loop_mapping_at(node: &Path) -> io::Result<Option<(u64, u64)>> {
    let owner = block_device_at(node)?;
    if read_sysfs_attr(&owner.join("loop/backing_file"))?.is_none() {
        return Ok(None);
    }
    let parse = |name: &str| -> io::Result<u64> {
        let v = read_sysfs_attr(&owner.join(name))?
            .ok_or_else(|| io::Error::other(format!("sysfs {name} attribute is missing")))?;
        v.parse::<u64>().map_err(|e| io::Error::other(format!("unparseable sysfs {name} {v:?}: {e}")))
    };
    Ok(Some((parse("loop/offset")?, parse("loop/sizelimit")?)))
}

/// sysfs 属性 → 去行尾换行的值。三态必须可分：**不存在**（ENOENT）⇒ `Ok(None)`，
/// 读失败（I/O、权限）⇒ `Err`，存在但值为空 ⇒ `Ok(Some(""))`。
/// 把后两者压进 None（`.ok()`），`partition` 判据会把一个**读不出来**的分区节点
/// 当成整设备——分区身份在那一刻静默降级，正是 fail-closed 要排除的退化
#[cfg(target_os = "linux")]
pub(crate) fn read_sysfs_attr(path: &Path) -> io::Result<Option<String>> {
    let v = match std::fs::read_to_string(path) {
        Ok(v) => v,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    Ok(Some(v.trim().to_string()))
}

/// 节点自己声明的设备层身份：按层探测（属性不存在或为空 → 试下一个），
/// 全部不存在则 `Ok(None)`；读失败原样上抛（见 [`read_sysfs_attr`]）
#[cfg(target_os = "linux")]
pub(crate) fn node_device_id(node: &Path) -> io::Result<Option<String>> {
    for attr in DEVICE_ID_ATTRS {
        if let Some(v) = read_sysfs_attr(&node.join(attr))?
            && !v.is_empty()
        {
            return Ok(Some(v));
        }
    }
    Ok(None)
}

/// 设备容量。sysfs 的 `size` 恒以 512 字节扇区计，与设备逻辑扇区大小无关。
/// 块设备节点的 `size` 恒存在：不存在 / 不可解析 / 读失败都是异常 ⇒ `Err`——
/// 容量进过身份键，读失败静默取 0 会让下一次成功读取把身份改名
#[cfg(target_os = "linux")]
pub(crate) fn sysfs_capacity(node: &Path) -> io::Result<u64> {
    let v = read_sysfs_attr(&node.join("size"))?
        .ok_or_else(|| io::Error::other("sysfs size attribute is missing"))?;
    let sectors =
        v.parse::<u64>().map_err(|e| io::Error::other(format!("unparseable sysfs size {v:?}: {e}")))?;
    sectors
        .checked_mul(512)
        .ok_or_else(|| io::Error::other(format!("sysfs size {v:?} overflows u64 bytes")))
}

/// 块设备节点 → sysfs 设备目录。fail-closed：节点 stat 不了或 maj:min 无对应 sysfs 项
/// 即拒绝，绝不退到"调用方 devname-容量"——那个退化会让 `/dev/sdb` 与 `/dev/sdb1`
/// 得到两把锁，journal / checkpoint / lock 落进两个序列化域。
/// 错误信息带设备名，指向该查的东西。`loop/backing_file` 等属性的探测也以此为根
#[cfg(target_os = "linux")]
pub(crate) fn sysfs_node(path: &Path) -> io::Result<PathBuf> {
    use std::os::unix::fs::MetadataExt;
    let deny = |why: String| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "cannot resolve block device topology for {}: {why} — refusing rather than \
                 degrading to a devname-based identity (journal, checkpoint and lock must share \
                 one serialization domain)",
                path.display()
            ),
        )
    };
    let meta = std::fs::metadata(path).map_err(|e| deny(e.to_string()))?;
    sysfs_node_from_rdev(meta.rdev())
}

/// 已打开设备的 sysfs 目录：按 fd 的 rdev 反查 `/sys/dev/block/<maj>:<min>`。
/// 与 [`sysfs_node`] 的区别只在取值来源——打开之后一律走这里，路径在 open 与 stat
/// 之间被 udev 重建时不会把锁与 journal 派生到另一个设备
#[cfg(target_os = "linux")]
pub(crate) fn sysfs_node_from_rdev(rdev: u64) -> io::Result<PathBuf> {
    let sysdev = Path::new("/sys/dev/block").join(format!("{}:{}", libc::major(rdev), libc::minor(rdev)));
    std::fs::canonicalize(&sysdev).map_err(|e| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!("no sysfs node at {} ({e})", sysdev.display()),
        )
    })
}

/// 设备层身份的**两个投影**，共用同一次 sysfs 解析（`node` 即 [`sysfs_node`] 的产物）：
/// - `.0`（目标自身）：分区节点带分区号，整设备就是它自己。旧版 journal / checkpoint
///   按它落点——现场归属曾是持久的、按分区落的
/// - `.1`（所在整设备）：分区节点抹掉分区号。锁用它——独占权针对的是**盘**（分区表属于
///   盘），只有盘粒度才能让"离线以分区节点为目标"与"在线对同一分区"落进同一把锁
///
/// 分区节点自身不携带设备身份（内核只给它 `partition` / `start` / `size`），故取父设备
/// 的身份再附自己的分区号。父设备与分区号都来自 sysfs 拓扑——`/sys/dev/block/<maj>:<min>`
/// 解析出的节点、它的 `partition` 属性、它的父目录——既不解析 `sda1` / `nvme0n1p1` /
/// `dm-0p1` 这类命名，也不自己推算分区号。容量因此不参与分区身份：分区扩容只改变自己
/// 的容量，父设备容量不受影响，撤销窗口不会在操作中途改名
///
/// 拓扑解析不出来时**拒绝**（fail-closed，见 [`block_keys`]）：journal / checkpoint /
/// lock 必须落在同一个序列化域，而"调用方 devname-容量"的退化身份会让
/// `/dev/sdb` 与 `/dev/sdb1` 得到两把锁、两个互不相干的现场命名空间
#[cfg(target_os = "linux")]
pub(crate) fn block_keys(node: &Path, path: &Path) -> io::Result<(String, String)> {
    let deny = |why: String| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "cannot resolve block device topology for {}: {why} — refusing rather than \
                 degrading to a devname-based identity (journal, checkpoint and lock must share \
                 one serialization domain)",
                path.display()
            ),
        )
    };
    // 设备层身份：有 device-id 用它；没有（多数普通盘）退到 **sysfs 节点自身**的名字
    // 与容量——那是拓扑事实，与调用方给的路径无关。两者都读不出来则拒绝，不退化成
    // "名字-0"：容量进过身份键，写死 0 会在下一次成功读取时把身份改名（换序列化域）。
    // 退化键不含绑到具体物理盘的成分：同一槽位先后出现同节点名、同容量的两块盘共用
    // 一个身份域（README 的"设备身份边界"）；要区分它们需要 device-id，本函数不消费
    // 分区内容摘要
    let key_of = |n: &Path| -> io::Result<String> {
        let id = node_device_id(n)
            .map_err(|e| deny(format!("reading device-id attribute of {}: {e}", n.display())))?;
        match id {
            Some(id) => Ok(id),
            None => {
                let cap = sysfs_capacity(n)
                    .map_err(|e| deny(format!("reading {}: {e}", n.join("size").display())))?;
                Ok(format!("{}-{cap}", file_name_lossy(n)))
            }
        }
    };
    // `partition` 是"这是个分区"的判据；**不存在**的节点自己就是整设备（含 kpartx 造出的
    // dm-N 分区，它们是独立的 DM 设备，自带 dm/uuid），此时两个投影重合。
    // 读失败上抛：把"读不出来"折进"不存在"，分区身份会在这一刻静默降级成整设备身份；
    // 值存在而不可解析（含空值）同理——分区属性不存在解析不出数字的情形，拒绝
    let part = read_sysfs_attr(&node.join("partition"))
        .map_err(|e| deny(format!("reading {}: {e}", node.join("partition").display())))?;
    let Some(part) = part else {
        let alone = key_of(node)?;
        return Ok((alone.clone(), alone));
    };
    let n = part
        .parse::<u32>()
        .map_err(|e| deny(format!("partition attribute is {part:?}, not a number ({e})")))?;
    let Some(parent) = node.parent() else {
        return Err(deny("partition node without a parent device".into()));
    };
    let disk = key_of(parent)?;
    Ok((format!("{disk}-p{n}"), disk))
}

pub(crate) fn file_name_lossy(path: &Path) -> String {
    path.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "dev".into())
}

/// 两个伴随文件候选是否指向同一个文件。按"父目录 realpath + 文件名"比对：候选本身
/// 可能还没落盘，但父目录通常在，故这条判据对"将来才出现"的候选同样成立；解析不出来
/// 时退回字面比较。裸文件名按当前目录解析（`canonicalize("")` 不可用）
pub(crate) fn same_candidate_file(a: &Path, b: &Path) -> bool {
    fn settled(p: &Path) -> Option<PathBuf> {
        let name = p.file_name()?;
        let dir = p.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or_else(|| Path::new("."));
        Some(std::fs::canonicalize(dir).ok()?.join(name))
    }
    match (settled(a), settled(b)) {
        (Some(x), Some(y)) => x == y,
        _ => a == b,
    }
}