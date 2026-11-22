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

/// 读 loop 映射事实：backing_file 在 ⇒ 这台设备是 loop，offset / sizelimit 与它同在
/// loop/ 下（Documentation/ABI/testing/sysfs-block-devloop），缺失即读失败如实上抛。
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

/// loop 设备（含 `losetup -P` 展开出的分区）的存储身份：
/// `(backing dev, backing ino, backing rdev, 数据起点字节)`。
/// 同一段存储可以经两个 loop 节点可见（LVM 报作 duplicate PV），此时两条 `/dev`
/// 路径的设备号不同，但 backing 对象与起点相同；整盘复制出的另一份存储共用 PV
/// UUID，backing 对象却不同。故认存储只认这两项，不认 UUID 也不认设备号。
/// 非 loop（没有 loop 映射）⇒ `Ok(None)`：没有证据按"不同存储"处理，调用方据此保持
/// 原有判定；sysfs 记录存在却读不出来 ⇒ `Err`，由调用方决定出口，不把读失败当成
/// "不是 loop"
#[cfg(target_os = "linux")]
pub(crate) fn loop_storage_key(dev: &Path) -> io::Result<Option<(u64, u64, u64, u64)>> {
    let node = sysfs_node(dev)?;
    let owner = block_device_at(&node)?;
    let Some(name) = owner.file_name().map(|n| n.to_string_lossy().into_owned()) else { return Ok(None) };
    let Some(mapping) = crate::fsops::mount::loop_mapping_of_node(&Path::new("/dev").join(&name))? else {
        return Ok(None);
    };
    // 分区节点自身不继承 loop 属性：起点 = loop 数据起点 + 分区在 loop 上的偏移。
    // sysfs `start` 与 [`sysfs_capacity`] 的 `size` 同为 512 字节扇区
    let start = if owner == node {
        0
    } else {
        let Some(raw) = read_sysfs_attr(&node.join("start"))? else {
            return Err(io::Error::other(format!("sysfs start attribute is missing for {}", node.display())));
        };
        let sectors = raw
            .parse::<u64>()
            .map_err(|e| io::Error::other(format!("unparseable sysfs start {raw:?}: {e}")))?;
        sectors
            .checked_mul(512)
            .ok_or_else(|| io::Error::other(format!("sysfs start {raw:?} overflows u64 bytes")))?
    };
    let offset = mapping
        .offset
        .checked_add(start)
        .ok_or_else(|| io::Error::other("loop offset plus partition start overflows u64 bytes"))?;
    Ok(Some((mapping.backing.dev, mapping.backing.ino, mapping.backing.rdev, offset)))
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
    device_id_from(node, AttrView::Placement)
}

/// 属性表的两个消费视图。两者是同一张表的过滤结果，差异只有 `dm/name`
#[cfg(target_os = "linux")]
#[derive(Clone, Copy, PartialEq, Eq)]
enum AttrView {
    /// journal / checkpoint 的落点：含只用于定位的 [`AttrRole::Locator`] 项
    Placement,
    /// 指纹与锁键：只收设备声明的事实
    Identity,
}

/// 按 [`DEVICE_ID_ATTRS`] 取第一条**存在且非空**的属性，返回 `(属性名, 值)`；
/// 三态语义与 [`read_sysfs_attr`] 相同，读失败上抛。所有消费者都经这里选属性，
/// "先看哪个属性、哪些算身份"只有一处答案
#[cfg(target_os = "linux")]
fn first_attr(node: &Path, view: AttrView) -> io::Result<Option<(&'static str, String)>> {
    for (attr, role) in DEVICE_ID_ATTRS {
        if view == AttrView::Identity && *role != AttrRole::Identity {
            continue;
        }
        if let Some(v) = read_sysfs_attr(&node.join(attr))? && !v.is_empty() {
            return Ok(Some((*attr, v)));
        }
    }
    Ok(None)
}

