//! 输入源：镜像文件与块设备统一为按偏移读写的字节存储。
//!
//! 本模块实际承载五件事（模块文档即职责表，改职责先改这里）：
//! 1. **输入源**（`FileSource`）：按偏移的 read/write/sync，镜像与块设备同一接口；
//! 2. **身份落点**（`TargetIdentity`）：设备层拓扑 → journal / checkpoint / log / lock
//!    的唯一派生处；
//! 3. **Journal 格式**（`Journal`）：undo 记录的编解码与生命周期（惰性创建、原子性）；
//! 4. **Mutation 语义**（`Mutation` / `RecoveryData`）：undo 的可回滚性判据与屏障；
//! 5. **losetup 用户提示**（`part_dev_hint`）：镜像上定位分区的外部工具提示。
//!
//! `TargetIdentity` 经两个具名构造取得：`resolve_image(path)` 恒成功；
//! `resolve_block(path)` 解析不出设备拓扑即失败——块设备身份拒绝任何退化路径。
//!
//! 身份一律是**盘级**的：分区号属于操作与校验，不进落盘键。loop 设备的身份是
//! backing 文件的 realpath（Linux；`/dev/loop0:2` 与直接以该镜像为目标的操作由此
//! 收敛到同一序列化域），非 loop 块设备仍是 sysfs 拓扑键，镜像路径 canonicalize
//! 后与 loop 侧对齐。两个列表的首项都是写入位置，其后是历史命名候选：只用于发现，
//! 发现后仍走完整校验；不做文件改名迁移。

use std::fs::{File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};

#[cfg(target_os = "linux")]
use std::os::unix::fs::{FileTypeExt, OpenOptionsExt};
#[cfg(target_os = "linux")]
use crate::ioctl;

pub struct FileSource {
    pub(crate) file: File,
    pub path: PathBuf,
    pub sector_size: u64,
    pub size: u64,
    pub is_block: bool,
    /// 目标身份：打开时解析一次，journal / checkpoint / undo 只消费它
    pub identity: TargetIdentity,
    /// undo journal：只记录本工具 write_at 的直接写入
    pub journal: Option<Journal>,
    /// 目标的独占所有权（见 `targetlock`）：写命令持有它，只读打开为 None。
    /// 与这次打开同生命周期，因此不必由调用方各自绑定，也就不会在写盘前被提前丢掉
    pub(crate) ownership: Option<crate::targetlock::TargetLock>,
}

/// 持久状态的默认落点
const DEFAULT_STATE_DIR: &str = "/var/lib/diskedit";

/// 持久状态的落点目录（journal / checkpoint / log）。
///
/// 默认取 `/var/lib/diskedit`：FHS §5.8 把 `/var/lib/<name>` 规定为应用/系统级、跨重启
/// 保留、且不得暴露给普通用户的状态；`$XDG_STATE_HOME` 面向的是用户级 state。本工具的
/// 块设备路径要独占打开整盘并改写分区表，属主机级操作，与后者不是一回事
///
/// `DISKEDIT_STATE_DIR` 只为测试 / 容器 / 打包提供显式 override，不接 `$XDG_STATE_HOME`
/// / `$HOME` 回退链：落点随运行用户与调用环境变化，journal 与 checkpoint 的命名空间就会
/// 漂移，撤销窗口和续传现场随之找不到。同一次未收尾作业的所有调用必须给同一个值
pub(crate) fn state_dir() -> PathBuf {
    match std::env::var_os("DISKEDIT_STATE_DIR") {
        Some(v) if !v.is_empty() => PathBuf::from(v),
        _ => PathBuf::from(DEFAULT_STATE_DIR),
    }
}

/// 目标身份：撤销窗口与续传现场共用的命名空间。**盘级**：分区号属于操作与校验，
/// 不进落盘键，`img:2` 与对应分区节点由此共享同一现场。
/// 镜像以 canonicalize 后的 realpath 为身份（Linux；loop 设备的 backing 侧用同一规则，
/// 两种目标表示由此收敛），realpath 取不到（cleanup 路径上文件已删）退到字面路径；
/// 非 Linux 无 loop、不 canonicalize（Windows 上 canonicalize 产生 `\\?\` verbatim
/// 路径，v1 不引入这套语义）。块设备以设备层拓扑键为身份；loop 设备例外——身份是
/// backing 文件本身，现场落镜像兄弟文件。
///
/// 候选列表的首项是写入位置，其后是历史命名（旧版本写下的那份）：查找按序取首个
/// 有效者、不扫描；两份有效候选同时存在即报歧义，不猜
#[derive(Clone, Debug)]
pub struct TargetIdentity {
    kind: TargetKind,
    /// 用户可见的目标路径：镜像日志沿用它的既有命名（canonicalize 不打断日志连续性），
    /// 块设备只取其文件名作无表时的日志名
    base: PathBuf,
    journal: Vec<PathBuf>,
    checkpoint: Vec<PathBuf>,
    /// 独占锁的落点（见 `targetlock`）。**恒有值**：镜像放在目标旁，块设备由设备身份
    /// 派生到 `state_dir()` 下。类型上没有"没有锁落点"这一状态——取不到锁就是拒绝，
    /// 不存在无锁继续跑的路径
    lock: PathBuf,
    /// 旧版按块设备语义（kind == Block）写下的 GUID 命名 checkpoint 是否可能存在。
    /// loop 归一后本身份已是 Image kind，但历史落点按 Block 规则生成，枚举时需补列
    legacy_guid_checkpoints: bool,
}

/// 只用于区分历史命名约定：块设备另有 GUID / devname 两份历史落点，镜像没有
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TargetKind {
    Image,
    Block,
}

/// 设备层持久 ID 的探测顺序：设备自己声明的身份优先，读不到就退到下一层。
/// 内核并不保证这些属性在所有块设备类型上都存在（ram、无 serial 的 virtio-blk 等），
/// 故本层只回答"最强的可用身份"，链尾恒有 devname + 容量兜底。
///
/// `dm/name` 是 DM 自己的退路（映射名，改名即变），不是全局物理身份，故只排在
/// `dm/uuid` 之后；`wwid` 及其后的条目才是跨设备类型通用的那几层。
/// `loop/backing_file` 不在列：loop 设备在进入本链之前就走专用的 backing 身份解析
/// （见 [`TargetIdentity::resolve_block`]），不再以属性值充当设备 ID
#[cfg(target_os = "linux")]
const DEVICE_ID_ATTRS: &[&str] = &["dm/uuid", "dm/name", "md/uuid", "wwid", "device/wwid", "device/serial"];