/// 按给定视图探测设备 ID；与 [`first_attr`] 共用同一套"不存在或为空就试下一个"的语义
#[cfg(target_os = "linux")]
fn device_id_from(node: &Path, view: AttrView) -> io::Result<Option<String>> {
    Ok(first_attr(node, view)?.map(|(_, v)| v))
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

/// 进指纹的整盘身份：(整盘容量, 设备 ID 字符串)。两者都取**整设备节点**——`/dev/sdb` 与
/// `/dev/sdb2` 由此得到同一份指纹（现场按盘共享）。ID 只认 [`DEVICE_ID_ATTRS`] 里
/// [`AttrRole::Identity`] 的属性，值为 `属性名=值` 原串（供哈希）；都没有时为 `None`，
/// 容量仍作为辅助证据
#[cfg(target_os = "linux")]
pub(crate) fn disk_identity(node: &Path) -> io::Result<(u64, Option<String>)> {
    let disk = block_device_at(node)?;
    let capacity = sysfs_capacity(&disk)?;
    Ok((capacity, first_attr(&disk, AttrView::Identity)?.map(|(a, v)| format!("{a}={v}"))))
}

/// 拓扑解析失败的拒绝形态：错误信息带设备名，且**不**退到"调用方 devname-容量"——
/// 那个退化会让 `/dev/sdb` 与 `/dev/sdb1` 得到两把锁，journal / checkpoint / lock
/// 落进两个序列化域（fail-closed 的唯一出口）
#[cfg(target_os = "linux")]
fn topology_denied(path: &Path, why: String) -> io::Error {
    io::Error::new(
        io::ErrorKind::NotFound,
        format!(
            "cannot resolve block device topology for {}: {why} — refusing rather than \
             degrading to a devname-based identity (journal, checkpoint and lock must share \
             one serialization domain)",
            path.display()
        ),
    )
}

/// 块设备节点 → sysfs 设备目录。fail-closed：节点 stat 不了或 maj:min 无对应 sysfs 项
/// 即拒绝，绝不退到"调用方 devname-容量"——那个退化会让 `/dev/sdb` 与 `/dev/sdb1`
/// 得到两把锁，journal / checkpoint / lock 落进两个序列化域。
/// 错误信息带设备名，指向该查的东西。`loop/backing_file` 等属性的探测也以此为根
#[cfg(target_os = "linux")]
pub(crate) fn sysfs_node(path: &Path) -> io::Result<PathBuf> {
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::metadata(path).map_err(|e| topology_denied(path, e.to_string()))?;
    sysfs_node_from_rdev(meta.rdev()).map_err(|e| topology_denied(path, e.to_string()))
}

/// sysfs 的挂载点。生产恒为 `/sys`：Documentation/admin-guide/sysfs-rules.rst 规定
/// "sysfs is always at ``/sys``"，应用不得自行查找。测试要拿假树替换整棵 sysfs，
/// 走 [`with_sysfs_root`]——一个只在本进程、只在本线程生效的覆盖，不读环境变量：
/// 身份核对读的外部事实一旦能被运行环境指定，`SYSFS_PATH` 就是身份校验的绕过面。
///
/// 本 crate 里每个 sysfs 路径都从这里派生，挂载点只有一个来源
#[cfg(target_os = "linux")]
pub(crate) fn sysfs_root() -> PathBuf {
    #[cfg(test)]
    if let Some(p) = SYSFS_ROOT_OVERRIDE.with(|c| c.borrow().clone()) {
        return p;
    }
    PathBuf::from("/sys")
}

/// `<sysfs>/block`：整盘节点的父目录
#[cfg(target_os = "linux")]
pub(crate) fn sysfs_block_dir() -> PathBuf {
    sysfs_root().join("block")
}

/// `<sysfs>/dev/block`：按 `<maj>:<min>` 索引的设备节点目录
#[cfg(target_os = "linux")]
pub(crate) fn sysfs_dev_block_dir() -> PathBuf {
    sysfs_root().join("dev/block")
}

/// `<root>/block` 下的整盘节点名，已排序。整盘枚举只有这一份：身份核对
/// （[`block_peers`]）、md 成员、loop 占用都从这里拿清单，差异只在随后的筛选。
/// `root` 生产取 [`sysfs_root`]，测试给假树
#[cfg(target_os = "linux")]
pub(crate) fn block_names(root: &Path) -> io::Result<Vec<String>> {
    let block_dir = root.join("block");
    let entries = std::fs::read_dir(&block_dir)
        .map_err(|e| io::Error::other(format!("cannot enumerate {}: {e}", block_dir.display())))?;
    let mut names = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| {
            io::Error::other(format!("cannot read an entry of {}: {e}", block_dir.display()))
        })?;
        // `/sys/block/<disk>` 是指向设备目录的符号链接；分区住在各自磁盘目录里，
        // 这一层只有整盘
        let kind = entry
            .file_type()
            .map_err(|e| io::Error::other(crate::support::cannot_stat(entry.path().display(), e)))?;
        if !kind.is_dir() && !kind.is_symlink() {
            continue;
        }
        names.push(entry.file_name().to_string_lossy().into_owned());
    }
    names.sort();
    Ok(names)
}

#[cfg(all(test, target_os = "linux"))]
thread_local! {
    static SYSFS_ROOT_OVERRIDE: std::cell::RefCell<Option<PathBuf>> =
        const { std::cell::RefCell::new(None) };
}

/// 在给定的 sysfs 根下执行 `body`，返回后恢复原值。只服务本 crate 的单元测试
#[cfg(all(test, target_os = "linux"))]
pub(crate) fn with_sysfs_root<R>(root: &Path, body: impl FnOnce() -> R) -> R {
    struct Restore(Option<PathBuf>);
    impl Drop for Restore {
        fn drop(&mut self) {
            SYSFS_ROOT_OVERRIDE.with(|c| *c.borrow_mut() = self.0.take());
        }
    }
    let prev = SYSFS_ROOT_OVERRIDE.with(|c| c.replace(Some(root.to_path_buf())));
    let _restore = Restore(prev);
    body()
}

/// 已打开设备的 sysfs 目录：按 fd 的 rdev 反查 `<sysfs>/dev/block/<maj>:<min>`。
/// 与 [`sysfs_node`] 的区别只在取值来源——打开之后一律走这里，路径在 open 与 stat
/// 之间被 udev 重建时不会把锁与 journal 派生到另一个设备
#[cfg(target_os = "linux")]
pub(crate) fn sysfs_node_from_rdev(rdev: u64) -> io::Result<PathBuf> {
    let sysdev = sysfs_dev_block_dir()
        .join(format!("{}:{}", libc::major(rdev), libc::minor(rdev)));
    std::fs::canonicalize(&sysdev).map_err(|e| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!("no sysfs node at {} ({e})", sysdev.display()),
        )
    })
}

/// 设备层身份：**所在整设备**的键。分区节点抹掉分区号后取父设备的键——journal /
/// checkpoint 按这个键派生。独占权针对的是**盘**（分区表属于盘），锁键因此同样取整设备，
/// 但走另一套属性表（[`block_lock_key`]）：`dm/name` 不参与锁键，改名不能把一把锁拆成两把
///
/// 分区节点自身不携带设备身份（内核只给它 `partition` / `start` / `size`），故取父设备的
/// 键。父设备与分区号都来自 sysfs 拓扑——`/sys/dev/block/<maj>:<min>`
/// 解析出的节点、它的 `partition` 属性、它的父目录——既不解析 `sda1` / `nvme0n1p1` /
/// `dm-0p1` 这类命名，也不自己推算分区号。容量因此不参与分区身份：分区扩容只改变自己
/// 的容量，父设备容量不受影响，撤销窗口不会在操作中途改名
///
/// 拓扑解析不出来时**拒绝**（fail-closed）：journal / checkpoint /
/// lock 必须落在同一个序列化域，而"调用方 devname-容量"的退化身份会让
/// `/dev/sdb` 与 `/dev/sdb1` 得到两把锁、两个互不相干的现场命名空间
#[cfg(target_os = "linux")]
pub(crate) fn block_keys(node: &Path, path: &Path) -> io::Result<String> {
    // 设备层身份：有 device-id 用它；没有（多数普通盘）退到 **sysfs 节点自身**的名字
    // 与容量——那是拓扑事实，与调用方给的路径无关。两者都读不出来则拒绝，不退化成
    // "名字-0"：容量进过身份键，写死 0 会在下一次成功读取时把身份改名（换序列化域）。
    // 退化键不含绑到具体物理盘的成分：同一槽位先后出现同节点名、同容量的两块盘共用
    // 一个身份域；要区分它们需要 device-id，本函数不消费
    let key_of = |n: &Path| -> io::Result<String> {
        let id = node_device_id(n)
            .map_err(|e| topology_denied(path, format!("reading device-id attribute of {}: {e}", n.display())))?;
        match id {
            Some(id) => Ok(id),
            None => {
                let cap = sysfs_capacity(n)
                    .map_err(|e| topology_denied(path, format!("reading {}: {e}", n.join("size").display())))?;
                Ok(format!("{}-{cap}", file_name_lossy(n)))
            }
        }
    };
    // `partition` 是"这是个分区"的判据；**不存在**的节点自己就是整设备（含 kpartx 造出的
    // dm-N 分区，它们是独立的 DM 设备，自带 dm/uuid），此时两个投影重合。
    // 读失败上抛：把"读不出来"折进"不存在"，分区身份会在这一刻静默降级成整设备身份；
    // 值存在而不可解析（含空值）同理——分区属性不存在解析不出数字的情形，拒绝
    let part = read_sysfs_attr(&node.join("partition"))
        .map_err(|e| topology_denied(path, format!("reading {}: {e}", node.join("partition").display())))?;
    let Some(part) = part else {
        return key_of(node);
    };
    part.parse::<u32>()
        .map_err(|e| topology_denied(path, format!("partition attribute is {part:?}, not a number ({e})")))?;
    let Some(parent) = node.parent() else {
        return Err(topology_denied(path, "partition node without a parent device".into()));
    };
    key_of(parent)
}

/// 独占锁的落点键：整设备口径，取 [`DEVICE_ID_ATTRS`] 的 [`AttrView::Identity`] 视图。
/// 与 journal / checkpoint 的盘键（[`node_device_id`] 的 `Placement` 视图）取自同一张表，
/// 差别只在是否收 `dm/name`。两者各成一个类型：都是 `String` 时把其中一个当另一个传进去
/// 照样编译
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LockKey(String);

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
impl LockKey {
    pub(crate) fn new(raw: String) -> Self {
        Self(raw)
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

/// 锁键：整设备的身份口径，与指纹同源（[`DEVICE_ID_ATTRS`] 的 `Identity` 视图）。
/// `dm/name` 不参与——
/// 映射名由用户指定、改名即变，用它派生锁会让同一台设备在改名前后各落一把锁，
/// 两个进程因此互不相见。有 `dm/uuid` / `md/uuid` / wwid / serial 取其一；
/// 都没有时退到 sysfs 节点名 + 容量（节点名由内核分配，`dm-0` 不随 `dmsetup rename` 变）
///
/// journal / checkpoint 仍按 [`node_device_id`] 的序列派生：`dm/name` 只影响这两类旧现场
/// 的落点，不参与锁
#[cfg(target_os = "linux")]
pub(crate) fn block_lock_key(node: &Path) -> io::Result<LockKey> {
    let disk = block_device_at(node)?;
    match device_id_from(&disk, AttrView::Identity)? {
        Some(id) => Ok(LockKey::new(id)),
        // 退到 sysfs 节点名 + 容量（DM 上即 `dm-N` + 容量）：锁键与 journal / checkpoint
        // 的盘键不同源是有意的——锁只管「同一台设备」，改名不能把一把锁拆成两把；
        // 那两类现场要标出落点，沿用带 `dm/name` 的序列。两处不是同一条，不是漏改
        None => Ok(LockKey::new(format!("{}-{}", file_name_lossy(&disk), sysfs_capacity(&disk)?))),
    }
}

pub(crate) fn file_name_lossy(path: &Path) -> String {
    path.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "dev".into())
}

/// 记录落点的三态生产者。与只读现场走同一份归类（[`crate::recovery::classify_probe`]）：
/// `Known` 带载体身份原文与分区号；sysfs 读不出、serial 两读不一致 ⇒ `Undecidable`
/// 并带原因；平台确认给不出稳定身份证据（dm / md / 只有 major:minor）落在 `Known`
/// 的 `carrier=None` 上，不另立一态。
///
/// 返回的原文不带 `属性名=` 前缀：记录目录的 identity-hash 按这个文本派生，
/// 加前缀会让盘上已有的记录目录集体失去归属
#[cfg(target_os = "linux")]
pub(crate) fn record_context(
    node: &Path,
) -> crate::domain::Fact<crate::recovery::RecordPlacement> {
    crate::recovery::classify_probe(record_context_inner(node))
}

/// sysfs 拓扑事实的原始采集：读失败包成 [`PlatformError::Io`]，归类交给
/// [`crate::recovery::classify_probe`]，本函数不回答"没有身份"还是"没读到"
#[cfg(target_os = "linux")]
fn record_context_inner(
    node: &Path,
) -> Result<crate::recovery::RecordPlacement, crate::platform::PlatformError> {
    use crate::platform::PlatformError;

    let io = |why: String| PlatformError::Io(io::Error::other(why));
    let disk = block_device_at(node).map_err(|e| {
        io(format!("cannot resolve the whole-disk node of {}: {e}", node.display()))
    })?;
    let partition = read_sysfs_attr(&node.join("partition"))
        .map_err(|e| {
            io(format!("cannot read the partition attribute of {}: {e}", node.display()))
        })?
        .map(|raw| {
            raw.parse::<u32>()
                .map_err(|e| io(format!("partition attribute is {raw:?}, not a number ({e})")))
        })
        .transpose()?;
    if has_sysfs_dir(&disk, "dm", "dm/uuid").map_err(io)?
        || has_sysfs_dir(&disk, "md", "md/uuid").map_err(io)?
    {
        return Ok(crate::recovery::RecordPlacement::new(None, partition));
    }
    Ok(crate::recovery::RecordPlacement::new(
        strong_carrier(&disk).map_err(io)?,
        partition,
    ))
}

/// dm / md 的判据：对应的属性可读，或 sysfs 目录本身存在。两者任一成立都算
/// ——`dm/uuid` 读不出来不能等同于"不是 dm"
#[cfg(target_os = "linux")]
fn has_sysfs_dir(disk: &Path, dir: &str, attr: &str) -> Result<bool, String> {
    match read_sysfs_attr(&disk.join(attr)) {
        Ok(Some(_)) => Ok(true),
        Ok(None) => Ok(disk.join(dir).is_dir()),
        Err(e) => Err(format!("cannot read sysfs {attr} of {}: {e}", disk.display())),
    }
}

/// WWN → EUI → serial：`wwid`、`device/wwid` 取第一个非空原文，再由
/// `device/serial` 兜底。serial 要求连续两次读数一致才当身份。
/// 返回值带属性名：它是声明的命名空间来源，只返回值会让同名同值的两份声明失去区分依据
#[cfg(target_os = "linux")]
fn strong_declaration(disk: &Path) -> Result<Option<(&'static str, String)>, String> {
    for attr in ["wwid", "device/wwid"] {
        match read_sysfs_attr(&disk.join(attr)) {
            Ok(Some(v)) if !v.is_empty() => return Ok(Some((attr, v))),
            Ok(_) => {}
            Err(e) => return Err(format!("cannot read sysfs {attr} of {}: {e}", disk.display())),
        }
    }
    let read_serial = || {
        read_sysfs_attr(&disk.join("device/serial"))
            .map_err(|e| format!("cannot read sysfs device/serial of {}: {e}", disk.display()))
    };
    let first = read_serial()?;
    let second = read_serial()?;
    match (first, second) {
        (Some(a), Some(b)) if a == b && a.is_empty() => Ok(None),
        (Some(a), Some(b)) if a == b => Ok(Some(("device/serial", a))),
        (Some(_), Some(_)) | (Some(_), None) | (None, Some(_)) => Err(
            "the device serial reads differently on repeated reads; it is not a stable identity"
                .to_string(),
        ),
        (None, None) => Ok(None),
    }
}

/// 记录载体使用声明里的值。记录目录的 identity-hash 按这段文本派生，加属性名前缀会让
/// 盘上已有的记录目录集体失去归属
#[cfg(target_os = "linux")]
fn strong_carrier(disk: &Path) -> Result<Option<String>, String> {
    Ok(strong_declaration(disk)?.map(|(_, value)| value))
}

/// 一个整盘节点给出的声明。三态不可压成两态：**读不出**与**没有声明**是两件事，
/// 前者会让"另有设备携带同一声明"无法排除
///
/// 声明与候选集都来自 `/sys/block`：非 Linux 平台没有块设备目标，这一组判据不参与编译
#[cfg(target_os = "linux")]
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ClaimState {
    Declared(crate::platform::Claim),
    /// 属性链读完了，设备没有给出任何声明
    Missing,
    /// 属性读失败，或序列号两次读数不一致
    Unreadable(String),
}

/// `/sys/block` 里的另一块整盘，以及它与当前目标的关系
#[cfg(target_os = "linux")]
#[derive(Clone, Debug)]
pub(crate) struct BlockPeer {
    pub(crate) name: String,
    pub(crate) claim: ClaimState,
    /// 与当前目标同属一个 dm / md 映射（当前目标的整盘名出现在某个节点的 `slaves/` 里）
    pub(crate) related: bool,
    /// 与当前目标是同一存储的另一条路径：存在一个 `dm/uuid` 以 `mpath-` 开头的映射，
    /// 当前目标与它都出现在该映射的 `slaves/` 里
    pub(crate) alias_of_target: bool,
}

/// 设备身份判定的四态。`Mismatch` 是记录与设备不符，`Ambiguous` 是另有设备携带同一声明，
/// `Insufficient` 是没有足够材料证明唯一——三者都拒绝，但拒绝的理由不同
#[cfg(target_os = "linux")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DeviceVerdict {
    Match,
    Mismatch,
    Ambiguous,
    Insufficient,
}