/// sysfs 属性 → 去行尾换行的值。三态必须可分：**不存在**（ENOENT）⇒ `Ok(None)`，
/// 读失败（I/O、权限）⇒ `Err`，存在但值为空 ⇒ `Ok(Some(""))`。
/// 把后两者压进 None（`.ok()`），`partition` 判据会把一个**读不出来**的分区节点
/// 当成整设备——分区身份在那一刻静默降级，正是 fail-closed 要排除的退化
#[cfg(target_os = "linux")]
fn read_sysfs_attr(path: &Path) -> io::Result<Option<String>> {
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
fn node_device_id(node: &Path) -> io::Result<Option<String>> {
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
fn sysfs_capacity(node: &Path) -> io::Result<u64> {
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
fn sysfs_node(path: &Path) -> io::Result<PathBuf> {
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
    let sysdev = format!("/sys/dev/block/{}:{}", libc::major(meta.rdev()), libc::minor(meta.rdev()));
    std::fs::canonicalize(&sysdev).map_err(|e| deny(format!("no sysfs node at {sysdev} ({e})")))
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
fn block_keys(node: &Path, path: &Path) -> io::Result<(String, String)> {
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
    // "名字-0"：容量进过身份键，写死 0 会在下一次成功读取时把身份改名（换序列化域）
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

fn file_name_lossy(path: &Path) -> String {
    path.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "dev".into())
}

/// 在目标路径后追加后缀（不适配扩展名，只做串接）：`/a/b.img` + `.diskedit.log` ⇒ `/a/b.img.diskedit.log`，
/// 与用户可见的目标名保持一一对应
pub(crate) fn suffix_path(base: &Path, suffix: &str) -> PathBuf {
    let mut p = base.to_path_buf().into_os_string();
    p.push(suffix);
    PathBuf::from(p)
}

/// 16 字节 Disk GUID → 大写无连字符十六进制（历史落点用的就是这种写法）
fn guid_hex(g: &[u8; 16]) -> String {
    g.iter().map(|b| format!("{b:02X}")).collect()
}

/// 身份键 → 文件名安全的 token：保留 ASCII 字母数字与 `.` `-` `_`，其余（含路径分隔符）
/// 换成 `_` 并截断到 32 字符，末尾附值的 CRC32——身份可能是 loop 的 backing 路径，
/// 原样落盘会带分隔符、可能超长，而截断与替换会令两个不同身份撞同一个名字
#[cfg_attr(not(target_os = "linux"), allow(dead_code))] // 块设备身份链只在 Linux 的生产路径上使用
fn key_token(value: &str) -> String {
    let mut token: String = value
        .chars()
        .take(32)
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') { c } else { '_' })
        .collect();
    token.push_str(&format!("-{:08x}", crate::table::crc32(value.as_bytes())));
    token
}

impl TargetIdentity {
    /// 镜像身份。canonical = 身份主体（journal / checkpoint / lock 落它旁边）；
    /// literal = 用户给的路径（base，日志命名沿用既有约定）。两者不同（路径含符号链接
    /// 或非规范化成分）时字面路径的兄弟文件降为历史候选——只用于发现，发现后照走
    /// 完整校验；两份候选同时有效即歧义拒绝，不猜
    fn image_at(canonical: &Path, literal: &Path) -> Self {
        let mut journal = vec![suffix_path(canonical, ".diskedit.journal")];
        let mut checkpoint = vec![suffix_path(canonical, ".diskedit.ckpt")];
        if literal != canonical {
            journal.push(suffix_path(literal, ".diskedit.journal"));
            checkpoint.push(suffix_path(literal, ".diskedit.ckpt"));
        }
        Self {
            kind: TargetKind::Image,
            base: literal.to_path_buf(),
            journal,
            checkpoint,
            lock: suffix_path(canonical, ".diskedit.lock"),
            legacy_guid_checkpoints: false,
        }
    }

    /// 镜像身份：Linux 上取 realpath（文件不存在——cleanup 路径的常态之一——退字面
    /// 路径），其余平台字面路径
    pub(crate) fn resolve_image(path: &Path) -> Self {
        #[cfg(target_os = "linux")]
        {
            let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
            Self::image_at(&canonical, path)
        }
        #[cfg(not(target_os = "linux"))]
        Self::image_at(path, path)
    }

    /// loop 设备的身份：backing 文件本身。现场落 backing 的兄弟文件——与直接以该
    /// 镜像为目标的操作天然同一序列化域，这正是 loop↔image 的收敛点。
    /// `raw` 来自 sysfs `loop/backing_file`：内核记录的 attach 时刻路径。backing 还在
    /// （写命令的常态）⇒ 以 realpath 为主；已消失（现场比文件活得久）⇒ 退到原始路径串
    /// 的兄弟文件——losetup 以绝对路径 attach 时那正是现场所在地；路径含符号链接时
    /// 由此不可达，这是 v1 文档化的恢复前提（backing 须留在原 canonical path）。
    /// 旧版身份（state_dir 下按 backing 串 token 与 devname 命名的落点）列为历史候选
    #[cfg(target_os = "linux")]
    fn loop_backed(dev_path: &Path, raw: PathBuf) -> Self {
        let canonical = std::fs::canonicalize(&raw).unwrap_or_else(|_| raw.clone());
        let mut id = Self::image_at(&canonical, &raw);
        let tok = key_token(&raw.to_string_lossy());
        id.journal.push(state_dir().join(format!("{tok}.diskedit.journal")));
        id.checkpoint.push(state_dir().join(format!("{tok}.diskedit.ckpt")));
        // 更旧的历史命名：块设备 journal 曾以 devname 命名
        id.journal.push(state_dir().join(format!("{}.diskedit.journal", file_name_lossy(dev_path))));
        id.legacy_guid_checkpoints = true;
        id
    }

    /// 块设备身份：打开目标时解析一次，设备层拓扑取自 sysfs。
    /// **fail-closed**：拓扑解析不出来即 `Err`（错误信息带设备名），调用方拒绝——
    /// 绝不退到"devname-容量"的调用方身份，journal / checkpoint / lock 三者的落点
    /// 必须落在同一个序列化域。
    ///
    /// loop 设备走专用解析：`loop/backing_file` 属性在 ⇒ 身份是 backing 文件（见
    /// [`Self::loop_backed`]），不再以属性值充当设备 ID。读失败原样上抛；属性不存在
    /// （含空值）按非 loop 走拓扑链
    ///
    /// 不接收容量：容量不参与身份（分区扩容会改变自己的容量，父设备容量不受影响，
    /// 撤销窗口不该在操作中途改名）
    #[cfg(target_os = "linux")]
    pub(crate) fn resolve_block(path: &Path) -> Result<Self, String> {
        let node = sysfs_node(path).map_err(|e| e.to_string())?;
        match read_sysfs_attr(&node.join("loop/backing_file")).map_err(|e| e.to_string())? {
            Some(raw) if !raw.is_empty() => return Ok(Self::loop_backed(path, PathBuf::from(raw))),
            _ => {}
        }
        let (self_key, disk_key) = block_keys(&node, path).map_err(|e| e.to_string())?;
        Ok(Self::from_block_keys(path, self_key, disk_key))
    }

    /// 非 Linux 平台没有 sysfs，块设备判定恒 false，本入口不会到达；保留防御性实现
    /// 使两侧签名一致
    #[cfg(not(target_os = "linux"))]
    #[allow(dead_code)] // 非 Linux 没有块设备的生产打开路径，此处只服务命名与锁落点断言
    pub(crate) fn resolve_block(path: &Path) -> Result<Self, String> {
        let k = file_name_lossy(path);
        Ok(Self::from_block_keys(path, k.clone(), k))
    }

    /// 非 loop 块设备的身份构造。**盘级**：journal / checkpoint / lock 全部按所在整设备
    /// 派生——分区号属于操作，不进落盘键，`/dev/sdb:2`（整盘节点 + 后缀）与
    /// `/dev/sdb2`（分区节点）由此共享同一现场，旧版按分区落的那份降为历史候选
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))] // 非 Linux 只经 resolve_block 的测试到达
    fn from_block_keys(path: &Path, self_key: String, disk_key: String) -> Self {
        let stable = key_token(&disk_key);
        let dir = state_dir();
        let mut journal = vec![dir.join(format!("{stable}.diskedit.journal"))];
        let mut checkpoint = vec![dir.join(format!("{stable}.diskedit.ckpt"))];
        if self_key != disk_key {
            // 历史命名：旧版现场按分区落（self_key 含分区号）
            let self_tok = key_token(&self_key);
            journal.push(dir.join(format!("{self_tok}.diskedit.journal")));
            checkpoint.push(dir.join(format!("{self_tok}.diskedit.ckpt")));
        }
        journal.push(dir.join(format!("{}.diskedit.journal", file_name_lossy(path))));
        Self {
            kind: TargetKind::Block,
            base: path.to_path_buf(),
            journal,
            checkpoint,
            // 锁按**所在整设备**派生，与 journal / checkpoint 的盘级键一致：
            // 独占权针对盘（分区表属于盘），于是"离线以分区节点为目标"与"在线对同一
            // 分区"落到同一把锁上。锁文件是纯运行时 artifact（重启自清也无妨），残留
            // 不构成阻挡——判据是"锁取不取得到"，不是"文件在不在"（见 targetlock）
            lock: dir.join(format!("{stable}.diskedit.lock")),
            legacy_guid_checkpoints: true,
        }
    }

    /// 手上只有目标路径时解析身份（撤销窗口在命令收尾时按命令行参数关闭，那时
    /// FileSource 已释放）。块设备判定与容量都要重取一次，且必须与打开目标时算出
    /// 同一个身份——收尾删的是这里给出的名字，差一个字节就会漏删。取不到容量即返回
    /// None，由调用方告警：宁可留下 journal，也不能删错别人的
    pub(crate) fn resolve_path(path: &Path) -> Option<Self> {
        #[cfg(target_os = "linux")]
        {
            // stat 不了的目标身份不可知：按镜像身份收尾会删错候选。宁可漏删（None ⇒
            // 调用方告警并留下 journal），也不按错身份删
            match std::fs::metadata(path).map(|m| m.file_type().is_block_device()) {
                Ok(true) => return Self::resolve_block(path).ok(),
                Ok(false) => {}
                Err(_) => return None,
            }
        }
        Some(Self::resolve_image(path))
    }

    /// 清理类命令（abandon）的身份解析：目标可能已不存在，而现场文件可能比目标活得
    /// 久（用户删了镜像，journal 还在旁边）。路径 stat 不了时按镜像身份回退——镜像
    /// 身份就是给定的路径本身，兄弟候选的枚举不需要目标存在。节点消失的块设备身份
    /// 不可恢复（拓扑派生），其 state_dir 下的现场随之不可达：此处无从区分两者，按
    /// 镜像回退时前者得救、后者空跑——报"nothing to abandon"而不是按错身份删候选。
    /// 拓扑解析得出但解析失败的块设备仍返回 None（同 resolve_path 的保守方向）。
    /// loop 设备的回退内建于 [`Self::loop_backed`]：backing 的 realpath 取不到时退到
    /// sysfs 记录的原始路径串与 state_dir 历史落点，不因 backing 消失而失去现场
    pub(crate) fn resolve_for_cleanup(path: &Path) -> Option<Self> {
        #[cfg(target_os = "linux")]
        {
            match std::fs::metadata(path).map(|m| m.file_type().is_block_device()) {
                Ok(true) => return Self::resolve_block(path).ok(),
                Ok(false) => {}
                Err(_) => return Some(Self::resolve_image(path)),
            }
        }
        Some(Self::resolve_image(path))
    }

    pub(crate) fn journal_path(&self) -> &Path {
        &self.journal[0]
    }

    /// 独占锁的落点。恒有值：取锁失败即拒绝，不提供"没有锁落点"这种状态
    pub(crate) fn lock_path(&self) -> &Path {
        &self.lock
    }

    pub(crate) fn journal_candidates(&self) -> &[PathBuf] {
        &self.journal
    }

    pub(crate) fn checkpoint_path(&self) -> &Path {
        &self.checkpoint[0]
    }

    /// 日志落点：镜像 = `<目标路径>.diskedit.log`；块设备 = `<state_dir>/<名>.diskedit.log`，
    /// 名取可读的 GPT Disk GUID（与 checkpoint 同源），读不到表时退到 devname。
    ///
    /// 与 journal / checkpoint 的差别只有一处：日志只增、不参与恢复，因此不做候选回退，
    /// 名字也保持既有约定不变——改名会打断已写下日志的连续性。落点只在此处拼装，
    /// 调用方不得自行拼 `state_dir()`
    pub(crate) fn log_path(&self, disk_guid: Option<[u8; 16]>) -> PathBuf {
        if self.kind == TargetKind::Image {
            return suffix_path(&self.base, ".diskedit.log");
        }
        let name = match disk_guid {
            Some(g) => guid_hex(&g),
            // 无 GPT（MBR / 裸盘）：devname 是这类目标上唯一稳定的标识
            None => file_name_lossy(&self.base),
        };
        state_dir().join(format!("{name}.diskedit.log"))
    }

    /// checkpoint 的候选落点。块设备语义下的历史落点以 GPT Disk GUID 命名，而 GUID 只
    /// 在表可读时存在，读不到就没有那一条
    pub(crate) fn checkpoint_candidates(&self, legacy_disk_guid: Option<[u8; 16]>) -> Vec<PathBuf> {
        let mut v = self.checkpoint.clone();
        if self.has_block_legacy_naming()
            && let Some(g) = legacy_disk_guid
        {
            v.push(state_dir().join(format!("{}.ckpt", guid_hex(&g))));
        }
        v
    }

    /// 本身份是否可能带 Block 语义的历史落点（GUID 命名的 checkpoint）。两个消费点：
    /// checkpoint 候选补列，以及补列前打开目标读表（GUID 只在表可读时存在）。
    /// loop 归一后身份已是 Image kind，但其历史落点按 Block 规则生成，同样适用
    pub(crate) fn has_block_legacy_naming(&self) -> bool {
        self.kind == TargetKind::Block || self.legacy_guid_checkpoints
    }
}

/// 尽力创建目录：失败不在此处报错——真正的失败会在随后打开文件时以更具体的
/// 错误（完整路径 + 原因）暴露，比这里笼统的 EACCES 更有诊断价值
#[allow(clippy::let_underscore_must_use)] // 有意忽略：失败在打开文件时以更具体错误暴露
pub(crate) fn best_effort_mkdir(dir: &Path) {
    let _ = std::fs::create_dir_all(dir);
}

// 测试期的定点读失败注入（模拟坏扇区），按线程生效。
//
// 回退逻辑的关键情形是"某个位置读不出来、别处正常"——常规文件构造不出这种形态
// （让 file 短于 size 会连盘尾一并读失败），故留一个最小的注入点。
// 测试各自跑在自己的线程上，用 RAII 守卫设置与复位
#[cfg(test)]
thread_local! {
    static READ_FAULT: std::cell::Cell<Option<u64>> = const { std::cell::Cell::new(None) };
}

/// 注入守卫：命中该偏移的 `read_at` 报错，守卫析构即复位
#[cfg(test)]
pub(crate) struct ReadFaultGuard;

#[cfg(test)]
impl ReadFaultGuard {
    pub(crate) fn at(off: u64) -> Self {
        READ_FAULT.with(|f| f.set(Some(off)));
        Self
    }
}

#[cfg(test)]
impl Drop for ReadFaultGuard {
    fn drop(&mut self) {
        READ_FAULT.with(|f| f.set(None));
    }
}

impl FileSource {
    /// `sector_size_override`：镜像默认 512（镜像不携带扇区信息），块设备经 BLKSSZGET 查询并忽略覆盖值。
    pub fn open(path: &Path, sector_size_override: Option<u64>) -> io::Result<Self> {
        // 块设备判定：块设备文件（其 metadata().len() 恒 0，容量需 ioctl 取）
        #[cfg(target_os = "linux")]
        {
            let meta = std::fs::metadata(path)?;
            if meta.file_type().is_block_device() {
                // 读写 + O_EXCL（man open(2)）：设备被 claim 时内核拒绝打开，
                // 分区被挂载或占用会连同整盘一起被 claim
                let file = OpenOptions::new()
                    .read(true)
                    .write(true)
                    .custom_flags(libc::O_EXCL)
                    .open(path)?;
                let size = ioctl::blkgetsize64(&file)?;
                let sector_size = ioctl::blksszget(&file)? as u64;
                // 块设备身份解析失败（fail-closed）⇒ 打开失败：错误信息带设备名
                let identity = TargetIdentity::resolve_block(path).map_err(io::Error::other)?;
                return Ok(FileSource {
                    identity,
                    file,
                    path: path.to_path_buf(),
                    sector_size,
                    size,
                    is_block: true,
                    journal: None,
                    ownership: None,
                });
            }
        }
        Self::open_image(path, sector_size_override, true)
    }

    /// 镜像的只读打开：info/plan/resize 的只读阶段经此打开——调用面已核清
    /// 无一处经该 FileSource 写盘，真只读让只读文件、只读介质或被他进程独占的镜像
    /// 同样可用，且"只读命令不申请写权限"与块设备侧同一口径
    pub(crate) fn open_read_only_image(path: &Path, sector_size_override: Option<u64>) -> io::Result<Self> {
        Self::open_image(path, sector_size_override, false)
    }

    /// 镜像打开的唯一实现：可写性是显式参数，读写与只读两个入口不各自拼一份
    fn open_image(path: &Path, sector_size_override: Option<u64>, writable: bool) -> io::Result<Self> {
        let meta = std::fs::metadata(path)?;
        let mut opts = OpenOptions::new();
        opts.read(true);
        if writable {
            opts.write(true);
        }
        let file = opts.open(path)?;
        let size = meta.len();
        let sector_size = sector_size_override.unwrap_or(512);
        // 镜像扇区大小须为 2 的幂且落在 512..=65536；这是本工具的自定约束，非规范要求
        if !(512..=65536).contains(&sector_size) || !sector_size.is_power_of_two() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("invalid sector size {sector_size}"),
            ));
        }
        Ok(FileSource {
            identity: TargetIdentity::resolve_image(path),
            file,
            path: path.to_path_buf(),
            sector_size,
            size,
            is_block: false,
            journal: None,
            ownership: None,
        })
    }

    /// 只读打开块设备（在线路径识别 FS 用：读写 + O_EXCL 在设备被 claim 时会失败）。
    /// 身份解析失败（fail-closed）⇒ 打开失败
    #[cfg(target_os = "linux")]
    pub(crate) fn open_read_only(path: &Path) -> io::Result<Self> {
        let file = OpenOptions::new().read(true).open(path)?;
        let size = ioctl::blkgetsize64(&file)?;
        let sector_size = ioctl::blksszget(&file)? as u64;
        Ok(FileSource {
            identity: TargetIdentity::resolve_block(path).map_err(io::Error::other)?,
            file,
            path: path.to_path_buf(),
            sector_size,
            size,
            is_block: true,
            journal: None,
            ownership: None,
        })
    }

    /// 非 Linux 平台的占位实现：块设备判定与 ioctl 都不可用，只读路径按普通文件打开。
    /// 该平台上身份解析恒为镜像（`is_block()` 恒 false），此函数不会被块设备路径走到
    #[cfg(not(target_os = "linux"))]
    pub(crate) fn open_read_only(path: &Path) -> io::Result<Self> {
        let file = File::open(path)?;
        let size = file.metadata()?.len();
        Ok(FileSource {
            identity: TargetIdentity::resolve_image(path),
            file,
            path: path.to_path_buf(),
            sector_size: 512,
            size,
            is_block: false,
            journal: None,
            ownership: None,
        })
    }

    /// pread 语义（不移动文件游标，&self 可调用）；不足 buf 长度报错
    pub fn read_at(&self, off: u64, buf: &mut [u8]) -> io::Result<()> {
        #[cfg(test)]
        if READ_FAULT.with(|f| f.get()) == Some(off) {
            return Err(io::Error::other("injected read fault"));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::FileExt;
            self.file.read_exact_at(buf, off)
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::FileExt;
            let mut done = 0usize;
            while done < buf.len() {
                let at = off.checked_add(done as u64).ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidInput, "read offset overflows")
                })?;
                let n = self.file.seek_read(&mut buf[done..], at)?;
                if n == 0 {
                    return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "short read"));
                }
                done += n;
            }
            Ok(())
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = (off, buf);
            Err(io::Error::new(io::ErrorKind::Unsupported, "no read_at on this platform"))
        }
    }

    /// 元数据写入：被覆盖字节的原文先入 undo journal 再写
    pub fn write_at(&mut self, off: u64, buf: &[u8]) -> io::Result<()> {
        if self.journal.is_some() {
            let mut orig = vec![0u8; buf.len()];
            let within = self.size.saturating_sub(off).min(buf.len() as u64) as usize;
            if within > 0 {
                self.read_at(off, &mut orig[..within])?;
            } // EOF 之外视为零，其余保持 0 填充
              // journal 先行落盘：写入发生前，被覆盖字节的原文必须已持久化
            if let Some(journal) = self.journal.as_mut() {
                journal.record(off, &orig)?;
            }
        }
        self.write_raw(off, buf)
    }

    /// 数据块写入（搬移的 chunk 拷贝）：不入 undo journal。
    /// 搬移的设计是"前向恢复、无回滚"（见 movepart 模块注释），undo 不消费数据字节，
    /// 记录它们只会产生与搬移量等大的 journal。数据一致性由 ckpt + 幂等重做保证
    pub fn write_data_at(&mut self, off: u64, buf: &[u8]) -> io::Result<()> {
        self.write_raw(off, buf)
    }

    fn write_raw(&mut self, off: u64, buf: &[u8]) -> io::Result<()> {
        use std::io::{Seek, SeekFrom, Write};
        self.file.seek(SeekFrom::Start(off))?;
        self.file.write_all(buf)
    }

    /// 记下"本次操作含有不可回滚的写入"，undo 见到它即拒绝回滚。
    ///
    /// 判据是**可逆性**，不是"是不是外部进程"：数据搬移的字节（`write_data_at`）是本进程
    /// 写的，但按设计故意不入 journal；外部 FS 工具的写入（resize2fs/mkswap/mkfs…）
    /// 与内核侧的分区表写入同样无法回滚。凡"回滚表项会与盘上内容自相矛盾"的写入都属于这一类，
    /// 它们共同落一条屏障，屏障上的 mutation 说明是哪一类越过了这条线
    pub fn mark_non_reversible(&mut self) -> io::Result<()> {
        if let Some(journal) = self.journal.as_mut() {
            journal.barrier()?;
        }
        Ok(())
    }

    /// 声明"接下来这类改动是什么"，作用于随后的 pre-image 与屏障记录。
    /// 语义由知道自己在做什么的那一层给出——journal 只负责记，不负责猜
    pub fn set_mutation(&mut self, m: Mutation) {
        if let Some(journal) = self.journal.as_mut() {
            journal.set_mutation(m);
        }
    }

    /// 每步落盘。容量有变化的场景用 sync_all，纯数据覆盖用 sync_data。
    pub fn sync_all(&self) -> io::Result<()> {
        self.file.sync_all()
    }

    pub fn sync_data(&self) -> io::Result<()> {
        self.file.sync_data()
    }
}