/// 判定结果与拒绝理由。理由只说这一件事：哪一侧没有声明、声明差在哪、谁与谁撞了
#[cfg(target_os = "linux")]
#[derive(Clone, Debug)]
pub(crate) struct DeviceCheck {
    pub(crate) verdict: DeviceVerdict,
    pub(crate) detail: String,
}

#[cfg(target_os = "linux")]
impl DeviceCheck {
    pub(crate) fn is_match(&self) -> bool {
        self.verdict == DeviceVerdict::Match
    }
}

/// 四态判定（冻结表 §7.3）。顺序固定：没有声明 ⇒ 声明不等 ⇒ 另有设备携带同一声明 ⇒
/// 相关设备读不出声明 ⇒ 匹配。`peers` 不含当前目标自身。
///
/// `recorded` 与 `live` 的相等比较只在两边都带有声明时有意义；生产调用点用它比较
/// 同一份声明（问"唯一吗"），记录载荷与设备的比对由 `recovery::SceneIdentity` 负责
#[cfg(target_os = "linux")]
pub(crate) fn compare_claims(
    recorded: Option<&crate::platform::Claim>,
    live: Option<&crate::platform::Claim>,
    peers: &[BlockPeer],
) -> DeviceCheck {
    let (Some(recorded), Some(live)) = (recorded, live) else {
        let detail = match (recorded.is_some(), live.is_some()) {
            (false, false) => "neither the record nor the current device carries an identity declaration",
            (false, true) => "the record carries no identity declaration",
            (true, false) => "the current device carries no identity declaration",
            (true, true) => unreachable!(),
        };
        return DeviceCheck { verdict: DeviceVerdict::Insufficient, detail: detail.to_string() };
    };
    if recorded != live {
        return DeviceCheck {
            verdict: DeviceVerdict::Mismatch,
            detail: format!("recorded {recorded}, current {live}"),
        };
    }
    // 同一存储的另一条路径不算"另一块盘"：mpath 的两条路径按同一份 WWN 落在 sysfs 里，
    // 归并判据见 [`BlockPeer::alias_of_target`]
    if let Some(peer) = peers
        .iter()
        .find(|p| !p.alias_of_target && matches!(&p.claim, ClaimState::Declared(c) if c == live))
    {
        return DeviceCheck {
            verdict: DeviceVerdict::Ambiguous,
            detail: format!("{} also carries the identity declaration {live}", peer.name),
        };
    }
    // 相关设备的声明读不出时，"同一声明只在当前设备上"无法证明。无关设备读失败不进结论
    if let Some(peer) = peers.iter().find(|p| p.related && matches!(p.claim, ClaimState::Unreadable(_))) {
        let why = match &peer.claim {
            ClaimState::Unreadable(why) => why.as_str(),
            _ => unreachable!(),
        };
        return DeviceCheck {
            verdict: DeviceVerdict::Insufficient,
            detail: format!("the related device {} cannot be identified: {why}", peer.name),
        };
    }
    DeviceCheck {
        verdict: DeviceVerdict::Match,
        detail: format!("the identity declaration {live} is unique among the visible whole disks"),
    }
}