/// 解析 `<target>[:N]` → (路径, 分区号 Option)。
/// 本层只判定合法性、不决定进程怎么退出：数字段溢出 u32 静默当整盘目标会误伤数据，
/// 故作为错误上抛，由调用方（main 的参数层）转成退出码。
/// `:0` 同样拒绝——分区号是 1-based，静默折叠成"整盘"会把一次针对具体分区的操作
/// 放大成对整盘的表操作
pub fn parse_target(s: &str) -> Result<(String, Option<u32>), &'static str> {
    match s.rfind(':') {
        // 尾冒号（`img:`）多半是分区号漏写的笔误：按整路径打开只会报"文件不存在"，
        // 不如当场说清缺的是什么
        Some(pos) if pos + 1 == s.len() => Err("missing partition number after ':'"),
        Some(pos) if s[pos + 1..].chars().all(|c| c.is_ascii_digit()) => {
            let n: u32 = s[pos + 1..].parse().map_err(|_| "partition number out of range")?;
            if n == 0 {
                return Err("partition number is 1-based (:0 is not a partition)");
            }
            Ok((s[..pos].to_string(), Some(n)))
        }
        _ => Ok((s.to_string(), None)),
    }
}

/// 块设备分区节点命名（util-linux 与内核通用惯例）：盘名以数字结尾时分区号加 "p"。
/// `part_dev_hint`（镜像 losetup 提示）与在线/离线 LVM 链的分区节点路径共用此规则。
/// `base` 是**盘名**（如 /dev/sda、nvme0n1），不含分区号
pub(crate) fn part_node_name(base: &str, part: u32) -> String {
    let sep = if base.chars().last().is_some_and(|c| c.is_ascii_digit()) { "p" } else { "" };
    format!("{base}{sep}{part}")
}