/// 一个整盘节点携带的声明。与记录载体走同一条属性链（[`strong_declaration`]）：
/// 判定比较的是声明，两侧必须同源
#[cfg(target_os = "linux")]
fn claim_state_of(node: &Path) -> ClaimState {
    match strong_declaration(node) {
        Ok(Some((attr, value))) => match crate::platform::claim_of(&format!("{attr}={value}")) {
            Some(claim) => ClaimState::Declared(claim),
            // 全零 / 空白是"设备没有给出身份"的写法，不构成声明
            None => ClaimState::Missing,
        },
        Ok(None) => ClaimState::Missing,
        Err(why) => ClaimState::Unreadable(why),
    }
}

/// `slaves/` 的成员。目录不存在 = 这个节点没有从属设备（叶子盘）；读失败上抛，
/// 不把读失败折进"没有从属设备"
#[cfg(target_os = "linux")]
fn slaves_of(node: &Path) -> Result<Vec<String>, String> {
    let dir = node.join("slaves");
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(format!("cannot enumerate {}: {e}", dir.display())),
    };
    let mut names = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| format!("cannot read an entry of {}: {e}", dir.display()))?;
        names.push(entry.file_name().to_string_lossy().into_owned());
    }
    Ok(names)
}

/// `dm/uuid` 是否由 multipath 工具写入。判据是 [`crate::platform::MPATH_UUID_PREFIX`]
/// 一处给出的前缀（来源与大小写口径见该常量）；kpartx 造出的分区映射是另一种形状
/// （`part<N>-mpath-<wwid>`），不以该前缀开头，这里按"不是整盘映射"处理。
/// 读不出 / 不存在 / 其它 DM 类型一律 `false`：归并需要**正面证据**，
/// 没有证据时两个节点仍是两块盘
#[cfg(target_os = "linux")]
fn is_multipath_map(node: &Path) -> bool {
    match read_sysfs_attr(&node.join("dm/uuid")) {
        Ok(Some(uuid)) => uuid.starts_with(crate::platform::MPATH_UUID_PREFIX),
        _ => false,
    }
}

/// `/sys/block` 里除当前目标之外的整盘节点，附带相关关系与 mpath 归并结论。
/// `target` 是**整盘**节点（分区先经 [`block_device_at`] 上溯）；`root` 生产取
/// [`sysfs_root`]，测试给假树
#[cfg(target_os = "linux")]
pub(crate) fn block_peers(root: &Path, target: &Path) -> Result<Vec<BlockPeer>, String> {
    let block_dir = root.join("block");
    let names = block_names(root).map_err(|e| e.to_string())?;
    let target_name = target
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .ok_or_else(|| format!("the sysfs node {} has no name", target.display()))?;
    // 相关关系：存在节点 X，其 `slaves/` 含当前目标的整盘名。X 与它的从属设备同属一个
    // 映射；只认这一种关系，不推断 RAID / LVM / dm-crypt 的语义。
    // `slaves/` 读不出即拒绝：读不出来就无法证明某个节点与目标无关，跳过它等于让
    // 一条可能的克隆路径从判定里消失
    let mut slaves: Vec<(String, Vec<String>)> = Vec::new();
    for name in &names {
        let node = block_dir.join(name);
        let members = slaves_of(&node).map_err(|e| format!("cannot read the slaves of {}: {e}", node.display()))?;
        slaves.push((name.clone(), members));
    }
    let mut related: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut containers: Vec<&(String, Vec<String>)> = Vec::new();
    for entry in &slaves {
        if !entry.1.iter().any(|m| m == &target_name) {
            continue;
        }
        containers.push(entry);
        related.insert(entry.0.clone());
        related.extend(entry.1.iter().cloned());
    }
    // 多路径归并：只有 `mpath-` 映射的 `slaves` 同时含当前目标与该节点时才归并。
    // `dm/uuid` 的稳定性见冻结表 §5 待核实项，它是区分 mpath 与 LVM / CRYPT / RAID 的
    // 唯一可用判据，故这里按它实现，且要求两个成员同时在场
    let multipath_paths: Vec<&str> = containers
        .iter()
        .filter(|(name, _)| is_multipath_map(&block_dir.join(name)))
        .flat_map(|(_, members)| members.iter().map(String::as_str))
        .collect();
    let mut peers = Vec::new();
    for name in names.iter().filter(|n| **n != target_name) {
        let node = block_dir.join(name);
        peers.push(BlockPeer {
            name: name.clone(),
            claim: claim_state_of(&node),
            related: related.contains(name),
            alias_of_target: multipath_paths.iter().any(|p| p == name),
        });
    }
    Ok(peers)
}

/// 当前目标（整盘口径）的声明是否可证明唯一。设备号取自**已打开的句柄**，sysfs 节点按
/// 设备号反查，不按命令行名字取：名字在 open 与本次判定之间可被 udev 重建。
///
/// 问的是"这个声明在可见的整盘里是否只有它一份、且相关设备都读得出"。记录载荷与当前
/// 设备的比对（`Mismatch`）不在这里重复实现：那一条由 `recovery::SceneIdentity` 按完整
/// 现场材料判定，两者各自回答一个问题
#[cfg(target_os = "linux")]
pub(crate) fn check_device(src: &FileSource) -> Result<DeviceCheck, String> {
    let (major, minor, _) = src
        .block_identity
        .as_ref()
        .ok_or_else(|| "the opened target reports no device number".to_string())?;
    let node = sysfs_node_from_rdev(libc::makedev(*major, *minor)).map_err(|e| e.to_string())?;
    let disk = block_device_at(&node).map_err(|e| format!("cannot resolve the whole-disk node: {e}"))?;
    let live = match strong_declaration(&disk)? {
        Some((attr, value)) => crate::platform::claim_of(&format!("{attr}={value}")),
        None => None,
    };
    let peers = block_peers(&sysfs_root(), &disk)?;
    Ok(compare_claims(live.as_ref(), live.as_ref(), &peers))
}

/// 假 sysfs 树的构造件：`<root>/block/<disk>/<attr>`、`<root>/block/<disk>/slaves/<name>`，
/// 以及把设备号接到整盘节点的 `<root>/dev/block/<maj>:<min>`。判定测试与调用链测试
/// 共用这一套，树的形状只有一处
#[cfg(all(test, target_os = "linux"))]
pub(crate) mod fixtures {
    use std::path::{Path, PathBuf};

    pub(crate) fn disk(root: &Path, name: &str) -> PathBuf {
        let node = root.join("block").join(name);
        std::fs::create_dir_all(&node).unwrap();
        node
    }

    pub(crate) fn attr(node: &Path, attr: &str, value: &str) {
        let path = node.join(attr);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, value).unwrap();
    }

    pub(crate) fn slave(container: &Path, name: &str) {
        std::fs::create_dir_all(container.join("slaves").join(name)).unwrap();
    }

    /// `<root>/dev/block/<maj>:<min>` 指向整盘节点：设备号反查靠它
    pub(crate) fn bind_rdev(root: &Path, major: u32, minor: u32, disk: &Path) {
        use std::os::unix::fs::symlink;
        let dir = root.join("dev/block");
        std::fs::create_dir_all(&dir).unwrap();
        symlink(disk, dir.join(format!("{major}:{minor}"))).unwrap();
    }
}

#[cfg(all(test, target_os = "linux"))]
mod identity_tests {
    use super::*;

    fn claim(raw: &str) -> crate::platform::Claim {
        crate::platform::claim_of(raw).expect("the test text must be a declaration")
    }

    fn declared(raw: &str) -> ClaimState {
        ClaimState::Declared(claim(raw))
    }

    fn peer(name: &str, claim: ClaimState) -> BlockPeer {
        BlockPeer { name: name.to_string(), claim, related: false, alias_of_target: false }
    }

    fn related_peer(name: &str, claim: ClaimState) -> BlockPeer {
        BlockPeer { name: name.to_string(), claim, related: true, alias_of_target: false }
    }

    fn alias_peer(name: &str, claim: ClaimState) -> BlockPeer {
        BlockPeer { name: name.to_string(), claim, related: true, alias_of_target: true }
    }

    /// 同一个值经不同属性读到仍是同一个身份：判定只看（命名空间, 值）
    #[test]
    fn the_attribute_that_carried_a_value_does_not_split_it() {
        let live = claim("wwid=naa.5000c500");
        let recorded = claim("device/wwid=naa.5000c500");
        assert_eq!(compare_claims(Some(&recorded), Some(&live), &[]).verdict, DeviceVerdict::Match);
    }

    /// 同序列号的克隆盘：另一块盘带着同一份声明，自动认领必须拒绝
    #[test]
    fn a_cloned_serial_is_ambiguous() {
        let live = claim("device/serial=CLONE-1");
        let peers = [peer("sdb", declared("device/serial=CLONE-1"))];
        let check = compare_claims(Some(&live), Some(&live), &peers);
        assert_eq!(check.verdict, DeviceVerdict::Ambiguous);
        assert!(check.detail.contains("sdb"), "{}", check.detail);
    }

    /// 设备号 / 节点名不参与身份：同名同值之外的一切名字都不影响"还是同一块盘"
    #[test]
    fn a_renamed_node_keeps_the_identity() {
        let live = claim("wwid=naa.5000c500");
        let peers = [peer("sdz", declared("wwid=naa.60000000"))];
        assert_eq!(compare_claims(Some(&live), Some(&live), &peers).verdict, DeviceVerdict::Match);
    }

    /// USB 换口：同一块盘在任一时刻只有一个整盘节点，候选集里没有第二个同声明者
    #[test]
    fn a_device_seen_alone_is_match() {
        let live = claim("device/serial=USB-1");
        assert_eq!(compare_claims(Some(&live), Some(&live), &[]).verdict, DeviceVerdict::Match);
    }

    /// multipath：两条路径同属一个 `mpath-` 映射，归并后不算第二块盘
    #[test]
    fn multipath_paths_of_one_storage_are_not_a_second_disk() {
        let live = claim("wwid=naa.5000c500");
        let peers = [alias_peer("sdb", declared("wwid=naa.5000c500"))];
        assert_eq!(compare_claims(Some(&live), Some(&live), &peers).verdict, DeviceVerdict::Match);
    }

    /// 普通 RAID / LVM / dm-crypt 即使把两块盘放在一个 dm 下，也不构成 multipath 别名
    #[test]
    fn a_non_multipath_mapping_does_not_merge_two_disks() {
        let live = claim("wwid=naa.5000c500");
        let peers = [related_peer("sdb", declared("wwid=naa.5000c500"))];
        assert_eq!(compare_claims(Some(&live), Some(&live), &peers).verdict, DeviceVerdict::Ambiguous);
    }

    /// 相关设备的声明读不出时无法证明唯一：拒绝，而不是把"读不到"当成"没有"
    #[test]
    fn an_unreadable_related_device_is_insufficient() {
        let live = claim("wwid=naa.5000c500");
        let peers = [related_peer("sdb", ClaimState::Unreadable("EIO".into()))];
        let check = compare_claims(Some(&live), Some(&live), &peers);
        assert_eq!(check.verdict, DeviceVerdict::Insufficient);
        assert!(check.detail.contains("sdb"), "{}", check.detail);
    }

    /// 无关设备读失败不影响当前目标
    #[test]
    fn an_unreadable_unrelated_device_does_not_affect_the_target() {
        let live = claim("wwid=naa.5000c500");
        let peers = [peer("sdc", ClaimState::Unreadable("EIO".into()))];
        assert_eq!(compare_claims(Some(&live), Some(&live), &peers).verdict, DeviceVerdict::Match);
    }

    /// 记录侧与设备侧声明不等优先于歧义：先把"不是同一块盘"说清楚
    #[test]
    fn a_mismatch_is_reported_before_ambiguity() {
        let recorded = claim("wwid=naa.5000c500");
        let live = claim("device/serial=OTHER");
        let peers = [peer("sdb", declared("device/serial=OTHER"))];
        assert_eq!(compare_claims(Some(&recorded), Some(&live), &peers).verdict, DeviceVerdict::Mismatch);
    }