/// 补救提示里的设备标识：块设备给出可直接粘贴的分区节点（/dev/sdb→/dev/sdb1、
/// /dev/nvme0n1→/dev/nvme0n1p1，末尾数字需 p 分隔，命名规则见 `part_node_name`）；
/// 镜像文件没有分区节点，给出字节偏移供 `losetup -o` 使用
pub(crate) fn part_dev_hint(src: &FileSource, part: u32, offset_bytes: u64) -> String {
    if src.is_block {
        part_node_name(&src.path.to_string_lossy(), part)
    } else {
        format!("<part {part} of {} at offset {offset_bytes} — e.g. losetup -o {offset_bytes}>", src.path.display())
    }
}

/// 删除持久化元数据（journal / checkpoint）失败：残留不是"无害垃圾"——
/// journal 残留会让下次 undo 重复回滚已回滚的内容，checkpoint 残留会让下次运行
/// 被旧计划阻塞或误续传。属用户必须知道的状态，故告警（不改变本次的成功结论）。
/// 文件本就不存在不算失败：删除的后置条件是"不残留"，此时已然成立
pub(crate) fn warn_if_remove_failed(path: &std::path::Path) {
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => eprintln!(
            "warning: cannot remove {}: {e} — a later run may re-apply this journal or be blocked by a stale checkpoint",
            path.display()
        ),
    }
}