    /// 任一侧没有声明就是材料不足：不强猜
    #[test]
    fn a_missing_declaration_is_insufficient() {
        let live = claim("wwid=naa.5000c500");
        assert_eq!(compare_claims(None, Some(&live), &[]).verdict, DeviceVerdict::Insufficient);
        assert_eq!(compare_claims(Some(&live), None, &[]).verdict, DeviceVerdict::Insufficient);
        assert_eq!(compare_claims(None, None, &[]).verdict, DeviceVerdict::Insufficient);
    }

    /// 全零 / 空白的原串不构成声明：两侧都没有声明仍是"材料不足"。
    /// 命名空间前缀与 UUID 连字符不改变全零的判定
    #[test]
    fn an_all_zero_identity_is_not_a_declaration() {
        assert!(crate::platform::claim_of("wwid=0x0000").is_none());
        assert!(crate::platform::claim_of("wwid=naa.0000000000000000").is_none());
        assert!(crate::platform::claim_of("dm/uuid=uuid.00000000-0000-0000-0000-000000000000").is_none());
        assert!(crate::platform::claim_of("device/serial=").is_none());
        assert!(crate::platform::claim_of("wwid=naa.5000c500a1b2c3d4").is_some());
        // `t10.` 是 ASCII 供应商串：值里的 0 是字符而不是十六进制零，前缀不剥
        assert!(crate::platform::claim_of("wwid=t10.0000000000000000").is_some());
        // 定长 ASCII 字段的尾部空格是填充：只剩填充不算身份，填充也不参与比较
        assert!(crate::platform::claim_of("wwid=t10.        ").is_none());
        assert_eq!(
            crate::platform::claim_of("wwid=t10.LINUX   "),
            crate::platform::claim_of("wwid=t10.LINUX")
        );
    }

    /// 裸值按属性名分命名空间：serial 读到的 `S1` 与 wwid 读到的 `S1` 不是同一条声明。
    /// 旧记录里这两者本来就分开落盘，判定不得把它们并成一块盘
    #[test]
    fn bare_values_keep_their_attribute_namespace() {
        let recorded = claim("device/serial=S1");
        let live = claim("wwid=S1");
        assert_eq!(compare_claims(Some(&recorded), Some(&live), &[]).verdict, DeviceVerdict::Mismatch);
    }

    /// serial-only 的盘跨重启续跑：记录侧与现场只有 serial 可读，只要没有第二块同序列号的
    /// 盘，判定就是匹配（强度弱不构成拒绝理由）
    #[test]
    fn a_serial_only_device_can_be_continued() {
        let recorded = claim("device/serial=SN-0001");
        let live = claim("device/serial=SN-0001");
        assert_eq!(compare_claims(Some(&recorded), Some(&live), &[]).verdict, DeviceVerdict::Match);
    }

    #[cfg(target_os = "linux")]
    mod sysfs {
        use super::super::fixtures::{attr, disk, slave};
        use super::*;
        use std::path::Path;

        fn verdict_of(root: &Path, target: &str) -> DeviceVerdict {
            let node = root.join("block").join(target);
            let live = match strong_declaration(&node).unwrap() {
                Some((attr, value)) => crate::platform::claim_of(&format!("{attr}={value}")),
                None => None,
            };
            let peers = block_peers(root, &node).unwrap();
            compare_claims(live.as_ref(), live.as_ref(), &peers).verdict
        }

        /// 同序列号的两块盘：枚举把第二块读成同一声明，判定为歧义
        #[cfg(target_os = "linux")]
        #[test]
        fn cloned_disks_are_enumerated_as_ambiguous() {
            let dir = crate::testdir::test_dir("claim-clone");
            let root = dir.path();
            attr(&disk(root, "sda"), "device/serial", "CLONE-1\n");
            attr(&disk(root, "sdb"), "device/serial", "CLONE-1\n");
            assert_eq!(verdict_of(root, "sda"), DeviceVerdict::Ambiguous);
        }

        /// 换设备号 / 换节点名：同一份声明、没有第二块盘 ⇒ 匹配
        #[cfg(target_os = "linux")]
        #[test]
        fn a_renamed_node_is_still_the_same_device() {
            let dir = crate::testdir::test_dir("claim-rename");
            let root = dir.path();
            attr(&disk(root, "nvme0n1"), "wwid", "naa.5000c500\n");
            assert_eq!(verdict_of(root, "nvme0n1"), DeviceVerdict::Match);
        }

        /// USB 换口：只有一个节点 ⇒ 匹配
        #[cfg(target_os = "linux")]
        #[test]
        fn a_single_node_is_match() {
            let dir = crate::testdir::test_dir("claim-single");
            let root = dir.path();
            attr(&disk(root, "sda"), "device/serial", "USB-1\n");
            assert_eq!(verdict_of(root, "sda"), DeviceVerdict::Match);
        }

        /// multipath：`mpath-` 映射的两条路径归并成一块存储
        #[cfg(target_os = "linux")]
        #[test]
        fn multipath_paths_merge() {
            let dir = crate::testdir::test_dir("claim-mpath");
            let root = dir.path();
            let a = disk(root, "sda");
            let b = disk(root, "sdb");
            attr(&a, "wwid", "naa.5000c500\n");
            attr(&b, "wwid", "naa.5000c500\n");
            let map = disk(root, "dm-0");
            attr(&map, "dm/uuid", "mpath-3600508b400105e210000900000490000\n");
            slave(&map, "sda");
            slave(&map, "sdb");
            assert_eq!(verdict_of(root, "sda"), DeviceVerdict::Match);
        }

        /// 归并只认 multipath 工具写下的 `mpath-<wwid>`：大小写不同、或 kpartx 分区映射的
        /// `part<N>-mpath-<wwid>` 都不是整盘映射，按"没有正面证据"处理 —— 两块盘仍各自成盘
        #[cfg(target_os = "linux")]
        #[test]
        fn only_the_whole_map_uuid_merges() {
            for uuid in ["MPATH-3600508b400105e210000900000490000\n", "part1-mpath-3600508b4\n", "mpath\n"] {
                let dir = crate::testdir::test_dir("claim-mpath-uuid");
                let root = dir.path();
                let a = disk(root, "sda");
                let b = disk(root, "sdb");
                attr(&a, "wwid", "naa.5000c500\n");
                attr(&b, "wwid", "naa.5000c500\n");
                let map = disk(root, "dm-0");
                attr(&map, "dm/uuid", uuid);
                slave(&map, "sda");
                slave(&map, "sdb");
                assert_eq!(verdict_of(root, "sda"), DeviceVerdict::Ambiguous, "{uuid:?}");
            }
        }