/// 尽力清理临时资源（临时挂载点等）：失败只影响资源占用，且调用点通常已有主错误/告警
#[allow(clippy::let_underscore_must_use)]
pub(crate) fn best_effort_rmdir(dir: &std::path::Path) {
    let _ = std::fs::remove_dir(dir);
}

/// 尽力而为的目录 fsync：把新建/重命名后的目录项推入持久存储。
/// 失败**不**影响安全性——只影响"崩溃后还能看到多新的元数据"：
/// - ckpt 目录项丢失 → 恢复点回退到上一个 checkpoint（重做已完成部分，安全，不会超前）
/// - journal 目录项丢失 → 该 journal 可能整份消失（undo 能力丢失，前向恢复不受影响）
///
/// 两者都不会产生"超前于数据"的持久化状态，故此处有意忽略失败
#[cfg(unix)]
#[allow(clippy::let_underscore_must_use)] // 有意忽略：失败只使元数据回退，见上
fn best_effort_dir_fsync(path: &std::path::Path) {
    if let Some(dir) = path.parent()
        && let Ok(d) = std::fs::File::open(dir)
    {
        let _ = d.sync_all();
    }
}

#[cfg(not(unix))]
fn best_effort_dir_fsync(_path: &std::path::Path) {}

/// 一次 durable mutation 的**语义**：用户看到的"做了什么"，也是 undo policy 的输入。
///
/// 语义与恢复数据分开表达是刻意的：字节 diff 反推不出业务语义（"这两段字节变了"不等于
/// "分区被搬走了"），而业务语义也替代不了 pre-image（想回滚就得有被覆盖的原文）。
/// 两者各记一份在**同一条记录**里，不设第二套 history
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mutation {
    /// 分区表/条目字节的改写：new / add / del / set / create / resize-part / 搬移的表项部分
    PartitionTable,
    /// 分区数据块被搬移
    RelocatePartition { partition: u32, from_lba: u64, to_lba: u64 },
    /// 分区数据块被复制到别处
    CopyPartition { partition: u32, from_lba: u64, to_lba: u64 },
    /// 外部 FS 工具写进了分区内容：resize2fs / xfs_growfs / mkswap / e2fsck / pvresize…
    ExternalFsTool,
    /// 在分区上创建了文件系统
    Mkfs,
}

impl Mutation {
    /// 人读的一句话。undo 的拒绝理由与 abandon 的清单都用它，避免两处各自措辞
    pub fn describe(&self) -> String {
        match self {
            Mutation::PartitionTable => "the partition table was rewritten".to_string(),
            Mutation::RelocatePartition { partition, from_lba, to_lba } => {
                format!("partition {partition} data was moved from LBA {from_lba} to {to_lba}")
            }
            Mutation::CopyPartition { partition, from_lba, to_lba } => {
                format!("partition {partition} data was copied from LBA {from_lba} to {to_lba}")
            }
            Mutation::ExternalFsTool => "an external filesystem tool wrote to the partition".to_string(),
            Mutation::Mkfs => "a filesystem was created on the partition".to_string(),
        }
    }
}

/// 一条记录里的**恢复依据**：undo 真正依赖的那部分
pub enum RecoveryData {
    /// 被覆盖字节的原文。回放它即回到这次写入之前
    PreImage { off: u64, bytes: Vec<u8> },
    /// 从这里起该 mutation 越过了不可回滚点：只回滚它之前的记录会得到"表与盘上内容
    /// 自相矛盾"的布局，故 undo 见到即整体拒绝
    Barrier,
}

/// 一条 durable 记录：发生了什么 + 怎么恢复
pub struct JournalRecord {
    pub mutation: Mutation,
    pub recovery: RecoveryData,
}

/// 记录头 `[len u32][crc32 u32]` 的字节数。`len` 不计入 CRC（见 [`Journal::read_entries`]）
const REC_HDR: usize = 8;

fn journal_decode_err(what: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, format!("journal record is malformed: {what}"))
}

impl Mutation {
    fn tag(self) -> u8 {
        match self {
            Mutation::PartitionTable => 0,
            Mutation::RelocatePartition { .. } => 1,
            Mutation::CopyPartition { .. } => 2,
            Mutation::ExternalFsTool => 3,
            Mutation::Mkfs => 4,
        }
    }

    fn write_meta(self, out: &mut Vec<u8>) {
        match self {
            Mutation::RelocatePartition { partition, from_lba, to_lba }
            | Mutation::CopyPartition { partition, from_lba, to_lba } => {
                out.extend_from_slice(&partition.to_le_bytes());
                out.extend_from_slice(&from_lba.to_le_bytes());
                out.extend_from_slice(&to_lba.to_le_bytes());
            }
            _ => {}
        }
    }

    /// 解出 mutation 与它消耗的字节数（调用方据此找到后面的 recovery 段）
    fn read(tag: u8, rest: &[u8]) -> io::Result<(Self, usize)> {
        let reloc = |kind: u8| -> io::Result<(Mutation, usize)> {
            if rest.len() < 20 {
                return Err(journal_decode_err("truncated mutation payload"));
            }
            let partition = u32::from_le_bytes(rest[0..4].try_into().unwrap());
            let from_lba = u64::from_le_bytes(rest[4..12].try_into().unwrap());
            let to_lba = u64::from_le_bytes(rest[12..20].try_into().unwrap());
            let m = if kind == 1 {
                Mutation::RelocatePartition { partition, from_lba, to_lba }
            } else {
                Mutation::CopyPartition { partition, from_lba, to_lba }
            };
            Ok((m, 20))
        };
        match tag {
            0 => Ok((Mutation::PartitionTable, 0)),
            1 | 2 => reloc(tag),
            3 => Ok((Mutation::ExternalFsTool, 0)),
            4 => Ok((Mutation::Mkfs, 0)),
            other => Err(journal_decode_err(&format!("unknown mutation tag {other}"))),
        }
    }
}

impl JournalRecord {
    /// `payload = [mutation tag][mutation meta][recovery tag][recovery body]`
    fn encode(&self) -> Vec<u8> {
        let mut p = Vec::new();
        p.push(self.mutation.tag());
        self.mutation.write_meta(&mut p);
        match &self.recovery {
            RecoveryData::PreImage { off, bytes } => {
                p.push(0);
                p.extend_from_slice(&off.to_le_bytes());
                p.extend_from_slice(bytes);
            }
            RecoveryData::Barrier => p.push(1),
        }
        p
    }

    fn decode(payload: &[u8]) -> io::Result<Self> {
        let tag = *payload.first().ok_or_else(|| journal_decode_err("empty payload"))?;
        let (mutation, used) = Mutation::read(tag, &payload[1..])?;
        let rest = &payload[1 + used..];
        let rtag = *rest.first().ok_or_else(|| journal_decode_err("missing recovery tag"))?;
        let recovery = match rtag {
            0 => {
                if rest.len() < 9 {
                    return Err(journal_decode_err("truncated pre-image"));
                }
                let off = u64::from_le_bytes(rest[1..9].try_into().unwrap());
                RecoveryData::PreImage { off, bytes: rest[9..].to_vec() }
            }
            1 => RecoveryData::Barrier,
            other => return Err(journal_decode_err(&format!("unknown recovery tag {other}"))),
        };
        Ok(JournalRecord { mutation, recovery })
    }
}