        /// LVM 把两条路径放在一个 dm 下，但 `dm/uuid` 不是 `mpath-`：不归并
        #[cfg(target_os = "linux")]
        #[test]
        fn lvm_paths_do_not_merge() {
            let dir = crate::testdir::test_dir("claim-lvm");
            let root = dir.path();
            let a = disk(root, "sda");
            let b = disk(root, "sdb");
            attr(&a, "wwid", "naa.5000c500\n");
            attr(&b, "wwid", "naa.5000c500\n");
            let map = disk(root, "dm-0");
            attr(&map, "dm/uuid", "LVM-abcdefghijklmnop\n");
            slave(&map, "sda");
            slave(&map, "sdb");
            assert_eq!(verdict_of(root, "sda"), DeviceVerdict::Ambiguous);
        }

        /// dm-crypt 把同一块盘的上层映射和下层节点放在一个 dm 下，`dm/uuid` 不是 `mpath-`：
        /// 不构成归并（只有多路径是"同一存储的两条路径"）
        #[cfg(target_os = "linux")]
        #[test]
        fn dm_crypt_does_not_merge() {
            let dir = crate::testdir::test_dir("claim-crypt");
            let root = dir.path();
            let a = disk(root, "sda");
            attr(&a, "wwid", "naa.5000c500\n");
            let map = disk(root, "dm-0");
            attr(&map, "dm/uuid", "CRYPT-LUKS2-a1b2c3d4e5f6a7b8c9d0e1f2a3b4c5d6\n");
            slave(&map, "sda");
            slave(&map, "dm-1");
            assert_eq!(verdict_of(root, "sda"), DeviceVerdict::Match);
        }

        /// RAID 成员不同声明：互不成歧义；`md0` 相关但没有声明，也不改变结论
        #[cfg(target_os = "linux")]
        #[test]
        fn raid_members_are_related_but_distinct() {
            let dir = crate::testdir::test_dir("claim-raid");
            let root = dir.path();
            let a = disk(root, "sda");
            let b = disk(root, "sdb");
            attr(&a, "wwid", "naa.5000c500\n");
            attr(&b, "wwid", "naa.5000c501\n");
            let md = disk(root, "md0");
            slave(&md, "sda");
            slave(&md, "sdb");
            assert_eq!(verdict_of(root, "sda"), DeviceVerdict::Match);
        }

        /// 相关设备的声明读不出：拒绝认领
        #[cfg(target_os = "linux")]
        #[test]
        fn an_unreadable_related_device_refuses_claiming() {
            let dir = crate::testdir::test_dir("claim-related-eio");
            let root = dir.path();
            let a = disk(root, "sda");
            attr(&a, "wwid", "naa.5000c500\n");
            let b = disk(root, "sdb");
            // 目录顶替属性文件：读取报错，而不是"不存在"
            std::fs::create_dir_all(b.join("device/serial")).unwrap();
            let md = disk(root, "md0");
            slave(&md, "sda");
            slave(&md, "sdb");
            assert_eq!(verdict_of(root, "sda"), DeviceVerdict::Insufficient);
        }

        /// `slaves/` 读不出的节点无法证明与目标无关：枚举如实报错，判定侧因此拒绝，
        /// 不把一条可能的克隆路径当成不存在
        #[cfg(target_os = "linux")]
        #[test]
        fn an_unreadable_slaves_directory_refuses_the_enumeration() {
            let dir = crate::testdir::test_dir("claim-slaves-eio");
            let root = dir.path();
            attr(&disk(root, "sda"), "wwid", "naa.5000c500\n");
            let md = disk(root, "md0");
            // 常规文件顶替目录：read_dir 报错，而不是"没有成员"
            std::fs::write(md.join("slaves"), b"").unwrap();
            assert!(block_peers(root, &root.join("block").join("sda")).is_err());
        }

        /// 无关设备读不出：不影响当前目标
        #[cfg(target_os = "linux")]
        #[test]
        fn an_unreadable_unrelated_device_is_ignored() {
            let dir = crate::testdir::test_dir("claim-unrelated-eio");
            let root = dir.path();
            attr(&disk(root, "sda"), "wwid", "naa.5000c500\n");
            let c = disk(root, "sdc");
            std::fs::create_dir_all(c.join("device/serial")).unwrap();
            assert_eq!(verdict_of(root, "sda"), DeviceVerdict::Match);
        }

        /// 空 serial 不是声明：那块盘既不构成歧义，也不是"读不出"
        #[cfg(target_os = "linux")]
        #[test]
        fn a_blank_serial_is_not_a_declaration() {
            let dir = crate::testdir::test_dir("claim-blank");
            let root = dir.path();
            attr(&disk(root, "sda"), "wwid", "naa.5000c500\n");
            attr(&disk(root, "sdb"), "device/serial", "\n");
            assert_eq!(verdict_of(root, "sda"), DeviceVerdict::Match);
            let peers = block_peers(root, &root.join("block").join("sda")).unwrap();
            assert!(matches!(peers[0].claim, ClaimState::Missing), "{:?}", peers[0].claim);
        }

        /// 判定读的是**本次调用时**的树：第二次枚举看见新插进来的同身份盘，
        /// 结论从 Match 变为 Ambiguous。枚举不是快照，只缩小窗口（冻结表 §7.5）
        #[cfg(target_os = "linux")]
        #[test]
        fn the_identity_is_read_at_call_time() {
            let dir = crate::testdir::test_dir("claim-hotplug");
            let root = dir.path();
            attr(&disk(root, "sda"), "wwid", "naa.5000c500\n");
            assert_eq!(verdict_of(root, "sda"), DeviceVerdict::Match);
            attr(&disk(root, "sdb"), "wwid", "naa.5000c500\n");
            assert_eq!(verdict_of(root, "sda"), DeviceVerdict::Ambiguous);
        }
    }
}