/// journal 整份读取的结论：区分"读完了"与"尾部有一笔未完成的事务"。
/// 后者是 append-only 语义下的正常产物（最后一次追加中途断电），不是损坏——两者若共用一个
/// 表示，"崩溃后能不能回滚"就变成靠错误文案猜的事
pub enum JournalRead {
    /// 全部记录完整且 CRC 校验通过
    Complete(Vec<JournalRecord>),
    /// 尾部存在一条未完成的记录，已丢弃；前面已完整的记录前缀原样返回
    TruncatedTail(Vec<JournalRecord>),
}

/// durable transaction log：追加式 `[len u32][crc32 u32][payload]` 记录流，其中
/// `payload = [mutation][recovery]`（见 [`JournalRecord`]）。
///
/// 每条记录先于实际写入持久化，故任意落点断电后 undo 都能还原已发生的写入。
/// pre-image 只覆盖本工具**元数据**写入（write_at）；回滚不了的那些写入——分区搬移的
/// **数据块**（走 write_data_at）、外部 FS 工具的写入（resize2fs/mkswap/mkfs…）、
/// 内核侧的分区表写入——不记字节，改为在**首次此类写入之前**记一条
/// [`RecoveryData::Barrier`]，其 `mutation` 说明是哪一类越过了不可回滚点。
/// 回放时整卷日志全量载入内存。
///
/// 文件是**惰性**创建的：写下第一条记录之前磁盘上没有它（见 open）
pub struct Journal {
    file: Option<File>,
    path: PathBuf,
    /// 之后写入的 pre-image 记在哪个 mutation 名下。由知道自己在做什么的那一层设置
    /// （见 [`FileSource::set_mutation`]）；journal 不猜语义
    mutation: Mutation,
}

impl Journal {
    /// 尾字节是格式版本：读到的 magic 不符即拒绝整卷（见 open）。版本**不做旧格式兼容**——
    /// 旧 journal 会被当作"读不出来的现场"，由 `abandon` 释放，而不是猜着回放
    const MAGIC: &[u8; 5] = b"DEJL\x02";

    fn at(path: &Path, file: Option<File>) -> Self {
        Journal { file, path: path.to_path_buf(), mutation: Mutation::PartitionTable }
    }

    /// 打开（必要时先校验）落点，**不产生任何痕迹**：文件已存在则必须是我们自己写的
    /// journal——拒绝把陌生文件当 journal 追加；不存在则什么都不建。
    ///
    /// 撤销窗口的痕迹应当由"确实记了什么"产生，而不是由"打开目标"产生：否则一条在
    /// 校验阶段就被拒的命令会留下空 journal，让后续 undo 报"journal 是空的"而不是
    /// "没有 journal"，也占住了候选落点
    pub fn open(path: &Path) -> io::Result<Self> {
        match OpenOptions::new().read(true).append(true).open(path) {
            Ok(mut f) => {
                use std::io::Read;
                let len = f.metadata()?.len();
                // 0 字节 = `ensure` 建好文件、但 magic 没写完（ENOSPC/EIO）留下的**空壳**。
                // 它不含任何记录 ⇒ 没有任何写入依赖它，就地补写 magic 即可。
                // 报"太短"会把"上次创建未完成"说成"journal 损坏"，把方向指错
                if len == 0 {
                    use std::io::Write;
                    f.write_all(Self::MAGIC)?; // append 模式：空文件的落点就是 0
                    f.sync_all()?;
                    best_effort_dir_fsync(path);
                    return Ok(Self::at(path, Some(f)));
                }
                let mut hdr = [0u8; Self::MAGIC.len()];
                f.read_exact(&mut hdr).map_err(|e| match e.kind() {
                    // 文件在但读不满一个头 = 魔数还没写完就断了（残骸）；
                    io::ErrorKind::UnexpectedEof => io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!(
                            "{} is a leftover from an interrupted journal creation ({len} bytes, no complete magic header) — \
                             it holds no records, so nothing depends on it; deleting it is safe (inspect it first if you do not trust that)",
                            path.display()
                        ),
                    ),
                    // 读得出长度却读不动内容是 I/O 故障：说成"创建残骸"会把用户引向
                    // 删文件，而真正该修的是那台盘
                    _ => io::Error::new(
                        e.kind(),
                        format!("{}: journal header read failed: {e}", path.display()),
                    ),
                })?;
                if &hdr != Self::MAGIC {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        // 版本只升不兼：旧版本的 journal 一律当作"读不出来的现场"交给 abandon，
                        // 而不是猜着回放（记录布局已经不同）
                        "existing file is not a diskedit journal of this format (magic/version mismatch) — \
                         if it was written by an older version of this tool, release it with `diskedit abandon`",
                    ));
                }
                Ok(Self::at(path, Some(f)))
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Self::at(path, None)),
            Err(e) => Err(e),
        }
    }

    /// 首次记录时才落盘。magic 先于记录、记录先于实际写入，故任何时刻的盘上内容
    /// 都不会超前于已经被记录的写入。
    ///
    /// 用 `create_new` 而不是 `create`：`open` 的"文件不存在"与本处的"创建它"之间存在
    /// TOCTOU 窗口，普通的 create 会安静地打开一个刚被别人建好的文件，把两方记录交错进
    /// 同一份 journal（一次 undo 就会回放出不属于本次的字节）。`create_new` 是原子断言
    /// "此前不存在"，失败即说明有人抢先——要么是另一个 diskedit 正在用同一个目标，
    /// 要么是上一次的 journal 没处理干净
    fn ensure(&mut self) -> io::Result<&mut File> {
        if self.file.is_none() {
            // 落点目录归文件自己保证：路径由身份派生，身份不知道目录是否存在
            if let Some(dir) = self.path.parent() {
                best_effort_mkdir(dir);
            }
            let mut f = OpenOptions::new().create_new(true).write(true).open(&self.path).map_err(|e| {
                if e.kind() == io::ErrorKind::AlreadyExists {
                    io::Error::new(
                        io::ErrorKind::AlreadyExists,
                        format!(
                            "{} appeared after the target was opened — another diskedit run may be using this target, or a previous journal was left behind (resolve it with `diskedit undo` first)",
                            self.path.display()
                        ),
                    )
                } else {
                    e
                }
            })?;
            use std::io::Write;
            f.write_all(Self::MAGIC)?;
            f.sync_all()?;
            // 新建文件的目录项也必须落盘：否则断电后 journal 整体消失，而写入已经发生
            best_effort_dir_fsync(&self.path);
            self.file = Some(f);
        }
        Ok(self.file.as_mut().expect("the branch above fills an empty slot"))
    }

    pub fn record(&mut self, off: u64, bytes: &[u8]) -> io::Result<()> {
        let rec = JournalRecord {
            mutation: self.mutation,
            recovery: RecoveryData::PreImage { off, bytes: bytes.to_vec() },
        };
        self.append(&rec)
    }

    /// 落一条屏障记录：从这一刻起，当前 mutation 已越过不可回滚点，undo 见到即整体拒绝
    /// （见 [`FileSource::mark_non_reversible`]）
    pub fn barrier(&mut self) -> io::Result<()> {
        let rec = JournalRecord { mutation: self.mutation, recovery: RecoveryData::Barrier };
        self.append(&rec)
    }

    /// 设置后续记录的 mutation。命令层在自己即将做的那类改动发生变化时调用
    /// （例如表项写完后要搬数据、要交给外部 FS 工具）
    pub fn set_mutation(&mut self, m: Mutation) {
        self.mutation = m;
    }

    fn append(&mut self, rec: &JournalRecord) -> io::Result<()> {
        use std::io::Write;
        let payload = rec.encode();
        let f = self.ensure()?;
        f.write_all(&(payload.len() as u32).to_le_bytes())?;
        f.write_all(&crate::table::crc32(&payload).to_le_bytes())?;
        f.write_all(&payload)?;
        f.sync_data()
    }

    /// 逐条校验并读取。**契约不变**：已形成的记录必须 CRC 正确，任一条损坏即整体拒绝，
    /// 不做"跳过坏记录继续回放"式的部分回放。
    ///
    /// 额外单列的是"**未完成的尾部记录**"——它与"损坏"不是一回事：`record` 先写头再写数据、
    /// 最后才 sync，而调用方在 record 返回之后才真正写盘，因此尾部撕裂只可能来自一次没走完的
    /// 追加，那条记录对应的数据写入**根本没有发生**，丢弃它是安全的。中途（非尾部）CRC 不符
    /// 才是真实损坏，仍整体拒绝
    ///
    /// 记录头里的 len 不参与任何 CRC，故"len 被写坏成大值"与"数据只写了一半"在文件里
    /// 无法区分，两者都落在 TruncatedTail。这不影响安全性——返回的前缀每条都通过了 CRC，
    /// 且各自对应的写入确实发生过；代价只是该点之后的记录无法回放，措辞里已如实点明
    pub fn read_entries(path: &Path) -> io::Result<JournalRead> {
        let data = std::fs::read(path)?;
        if data.len() < Self::MAGIC.len() || &data[..Self::MAGIC.len()] != Self::MAGIC {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "bad journal header"));
        }
        let mut out = Vec::new();
        let mut pos = Self::MAGIC.len();
        while pos < data.len() {
            // 记录头尚未写全 → 尾部未完成的事务
            if pos + REC_HDR > data.len() {
                return Ok(JournalRead::TruncatedTail(out));
            }
            let len = u32::from_le_bytes(data[pos..pos + 4].try_into().unwrap()) as usize;
            let crc = u32::from_le_bytes(data[pos + 4..pos + 8].try_into().unwrap());
            pos += REC_HDR;
            // 头写全了但载荷没写全 → 尾部未完成的事务
            if pos + len > data.len() {
                return Ok(JournalRead::TruncatedTail(out));
            }
            let payload = &data[pos..pos + len];
            if crate::table::crc32(payload) != crc {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "journal entry CRC mismatch"));
            }
            pos += len;
            // 载荷解不出来同样是"读不出来"：宁可让调用方按"有事没做完"处理，也不静默跳过
            out.push(JournalRecord::decode(payload)?);
        }
        Ok(JournalRead::Complete(out))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::let_underscore_must_use)] // 清理临时文件有意忽略失败
    use super::*;

    /// `:N` 后缀的判定：分区号 1-based，`:0` 必须拒绝而不是折叠成"整盘"——
    /// 折叠会把一次针对具体分区的操作放大成对整盘的表操作
    #[test]
    fn parse_target_partition_suffix() {
        assert_eq!(parse_target("img").unwrap(), ("img".to_string(), None));
        assert_eq!(parse_target("img:1").unwrap(), ("img".to_string(), Some(1)));
        assert_eq!(parse_target("img:4294967295").unwrap(), ("img".to_string(), Some(u32::MAX)));
        assert!(parse_target("img:0").is_err());
        assert!(parse_target("img:4294967296").is_err());
        assert!(parse_target("img:").is_err());
        // 文件名的冒号不是分区后缀（其后不是纯数字）
        assert_eq!(parse_target("/a/b:c.img").unwrap(), ("/a/b:c.img".to_string(), None));
    }

    fn journal_path(tag: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("diskedit_jr_{tag}_{}.log", std::process::id()));
        let _ = std::fs::remove_file(&p);
        p
    }

    /// 尾部撕裂是"未完成的事务"，不是损坏：丢弃那一条、回放完整前缀；
    /// 中途 CRC 不符才是损坏，仍整体拒绝（"不做部分回放"的契约只对后者生效）
    #[test]
    fn journal_tail_truncation_is_recovered_but_corruption_is_not() {
        let p = journal_path("tail");
        const DATA: usize = 512;
        // [len u32][crc u32] + [mutation u8][recovery u8][off u64][data]（PartitionTable 无 meta）
        const REC: usize = REC_HDR + 10 + DATA;
        {
            let mut j = Journal::open(&p).unwrap();
            for i in 0..3u64 {
                j.record(1024 * i, &[0xAA; DATA]).unwrap();
            }
        }
        assert!(matches!(
            Journal::read_entries(&p).unwrap(),
            JournalRead::Complete(v) if v.len() == 3
        ));

        // 把最后一条记录截掉任意一段非空的长度 → 都应是"2 条完整 + 尾部未完成"。
        // 上界取 REC（不含）：截满 REC 等于整条消失，那就不再是撕裂而是"到此为止"
        let full = std::fs::read(&p).unwrap();
        for cut in 1..REC {
            std::fs::write(&p, &full[..full.len() - cut]).unwrap();
            match Journal::read_entries(&p).unwrap() {
                JournalRead::TruncatedTail(v) => assert_eq!(v.len(), 2, "cut {cut}"),
                JournalRead::Complete(v) => {
                    panic!("cut {cut}: expected a truncated tail, got {} complete record(s)", v.len())
                }
            }
        }
        // 截满一条 = 该记录整条没写进去，此时应报 Complete(2)，不是撕裂
        std::fs::write(&p, &full[..full.len() - REC]).unwrap();
        assert!(matches!(
            Journal::read_entries(&p).unwrap(),
            JournalRead::Complete(v) if v.len() == 2
        ));

        // 中途（非尾部）CRC 损坏 → 整体拒绝
        let mut corrupt = full.clone();
        corrupt[5 + REC_HDR + 10 + 100] ^= 0xFF; // 第 1 条记录的 pre-image 字节中段
        std::fs::write(&p, &corrupt).unwrap();
        assert!(Journal::read_entries(&p).is_err(), "mid-file corruption must be refused");

        let _ = std::fs::remove_file(&p);
    }

    /// 惰性创建：打开只做校验、不落痕迹，第一条记录才建文件。若 open 就建，一条在被拒阶段
    /// 结束的命令会留下空 journal，让后续 undo 分不清"没有 journal"与"journal 是空的"
    #[test]
    fn journal_is_created_lazily() {
        let p = journal_path("lazy");
        let mut j = Journal::open(&p).unwrap();
        assert!(!p.exists(), "opening a journal must not leave a trace on disk");
        j.record(0, &[0xAA; 4]).unwrap();
        assert!(p.exists(), "the first record must materialize the file");
        drop(j);

        // 已有文件必须是我们的 journal：陌生文件不得被当作 journal 追加
        std::fs::write(&p, b"not-a-journal").unwrap();
        assert!(Journal::open(&p).is_err(), "a foreign file must be refused, not adopted");

        let _ = std::fs::remove_file(&p);
    }

    /// open 与首次 record 之间存在窗口：此刻冒出来的文件不是我们建的，必须拒绝而不是
    /// 把 MAGIC 追加进去（两方记录交错进同一份 journal，一次 undo 会回放出不属于本次的字节）。
    /// 抢建的文件还要原样保留——“拒绝”不包括把它截断
    #[test]
    fn journal_creation_refuses_a_file_that_appeared_late() {
        let p = journal_path("toctou");
        let mut j = Journal::open(&p).unwrap();
        assert!(!p.exists(), "opening a journal must not leave a trace on disk");

        std::fs::write(&p, b"someone else's file").unwrap();
        assert!(j.record(0, &[0xAA; 4]).is_err(), "a file that appeared after open must not be adopted");
        assert_eq!(std::fs::read(&p).unwrap(), b"someone else's file", "the intruding file must be left untouched");

        let _ = std::fs::remove_file(&p);
    }

    /// 日志落点也由身份推导：镜像与目标同层级，块设备落在 state_dir 下且以 Disk GUID /
    /// devname 命名。模块自行拼 state_dir() 会让落点随调用方漂移。
    /// 块设备侧的推导断言只在非 Linux 跑：Linux 上 /dev/sdz 的拓扑解析不出（fail-closed），
    /// 身份构造本身会拒绝，不存在"退化身份"可用来测命名
    #[test]
    fn log_path_comes_from_the_identity() {
        let img = PathBuf::from("/tmp/disk.img");
        let id = TargetIdentity::resolve_image(&img);
        assert_eq!(id.log_path(None), PathBuf::from("/tmp/disk.img.diskedit.log"));

        #[cfg(not(target_os = "linux"))]
        {
            let dev = PathBuf::from("/dev/sdz");
            let id = TargetIdentity::resolve_block(&dev).unwrap();
            let guid = [0xABu8; 16];
            assert_eq!(id.log_path(Some(guid)), state_dir().join(format!("{}.diskedit.log", guid_hex(&guid))));
            // 读不到表时退回 devname：MBR / 裸盘上没有更稳的标识
            assert_eq!(id.log_path(None), state_dir().join("sdz.diskedit.log"));
        }
    }

    /// 非 loop 块设备的身份是**盘级**的：journal / checkpoint / lock 按 disk_key 落点，
    /// 旧版按分区落的 self_key 落点降为历史候选（整盘目标上两个键重合，不产生重复候选）。
    /// 断言用 state_dir() 自身拼期望值——测试不改环境变量，进程内并行测试共享环境
    #[test]
    fn block_identity_keys_on_the_whole_disk() {
        let id = TargetIdentity::from_block_keys(
            Path::new("/dev/sdb1"),
            "sdb-1234-p1".to_string(),
            "sdb-1234".to_string(),
        );
        let disk_tok = key_token("sdb-1234");
        let part_tok = key_token("sdb-1234-p1");
        assert_eq!(id.journal[0], state_dir().join(format!("{disk_tok}.diskedit.journal")));
        assert_eq!(id.journal[1], state_dir().join(format!("{part_tok}.diskedit.journal")));
        assert_eq!(id.journal[2], state_dir().join("sdb1.diskedit.journal"));
        assert_eq!(id.checkpoint[0], state_dir().join(format!("{disk_tok}.diskedit.ckpt")));
        assert_eq!(id.checkpoint[1], state_dir().join(format!("{part_tok}.diskedit.ckpt")));
        assert_eq!(id.lock_path(), state_dir().join(format!("{disk_tok}.diskedit.lock")));
        assert!(id.has_block_legacy_naming());

        // 整盘目标：self_key 与 disk_key 重合，不重复列同一落点
        let id = TargetIdentity::from_block_keys(
            Path::new("/dev/sdb"),
            "sdb-1234".to_string(),
            "sdb-1234".to_string(),
        );
        assert_eq!(id.journal.len(), 2, "whole-disk target has no partition-level legacy entry");
        assert_eq!(id.journal[1], state_dir().join("sdb.diskedit.journal"));
    }

    /// loop 设备的身份收敛到 backing 文件：现场落 backing 的兄弟文件（首候选），旧版
    /// state_dir 落点降为历史候选，GUID 补列开启。realpath 取不到时退到原始路径串，
    /// 兄弟文件仍是首候选——backing 消失不等于现场不可达
    #[cfg(target_os = "linux")]
    #[test]
    fn loop_identity_converges_to_the_backing_file() {
        let dir = std::env::temp_dir().join(format!("diskedit_loop_id_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let backing = dir.join("backing.img");
        std::fs::write(&backing, b"x").unwrap();

        let id = TargetIdentity::loop_backed(Path::new("/dev/loop0"), backing.clone());
        assert_eq!(id.journal[0], suffix_path(&backing, ".diskedit.journal"));
        assert_eq!(id.lock_path(), suffix_path(&backing, ".diskedit.lock"));
        let tok = key_token(&backing.to_string_lossy());
        assert_eq!(id.journal[1], state_dir().join(format!("{tok}.diskedit.journal")));
        assert_eq!(id.checkpoint[1], state_dir().join(format!("{tok}.diskedit.ckpt")));
        assert_eq!(id.journal[2], state_dir().join("loop0.diskedit.journal"));
        assert!(id.has_block_legacy_naming());

        // backing 已删：realpath 失败退回原始路径串，兄弟文件仍是首候选
        std::fs::remove_file(&backing).unwrap();
        let id = TargetIdentity::loop_backed(Path::new("/dev/loop0"), backing.clone());
        assert_eq!(id.journal[0], suffix_path(&backing, ".diskedit.journal"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 镜像身份以 realpath 为主体：经符号链接打开与直接打开收敛到同一现场，字面路径
    /// 的兄弟文件降为历史候选；日志命名沿用字面路径，不因 canonicalize 打断连续性
    #[cfg(target_os = "linux")]
    #[test]
    fn image_identity_prefers_the_realpath() {
        let dir = std::env::temp_dir().join(format!("diskedit_img_id_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let real = dir.join("real.img");
        std::fs::write(&real, b"x").unwrap();
        let link = dir.join("link.img");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let via_link = TargetIdentity::resolve_image(&link);
        let direct = TargetIdentity::resolve_image(&real);
        assert_eq!(via_link.journal[0], direct.journal[0], "both spellings must share one journal");
        assert_eq!(via_link.lock_path(), direct.lock_path());
        assert_eq!(via_link.journal[1], suffix_path(&link, ".diskedit.journal"), "literal path stays a legacy candidate");
        assert_eq!(via_link.log_path(None), suffix_path(&link, ".diskedit.log"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// fail-closed：sysfs 拓扑解析不出来 ⇒ 拒绝（错误信息带设备名），不退到
    /// "devname-容量"的调用方身份——那个退化会让 /dev/sdb 与 /dev/sdb1 得到两把锁。
    /// 非 Linux 平台没有 sysfs 概念，`resolve_block` 本就不走该判据
    #[cfg(target_os = "linux")]
    #[test]
    fn block_identity_refuses_unresolvable_topology() {
        // 普通文件被当作 is_block 传入：rdev=0 ⇒ /sys/dev/block/0:0 不存在 ⇒ 拒绝
        let fake = std::env::temp_dir().join(format!("diskedit_notablock_{}", std::process::id()));
        std::fs::write(&fake, b"x").unwrap();
        let e = TargetIdentity::resolve_block(&fake).expect_err("an unresolvable topology must be refused");
        assert!(e.contains("fake") || e.contains("diskedit_notablock"), "the error must name the device: {e}");
        assert!(e.contains("topology"), "{e}");
        let _ = std::fs::remove_file(&fake);
    }

    /// [`read_sysfs_attr`] 的三态必须可分（上条测试的细粒度锁）："不存在 ⇒ None"
    /// 是整设备的正常路径，"读失败 ⇒ Err" 则不得降级——两者曾被 `.ok()` 压成同一个
    /// None，分区身份会在读不出来的那一刻静默变成整设备身份
    #[cfg(target_os = "linux")]
    #[test]
    fn sysfs_attr_distinguishes_missing_from_read_failure() {
        assert_eq!(
            read_sysfs_attr(Path::new("/nonexistent/diskedit/attr")).unwrap(),
            None,
            "不存在 ⇒ None"
        );
        let tmp = std::env::temp_dir().join(format!("diskedit_attr_{}", std::process::id()));
        std::fs::write(&tmp, "7\n").unwrap();
        assert_eq!(read_sysfs_attr(&tmp).unwrap().as_deref(), Some("7"));
        // 存在但值空：与"不存在"分开（空的 partition 属性是分区侧的异常，不该走整设备分支）
        std::fs::write(&tmp, " \n").unwrap();
        assert_eq!(read_sysfs_attr(&tmp).unwrap().as_deref(), Some(""));
        let _ = std::fs::remove_file(&tmp);
        // 读目录：失败且 errno 非 ENOENT ⇒ 必须上抛，不得折进"不存在"
        let e = read_sysfs_attr(&std::env::temp_dir())
            .expect_err("reading a directory must not count as 'attribute missing'");
        assert_ne!(e.kind(), io::ErrorKind::NotFound, "{e}");
    }

    /// 锁按**盘**派生、现场按**目标**派生：两者在块设备上是同一个函数算出来的两个投影，
    /// 因此锁落点必然落在 state_dir 下、与 journal 同目录（不引入第二套路径规则）。
    /// 非 Linux 跑退化身份（无 sysfs 判据）；Linux 上身份只能来自真实拓扑
    /// （见 `block_identity_refuses_unresolvable_topology`）
    #[cfg(not(target_os = "linux"))]
    #[test]
    fn block_lock_and_journal_share_one_directory() {
        let id = TargetIdentity::resolve_block(Path::new("/dev/diskedit-nonexistent")).unwrap();
        assert_eq!(id.lock_path().parent(), Some(state_dir().as_path()));
        assert_eq!(id.journal_path().parent(), Some(state_dir().as_path()));
        assert!(id.lock_path().to_string_lossy().ends_with(".diskedit.lock"));
    }
}