//! TargetIdentity：盘级身份，以及恢复记录、锁与日志的落点派生。

use super::*;
use sha2::Digest;

/// 目标载体的稳定指纹：镜像用 (st_dev, st_ino, size)；真块设备用 (整盘容量, 设备 ID 哈希)。
/// 恢复现场用它确认"还是当时那个目标"。字段的证据强度按载体分级，比对时**逐字段**判断
/// （见 [`TargetFingerprint::has_inode_evidence`] 与 [`TargetFingerprint::has_id_evidence`]）：`size` 镜像取文件长度、块设备取整盘容量；`dev` / `ino` 是
/// 文件层身份：Unix 取 (st_dev, st_ino)，Windows 取 (卷序列号, 文件索引)——两者都由文件系统
/// 在**对象**上维护，改路径、路径被别的文件复用都不变。Windows 这一对只能从已打开的句柄
/// 问到（见 [`TargetFingerprint::from_file`]）。真块设备节点恒零；其余平台也恒零。
/// `id` 只在块设备上有值。零值表示该字段 **没有证据**，不是"匹配"
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct TargetFingerprint {
    pub dev: u64,
    pub ino: u64,
    pub size: u64,
    /// 块设备整盘稳定 ID（`dm/uuid` / `md/uuid` / `wwid` / `device/wwid` / `device/serial`
    /// 的 FNV-1a 64 哈希）；镜像与无 ID 可读的盘为 0
    pub id: u64,
}

/// 文件层身份：Windows 的 `BY_HANDLE_FILE_INFORMATION` 给出 (卷序列号, 文件索引)，
/// 标准库的 `Metadata` 上还没有稳定接口（`windows_by_handle` 仍未稳定），只能从句柄问
#[cfg(windows)]
mod win {
    use std::os::windows::io::AsRawHandle;

    #[repr(C)]
    #[derive(Default)]
    struct FileTime {
        low: u32,
        high: u32,
    }

    #[repr(C)]
    #[derive(Default)]
    struct ByHandleFileInformation {
        attributes: u32,
        creation: FileTime,
        last_access: FileTime,
        last_write: FileTime,
        volume_serial: u32,
        size_high: u32,
        size_low: u32,
        links: u32,
        index_high: u32,
        index_low: u32,
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetFileInformationByHandle(
            handle: *mut core::ffi::c_void,
            info: *mut ByHandleFileInformation,
        ) -> i32;
    }

    pub(super) fn identity(file: &std::fs::File) -> std::io::Result<(u64, u64)> {
        let mut info = ByHandleFileInformation::default();
        // SAFETY: 句柄由 `file` 在整个调用期间持有；结构体是 `#[repr(C)]` 且先置零，
        // 窗口按 `BY_HANDLE_FILE_INFORMATION` 的布局写满它
        let ok = unsafe {
            GetFileInformationByHandle(file.as_raw_handle(), &mut info)
        };
        if ok == 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok((info.volume_serial as u64, ((info.index_high as u64) << 32) | info.index_low as u64))
    }
}

impl TargetFingerprint {
    /// 有没有 **inode 级**证据：dev / ino 任一非零。两者同源（Unix 的 `st_dev`/`st_ino`、
    /// Windows 的卷序列号/文件索引），且只在目标不是真块设备节点时才有意义（节点身份不承诺
    /// 稳定），故成对判断
    pub fn has_inode_evidence(&self) -> bool {
        self.dev != 0 || self.ino != 0
    }

    /// 有没有**设备 ID**证据：id 非零。只有真块设备的整盘节点能给出该字段
    pub fn has_id_evidence(&self) -> bool {
        self.id != 0
    }

    /// 路径现在是否仍指向录制时的那个文件。`None` = 无从判断（录制时没有 inode 证据）。
    /// 调用点把 `Some(false)` 当"路径已被换掉"处理：这条判据只用来拦住"把路径交给外部
    /// 工具时落点已变"，不承诺它等同于一次完整复核
    #[cfg(unix)]
    pub(crate) fn still_names(&self, path: &Path) -> io::Result<Option<bool>> {
        if !self.has_inode_evidence() {
            return Ok(None);
        }
        use std::os::unix::fs::MetadataExt;
        let meta = std::fs::metadata(path)?;
        Ok(Some((meta.dev(), meta.ino()) == (self.dev, self.ino)))
    }

    /// 只有 `Metadata` 时的指纹。写入路径一律走 [`Self::from_file`]（Windows 的文件身份
    /// 只能从句柄问）；这里只留给手上没有句柄、且平台能从 `Metadata` 给出身份的地方，
    /// Windows 上它不存在——留着一个恒返回零指纹的入口，等于给"看起来有身份"再造一个来源
    #[cfg(not(windows))]
    pub fn from_meta(meta: &std::fs::Metadata) -> Self {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            Self { dev: meta.dev(), ino: meta.ino(), size: meta.len(), id: 0 }
        }
        #[cfg(not(unix))]
        {
            // size 出自 `Metadata::len()`，同样可读；dev / ino 在这个平台上没有对应接口，
            // 留零。消费端据此走"没有文件身份证据"那一支
            Self { dev: 0, ino: 0, size: meta.len(), id: 0 }
        }
    }

    /// 从**已打开的文件**取指纹。Windows 的 (卷序列号, 文件索引) 只能从句柄问到，标准库的
    /// `Metadata` 上没有稳定接口；Unix 与其余平台等价于 [`Self::from_meta`]。写入路径一律
    /// 用这个构造，路径在打开之后被换掉也骗不过它
    pub fn from_file(file: &std::fs::File) -> io::Result<Self> {
        let meta = file.metadata()?;
        #[cfg(windows)]
        {
            let (dev, ino) = win::identity(file)?;
            Ok(Self { dev, ino, size: meta.len(), id: 0 })
        }
        #[cfg(not(windows))]
        Ok(Self::from_meta(&meta))
    }
}

/// 当前目标能给出的"凭什么认它"
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LiveProof {
    /// 整盘节点的稳定设备 ID（[`DEVICE_ID_ATTRS`] 里 [`AttrRole::Identity`] 之一的哈希）
    DeviceId,
    /// inode 身份：镜像文件的 (dev, ino)；loop 设备取 backing 文件的 (dev, ino)，
    /// 即指纹里的 dev / ino
    Inode,
    /// 用户用 `--identity` 给出的名字：目标自己没有任何可核对的声明，这个名字由
    /// 携带者的两次调用之间保持一致
    Declared,
    /// 录制时一个身份都取不到：块设备节点上没有可读的身份属性，也不是 loop
    Unavailable,
}

/// 用户为身份不可得的目标声明的名字。名字是一句断言，不是设备给出的事实：工具用它
/// 派生记录与锁的落点，并把它原样写进记录，使"同一个名字才能续跑"成为可验证的约束
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DeclaredIdentity(String);

impl DeclaredIdentity {
    /// 名字去掉首尾空白后必须非空、不含控制字符，且有长度上限（记录里的长度前缀是
    /// `u32`，但一个可被命令行输入的区块设备名不该长到与路径混淆）
    pub(crate) fn parse(raw: &str) -> Result<Self, String> {
        let name = raw.trim();
        if name.is_empty() {
            return Err("--identity needs a non-empty name".to_string());
        }
        if name.chars().any(char::is_control) {
            return Err("--identity cannot contain control characters".to_string());
        }
        if name.len() > 255 {
            return Err("--identity is longer than 255 bytes".to_string());
        }
        Ok(Self(name.to_string()))
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

/// 当前目标的"创建时事实"
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LiveScene {
    pub fp: TargetFingerprint,
    pub map: Option<(u64, u64)>,
    pub proof: LiveProof,
}

/// FNV-1a 64（http://www.isthe.com/chongo/tech/comp/fnv/）：确定性、跨版本稳定的字符串摘要。
/// 用作设备 ID 的指纹——只参与相等比较，不承担密码学用途；标准库的 `DefaultHasher` 算法
/// 不保证跨 Rust 版本稳定，故不用
#[cfg(target_os = "linux")]
pub(crate) fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// 持久状态的默认落点
pub(crate) const DEFAULT_STATE_DIR: &str = "/var/lib/diskedit";

/// 覆盖状态根的环变量名（测试 / 容器 / 打包用），两套根共用同一个名字
pub(crate) const STATE_DIR_ENV: &str = "DISKEDIT_STATE_DIR";

/// 持久状态的落点目录（恢复记录 / 锁 / 日志）。
///
/// 默认取 `/var/lib/diskedit`：FHS §5.8 把 `/var/lib/<name>` 规定为应用/系统级、跨重启
/// 保留、且不得暴露给普通用户的状态；`$XDG_STATE_HOME` 面向的是用户级 state。本工具的
/// 块设备路径要独占打开整盘并改写分区表，属主机级操作，与后者不是一回事
///
/// `DISKEDIT_STATE_DIR` 只为测试 / 容器 / 打包提供显式 override，不接 `$XDG_STATE_HOME`
/// / `$HOME` 回退链：落点随运行用户与调用环境变化，记录与锁的命名空间就会
/// 漂移，撤销窗口和续传现场随之找不到。同一次未收尾作业的所有调用必须给同一个值
pub(crate) fn state_dir() -> PathBuf {
    match std::env::var_os(STATE_DIR_ENV) {
        Some(v) if !v.is_empty() => PathBuf::from(v),
        _ => PathBuf::from(DEFAULT_STATE_DIR),
    }
}

/// 镜像记录的每用户状态根。镜像记录按**文件身份**落在 `<root>/images/<键摘要>/`，
/// 与块设备的 `<state_root>/recovery/` 分属两个命名空间，互不扫到。
///
/// 镜像的写路径不要求 root：Linux 非 root 与 Windows 都要能写，因此不落在 `/var/lib`
/// 下。取值顺序：`DISKEDIT_STATE_DIR`（打包 / 测试用它同时覆盖两套根）→ Windows
/// `%LOCALAPPDATA%\diskedit` → unix `$XDG_STATE_HOME/diskedit` 或
/// `$HOME/.local/state/diskedit`。都取不到时没有落点：镜像的写命令拒绝，读命令也无从
/// 定位它写下的记录。
///
/// 这是每用户根：同一未收尾作业必须由同一用户、同一 `DISKEDIT_STATE_DIR` / XDG 环境
/// 继续。换用户（如 `sudo`）或换根后旧记录不在新进程的寻址域内，需用
/// `records scan <image_state_root>` 在原根下枚举
pub(crate) fn image_state_root() -> Option<PathBuf> {
    if let Some(v) = std::env::var_os(STATE_DIR_ENV)
        && !v.is_empty()
    {
        return Some(PathBuf::from(v));
    }
    #[cfg(windows)]
    {
        let base = std::env::var_os("LOCALAPPDATA").filter(|v| !v.is_empty())?;
        Some(PathBuf::from(base).join("diskedit"))
    }
    #[cfg(unix)]
    {
        if let Some(base) = std::env::var_os("XDG_STATE_HOME").filter(|v| !v.is_empty()) {
            return Some(PathBuf::from(base).join("diskedit"));
        }
        let home = std::env::var_os("HOME").filter(|v| !v.is_empty())?;
        Some(PathBuf::from(home).join(".local").join("state").join("diskedit"))
    }
    #[cfg(not(any(windows, unix)))]
    {
        None
    }
}

/// 目标身份：撤销窗口与续传现场共用的命名空间。锁与并发互斥是**盘级**的；恢复记录按
/// （身份键, 分区号）各自分开，`img:2` 与对应分区节点由此共用同一把锁、各记各的现场。
/// 镜像以 canonicalize 后的 realpath 为身份（Linux；loop 设备的 backing 侧用同一规则，
/// 两种目标表示由此收敛），realpath 取不到（cleanup 路径上文件已删）退到字面路径；
/// 非 Linux 无 loop、不 canonicalize（Windows 上 canonicalize 产生 `\\?\` verbatim
/// 路径）。块设备以设备层拓扑键为身份；loop 设备例外——身份是
/// backing 文件本身，现场落镜像兄弟文件。
///
/// 现场落点由身份派生：记录与锁各有一个落点（写入位置，也是唯一查找位置）。
/// 身份只对应一个落点，因此不会出现"同一目标有两份现场"这种需要靠歧义判定处理的局面
#[derive(Clone, Debug)]
pub struct TargetIdentity {
    pub(crate) kind: TargetKind,
    /// 用户可见的目标路径：镜像日志沿用它的既有命名（canonicalize 不打断日志连续性），
    /// 块设备只取其文件名作无表时的日志名
    pub(crate) base: PathBuf,
    /// 打开时刻**由已打开的句柄**解析出的数据路径（镜像：`/proc/self/fd` 的名字；块设备：
    /// 命令行给出的节点路径）。伴随文件与锁都按它派生，需要再次打开数据文件的调用点
    /// （loop attach、第二道 inode 锁）也用它——按 `base` 重新打开会落到命令返回后
    /// 被改名或替换的另一个文件上
    pub(crate) canonical: PathBuf,
    /// 恢复记录的落点：镜像为 `<image_state_root>/images/<键摘要>/record`（取不到
    /// 每用户状态根时为 `None`）；块设备为 `<state_root>/recovery/<identity-hash>/record`，
    /// 取不到 WWN / EUI / 可重复 serial 时为 `None`，写命令需 `--identity` 补键，否则退 6
    pub(crate) record: Option<PathBuf>,
    /// 块设备记录落点的身份键（目标 + 分区）。`record` 是它派生出的路径；
    /// 锁后复核按键比对，两份派生结果不会各自漂移。镜像与测试构造的身份没有键
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub(crate) record_key: Option<IdentityKey>,
    /// 独占锁的落点（见 `targetlock`）。**恒有值**：块设备落在 `state_dir()` 下的盘键
    /// （整盘载体，分区号不进锁键）；镜像落在记录目录内的 `record.lock`，没有每用户
    /// 状态根时退回镜像旁的 `<image>.diskedit/record.lock`。类型上没有
    /// "没有锁落点"这一状态——取不到锁就是拒绝，不存在无锁继续跑的路径
    pub(crate) lock: PathBuf,
    /// 目标的分区号（整盘为 `None`）。显式身份键按它把各分区的记录目录分开
    pub(crate) partition: Option<u32>,
    /// 用户用 `--identity` 声明的名字。只有身份不可得的块设备会有值
    pub(crate) declared: Option<DeclaredIdentity>,
    /// 载体占据声明的落点（见 `claims`）。身份不可得的块设备才有：它由整盘载体的
    /// 锁键派生，写命令据此发现"这个载体上已有一份用别的名字开出的现场"
    pub(crate) claim: Option<PathBuf>,
    /// loop 目标的指纹：由 [`Self::loop_backed`] 在核对"backing 对象仍是句柄打开的那一个"
    /// 之后**从已打开的 backing 句柄**取得。打开数据文件的路径上取指纹的调用点直接用它，
    /// 不按 backing 名字二次 stat——名字在两次取用之间可能已指向另一个文件。
    /// 读点只在 Linux 的 `block_open_extras`（`dev/source.rs`）；其余平台身份恒按镜像解析，
    /// 该字段恒为 `None` 且无人读取
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub(crate) backing_fp: Option<TargetFingerprint>,
}

/// 目标载体的种类：镜像的伴随文件落在目标旁、按文件路径打开；块设备的伴随文件落在
/// `state_dir()` 下、按设备节点打开
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TargetKind {
    Image,
    /// 块设备只在 Linux 的块路径出现；非 Linux 平台的身份恒为镜像
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    Block,
}

/// 设备 ID 属性表：顺序即优先级，读不到就退到下一层。内核并不保证这些属性在所有
/// 块设备类型上都存在（ram、无 serial 的 virtio-blk 等），故本层只回答"最强的可用身份"，
/// 链尾恒有 devname + 容量兜底。
///
/// `dm/name` 是用户指定的映射名、改名即变，只回答"现场落在哪"（记录键），不构成身份（指纹、锁键）；其余条目同时是设备声明的事实。两个消费视图由
/// 这一张表过滤出来，不各抄一份。`loop/backing_file` 不在列：loop 设备在进入本链之前
/// 就走专用的 backing 身份解析（见 [`TargetIdentity::resolve_block`]）。
/// 属性名与值一起入哈希：不同属性下的同值字符串不构成同一个事实
#[cfg(target_os = "linux")]
pub(crate) const DEVICE_ID_ATTRS: &[(&str, AttrRole)] = &[
    ("dm/uuid", AttrRole::Identity),
    ("dm/name", AttrRole::Locator),
    ("md/uuid", AttrRole::Identity),
    ("wwid", AttrRole::Identity),
    ("device/wwid", AttrRole::Identity),
    ("device/serial", AttrRole::Identity),
];

/// 属性在身份问题里的角色
#[cfg(target_os = "linux")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AttrRole {
    /// 值本身是设备声明的事实，进指纹与锁键
    Identity,
    /// 只用于定位落点：改名即变，不能当证据
    Locator,
}

/// 在目标路径后追加后缀（不适配扩展名，只做串接）：`/a/b.img` + `.bak` ⇒ `/a/b.img.bak`，
/// 与用户可见的目标名保持一一对应
pub(crate) fn suffix_path(base: &Path, suffix: &str) -> PathBuf {
    let mut p = base.to_path_buf().into_os_string();
    p.push(suffix);
    PathBuf::from(p)
}

/// 镜像的记录键：文件**对象**身份。Unix 取 `(st_dev, st_ino)`，Windows 取
/// `(卷序列号, 文件索引)`——两者由文件系统在对象上维护，改名、移动都不变。
/// 读不到时退回规范化路径：路径键在改名后失效，但比"没有落点"更接近调用方的预期
fn image_file_key(canonical: &Path) -> String {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        // audit-allow: S2-23 读不出元数据时退回路径键：键只定位落点，记录归属仍按载荷身份核对
        if let Ok(meta) = std::fs::metadata(canonical) {
            return format!("inode:{}:{}", meta.dev(), meta.ino());
        }
    }
    #[cfg(windows)]
    {
        // audit-allow: S2-24 打不开镜像时退回路径键，同上
        if let Ok(file) = std::fs::File::open(canonical)
            // audit-allow: S2-25 取不到卷序列号与文件索引时退回路径键，同上
            && let Ok((volume, index)) = win::identity(&file)
        {
            return format!("file:{volume}:{index}");
        }
    }
    format!("path:{}", canonical.display())
}

/// 镜像记录目录：`<image_state_root>/images/<键的 sha256 前 16 字节>/`。
/// 键的原文不落盘：路径形态的键可能带分隔符或超长，摘要同时让目录名等长。
///
/// 键取自文件对象：镜像被删除后取不到键，按路径的 `records list/verify` 无从定位它，
/// 记录只能由 `records scan <image_state_root>` 枚举
pub(crate) fn image_record_dir(canonical: &Path) -> Option<PathBuf> {
    let root = image_state_root()?;
    let digest = sha2::Sha256::digest(image_file_key(canonical).as_bytes());
    let key = crate::support::hex_lower(&digest[..16]);
    Some(root.join(IMAGE_DIR).join(key))
}

/// 活跃记录移进隔离区后的落点：同一目录里的隐藏名 `.abandoned[.N]`
fn quarantine_layout(from: &Path) -> (PathBuf, std::ffi::OsString) {
    let dir = match from.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => PathBuf::from("."),
    };
    (dir, std::ffi::OsString::from(QUARANTINE_ABANDONED))
}

/// 隔离条目的第 `index` 个候选名；0 是不带编号的首选名
fn quarantine_path(dir: &Path, name: &std::ffi::OsStr, index: u32) -> PathBuf {
    let mut file = name.to_os_string();
    if index > 0 {
        file.push(format!(".{index}"));
    }
    dir.join(file)
}

/// 目录里 `<name>[.N]` 的最大编号（不带编号的 `<name>` 记为 0）；一个都没有是 None。
/// 编号只增不减：新条目取最大值 +1，不填已有的空号。
/// 列不出目录是错误而不是空集：把它当成"没有既有编号"会从 0 重号，覆盖掉已有证据
fn max_quarantine_index(dir: &Path, name: &std::ffi::OsStr) -> std::io::Result<Option<u32>> {
    let base = name.to_string_lossy().into_owned();
    let mut max = None;
    let entries = std::fs::read_dir(dir).map_err(|e| {
        std::io::Error::new(e.kind(), format!("cannot enumerate the quarantine directory {}: {e}", dir.display()))
    })?;
    for entry in entries {
        let entry = entry.map_err(|e| {
            std::io::Error::new(e.kind(), format!("cannot read an entry of the quarantine directory {}: {e}", dir.display()))
        })?;
        let text = entry.file_name().to_string_lossy().into_owned();
        let Some(rest) = text.strip_prefix(base.as_str()) else { continue };
        let index = if rest.is_empty() {
            0
        } else if let Some(digits) = rest.strip_prefix('.') {
            match digits.parse::<u32>() {
                Ok(n) if !digits.is_empty() => n,
                _ => continue,
            }
        } else {
            continue;
        };
        max = Some(max.map_or(index, |m: u32| m.max(index)));
    }
    Ok(max)
}

/// [`quarantine_move`] 的结局。三态区分"什么都没发生"（`Err`）、"副本已建但原件还在"
/// （`CopiedOnly`）与"转换完成"（`Moved`）：前者不改变持久状态，中者已经创建了一条隔离
/// 条目，只是原件没删掉，重跑按 inode 收敛
#[derive(Debug)]
pub(crate) enum Quarantine {
    Moved,
    CopiedOnly { to: PathBuf, cause: std::io::Error },
}

// 测试期的定点删除失败注入：模拟"副本已建、原名删不掉"的形态。常规文件系统上
// 该形态只在文件被占用或目录只读时出现，测试里按线程设置、守卫析构即复位
#[cfg(test)]
thread_local! {
    static REMOVE_FAULT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// 注入守卫：`remove_original` 报错，守卫析构即复位
#[cfg(test)]
pub(crate) struct RemoveFaultGuard;

#[cfg(test)]
impl RemoveFaultGuard {
    pub(crate) fn armed() -> Self {
        REMOVE_FAULT.with(|f| f.set(true));
        Self
    }
}

#[cfg(test)]
impl Drop for RemoveFaultGuard {
    fn drop(&mut self) {
        REMOVE_FAULT.with(|f| f.set(false));
    }
}

/// 删掉隔离条目的原名。测试构建可注入失败，用来走通"副本已建、原件未删"的结局
fn remove_original(path: &Path) -> std::io::Result<()> {
    #[cfg(test)]
    if REMOVE_FAULT.with(|f| f.get()) {
        return Err(std::io::Error::other("injected remove fault"));
    }
    std::fs::remove_file(path)
}

/// 把活跃条目移进隔离区：`hard_link` 建副本再删原名，绝不覆盖已有条目。
///
/// 三种结局与 `link(2)` 的返回值一一对应：
/// - 建副本成功：删掉原名，转换完成
/// - 目标名已存在且与原件是**同一个 inode**：上一次建了副本而删原名没跑完的残局，
///   删原名即收敛（判据是 inode，不是文件名）
/// - 目标名已存在但是**另一个文件**：那是另一次隔离留下的证据，不能覆盖；取更大编号重试
///
/// 不用 `rename` 的理由是它会静默覆盖：POSIX 的 rename 在目标存在时直接替换掉它。
/// 硬链接不可用（FAT/exFAT、部分网络盘）时退到 `rename_over_placeholder`：先用
/// `create_new` 原子占住名字，再覆盖本进程刚建的占位。本命令持目标锁，窗口内不会有
/// 第二个 diskedit 来抢名字
pub(crate) fn quarantine_move(from: &Path) -> std::io::Result<Quarantine> {
    let (dir, name) = quarantine_layout(from);
    let max = max_quarantine_index(&dir, &name)?;
    // 残局判据先于编号分配：副本可能是任意一个已存在编号，逐个比 inode
    if let Some(max) = max {
        for index in 0..=max {
            let to = quarantine_path(&dir, &name, index);
            if same_file(from, &to) {
                remove_original(from)?;
                // 改名转换要落盘目录项：否则掉电后原件可能复活，目标重新被判成 busy
                best_effort_dir_fsync(from);
                note_rename_unverified(from, &to);
                return Ok(Quarantine::Moved);
            }
        }
    }
    let start = max.map_or(0, |m| m.saturating_add(1));
    for index in start..u32::MAX {
        let to = quarantine_path(&dir, &name, index);
        match std::fs::hard_link(from, &to) {
            Ok(()) => {
                // 副本已经落成：原件删不掉时，本次运行仍改变了持久状态
                if let Err(cause) = remove_original(from) {
                    best_effort_dir_fsync(from);
                    note_rename_unverified(from, &to);
                    return Ok(Quarantine::CopiedOnly { to, cause });
                }
                best_effort_dir_fsync(from);
                note_rename_unverified(from, &to);
                return Ok(Quarantine::Moved);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(_) => match rename_over_placeholder(from, &to) {
                Ok(()) => {
                    best_effort_dir_fsync(from);
                    note_rename_unverified(from, &to);
                    return Ok(Quarantine::Moved);
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e),
            },
        }
    }
    Err(std::io::Error::other("no free quarantine name available"))
}

/// 两个路径是否指向同一个文件。两侧都必须是普通文件（`symlink_metadata`，不跟随
/// 链接）：跟随链接的话，指向原件的隔离条目会与原件同身份，随后删掉原件的最后一个
/// 名字，只留下一条 dangling 链接。判据取自各自打开后的句柄身份——unix 是
/// `(st_dev, st_ino)`，Windows 是 `(卷序列号, 文件索引)`；路径本身不参与比较。
/// 打不开、读不出身份或任一侧不是普通文件时一律按"不同文件"处理——保守方向是不动
/// 别人的文件（顺延一个名字），而不是猜测后覆盖
#[cfg(unix)]
fn same_file(a: &Path, b: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    let regular = |p: &Path| std::fs::symlink_metadata(p).ok().filter(|m| m.file_type().is_file());
    match (regular(a), regular(b)) {
        (Some(x), Some(y)) => (x.dev(), x.ino()) == (y.dev(), y.ino()),
        _ => false,
    }
}

/// [`same_file`] 的 Windows 实现：`symlink_metadata` 只决定"是不是普通文件"，
/// 身份取打开后的句柄——`(卷序列号, 文件索引)` 只能从句柄问到
#[cfg(windows)]
fn same_file(a: &Path, b: &Path) -> bool {
    let identity = |p: &Path| {
        std::fs::symlink_metadata(p)
            .ok()
            .filter(|m| m.file_type().is_file())
            .and_then(|_| std::fs::File::open(p).ok())
            .and_then(|f| TargetFingerprint::from_file(&f).ok())
            .filter(|fp| fp.has_inode_evidence())
    };
    match (identity(a), identity(b)) {
        (Some(x), Some(y)) => (x.dev, x.ino) == (y.dev, y.ino),
        _ => false,
    }
}

/// 其余平台没有可读的句柄身份，一律按"不同文件"处理
#[cfg(not(any(unix, windows)))]
fn same_file(_a: &Path, _b: &Path) -> bool {
    false
}

// 落盘文件名的词表：记录目录后缀与 log / lock 两类伴随文件都在这里，派生点
// （身份构造、日志落点）都取自这些常量，不另写字形
macro_rules! sidecar_suffix {
    ($stem:literal) => {
        concat!(".diskedit.", $stem)
    };
}

/// 块设备没有记录目录（身份 `Unusable`）时的锁文件名后缀
#[cfg_attr(not(any(target_os = "linux", test)), allow(dead_code))]
pub(crate) const LOCK_SUFFIX: &str = sidecar_suffix!("lock");

pub(crate) const LOG_SUFFIX: &str = sidecar_suffix!("log");

/// 镜像旁记录目录的后缀：镜像 `/a/b.img` ⇒ 目录 `/a/b.img.diskedit/`
pub(crate) const RECORD_SUFFIX: &str = ".diskedit";

/// 块设备记录仓库的父目录名：`<state_root>/recovery/<identity-hash>`
pub(crate) const RECOVERY_DIR: &str = "recovery";

/// 载体占据声明的目录名：`<state_root>/claims/<盘键摘要>`。它记的是"这个载体上次
/// 用哪个显式身份打开"，与记录仓库分属两个命名空间
pub(crate) const CLAIM_DIR: &str = "claims";

/// 镜像记录仓库的父目录名：`<image_state_root>/images/<identity-hash>`
pub(crate) const IMAGE_DIR: &str = "images";

/// 记录目录内的记录文件名
pub(crate) const RECORD_NAME: &str = "record";

/// 记录目录内的记录级锁文件名
pub(crate) const RECORD_LOCK_NAME: &str = "record.lock";

/// 隔离区条目名：`recover --release` 把记录目录里的 `record` 换成这个隐藏名，重名时取 `.abandoned.N`
pub(crate) const QUARANTINE_ABANDONED: &str = ".abandoned";

/// 隔离条目的类别：`abandoned` 表示记录已不在正式名上（被放弃回滚能力）。
/// 只参与识别与报告，不参与活跃记录判定
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum QuarantineMark {
    Abandoned,
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
impl QuarantineMark {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Abandoned => "abandoned",
        }
    }
}

/// 16 字节 Disk GUID → 大写无连字符十六进制
pub(crate) fn guid_hex(g: &[u8; 16]) -> String {
    g.iter().map(|b| format!("{b:02X}")).collect()
}

/// 身份键 → 文件名安全的 token：保留 ASCII 字母数字与 `.` `-` `_`，其余（含路径分隔符）
/// 换成 `_` 并截断到 32 字符，末尾附值的 CRC32——身份可能是 loop 的 backing 路径，
/// 原样落盘会带分隔符、可能超长，而截断与替换会令两个不同身份撞同一个名字。
/// 消费者是块设备身份的落点命名
#[cfg_attr(not(any(target_os = "linux", test)), allow(dead_code))]
pub(crate) fn key_token(value: &str) -> String {
    let mut token: String = value
        .chars()
        .take(32)
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') { c } else { '_' })
        .collect();
    token.push_str(&format!("-{:08x}", crate::table::crc32(value.as_bytes())));
    token
}

impl TargetIdentity {
    /// 镜像身份。canonical = 身份主体（记录 / lock / 日志都由它派生）；
    /// literal = 用户给的路径，只用于日志命名（base）。两者不同（路径含符号链接或非规范
    /// 化成分）不影响落点：身份只认 realpath 指向的那个文件。
    ///
    /// 记录按文件对象身份落在每用户状态根里，因此改名、移动到别的目录后仍能定位到同一份
    /// 记录；取不到状态根时没有记录落点，锁退回镜像旁的 sidecar 目录
    pub(crate) fn image_at(canonical: &Path, literal: &Path) -> Self {
        let record_dir = image_record_dir(canonical);
        let lock = match &record_dir {
            Some(dir) => dir.join(RECORD_LOCK_NAME),
            None => suffix_path(canonical, RECORD_SUFFIX).join(RECORD_LOCK_NAME),
        };
        Self {
            kind: TargetKind::Image,
            base: literal.to_path_buf(),
            canonical: canonical.to_path_buf(),
            record: record_dir.map(|dir| dir.join(RECORD_NAME)),
            record_key: None,
            lock,
            partition: None,
            declared: None,
            claim: None,
            backing_fp: None,
        }
    }

    /// 镜像身份：Linux 上取 realpath（文件不存在——cleanup 路径的常态之一——退字面
    /// 路径），其余平台字面路径。realpath 只收敛符号链接与路径写法：**硬链接不收敛**
    /// （两个名字指向同一 inode，realpath 各给各的），因此同一镜像的两条硬链接路径各
    /// 派生一份记录——在一处写下的现场，另一处看不见。别名路径上的
    /// 并发写入在 unix 上由数据文件自身的 inode 级第二道锁收敛（见 `targetlock`），
    /// 其余平台只剩锁文件这一道
    pub(crate) fn resolve_image(path: &Path) -> Self {
        // 路径采样的不可失败入口：采样实现只有 `FileSource::sample_image` 一处
        #[cfg(target_os = "linux")]
        {
            crate::dev::FileSource::sample_image(path, None)
                .unwrap_or_else(|_| Self::image_at(path, path))
        }
        #[cfg(not(target_os = "linux"))]
        Self::image_at(path, path)
    }

    /// loop 设备的身份：backing **对象**本身。现场落 backing 的兄弟文件——与直接以该
    /// 镜像为目标的操作天然同一序列化域，这正是 loop↔image 的收敛点。
    ///
    /// `raw` 来自 sysfs `loop/backing_file`，内容是内核在读取属性时按读取者的 root
    /// 与 cwd 用 `d_path` 拼出的名字（`loop_attr_backing_file_show` → `file_path`）。
    /// 名字只是诊断信息，不是身份：同一对象在不同 mount namespace 下可以有不同名字，
    /// 名字也可能已被另一个文件复用。故这里重新打开它，用**已打开的句柄**解析名字
    /// （`/proc/self/fd`），并要求该名字此刻仍指向同一个 inode；backing 不是普通文件、
    /// 打不开或名字已不指向它，一律 `Err`——退到字符串会把记录与锁
    /// 挂到一个与 loop 无关的文件旁
    ///
    #[cfg(target_os = "linux")]
    pub(crate) fn loop_backed(dev_path: &Path, raw: PathBuf) -> Result<Self, String> {
        let file = std::fs::File::open(&raw).map_err(|e| {
            format!(
                "cannot open the backing file {} of {}: {e} — refusing to derive the target \
                 identity from the sysfs path string alone",
                raw.display(),
                dev_path.display()
            )
        })?;
        let meta = file.metadata().map_err(|e| {
            format!("cannot stat the backing file {} of {}: {e}", raw.display(), dev_path.display())
        })?;
        if !meta.is_file() {
            return Err(format!(
                "{} is backed by {} (not a regular file) — its recovery state has no data file to \
                 be placed next to",
                dev_path.display(),
                raw.display()
            ));
        }
        let mut id = crate::dev::FileSource::sample_image(&raw, Some((&file, &meta))).map_err(|e| {
            format!("cannot bind the recovery state of {} to its backing file: {e}", dev_path.display())
        })?;
        // sysfs 串只说明"内核认为这个名字是 backing"，而名字可能已被另一个文件复用。
        // 内核里那条事实是 loop 记下的 backing **对象**（`LOOP_GET_STATUS64` 的
        // dev / ino / rdev，由 loop_get_status 对 backing 的 f_path 做 vfs_getattr 得到）：
        // 与刚打开的文件逐个比对，比不上一律拒绝——否则记录会落在
        // 一个与这个 loop 无关的文件旁边
        {
            use std::os::unix::fs::MetadataExt;
            let bound = crate::fsops::mount::loop_backing_of_node(dev_path)
                .map_err(|e| format!("cannot read the backing object of {}: {e}", dev_path.display()))?
                .ok_or_else(|| {
                    format!(
                        "{} reports no bound backing file — refusing to place its recovery state next to {}",
                        dev_path.display(),
                        raw.display()
                    )
                })?;
            // backing 上面已限定为普通文件，按 (dev, ino) 认，与占用判定同一套判据
            let same_object = bound.dev == meta.dev() && bound.ino == meta.ino();
            if !same_object {
                return Err(format!(
                    "{} is no longer backed by {} (the bound backing is a different object) — \
                     refusing to derive the target identity from the sysfs path string",
                    dev_path.display(),
                    raw.display()
                ));
            }
        }
        // 指纹取自刚核对过的那一个 backing 对象（句柄的元数据），打开数据文件的路径不按
        // 名字二次 stat
        id.backing_fp = Some(TargetFingerprint::from_meta(&meta));
        Ok(id)
    }

    /// 块设备身份：打开目标时解析一次，设备层拓扑取自 sysfs。
    /// **fail-closed**：拓扑解析不出来即 `Err`（错误信息带设备名），调用方拒绝——
    /// 绝不退到"devname-容量"的调用方身份，记录与锁的落点
    /// 必须落在同一个序列化域。
    ///
    /// loop 设备走专用解析：`loop/backing_file` 属性在 ⇒ 身份是 backing 文件（见
    /// [`Self::loop_backed`]），不以属性值充当设备 ID。读失败原样上抛；属性不存在
    /// （含空值）按非 loop 走拓扑链
    ///
    /// 不接收容量：容量不参与身份（分区扩容会改变自己的容量，父设备容量不受影响，
    /// 撤销窗口不该在操作中途改名）
    #[cfg(target_os = "linux")]
    pub(crate) fn resolve_block(path: &Path) -> Result<Self, String> {
        let node = sysfs_node(path).map_err(|e| e.to_string())?;
        Self::resolve_block_at(&node, path)
    }

    /// 已解析出 sysfs 节点时的身份构造（打开路径按 fd 的 rdev 定位，见
    /// [`sysfs_node_from_rdev`]）。其余判据与 [`Self::resolve_block`] 完全一致
    #[cfg(target_os = "linux")]
    pub(crate) fn resolve_block_at(node: &Path, path: &Path) -> Result<Self, String> {
        // loop 属性属于整设备节点：分区节点经设备链上溯到父 loop 设备后再读
        if let Some(raw) = loop_backing_at(node).map_err(|e| e.to_string())? {
            return Self::loop_backed(path, raw);
        }
        let disk_key = block_keys(node, path).map_err(|e| e.to_string())?;
        // 锁键单独派生：`dm/name` 不在其中，改名不会把同一台设备的锁拆成两把
        let lock_key = block_lock_key(node).map_err(|e| e.to_string())?;
        // 记录目录由 WWN / EUI / serial 派生的身份键决定；dm / md / 只有 major:minor
        // 的目标没有键，记录落点为 None，写命令退 6（或由 `--identity` 补一个键）
        let (record_key, partition) = block_record_context(node).map_err(|e| e.to_string())?;
        Ok(Self::from_block_parts(path, disk_key, lock_key, record_key, partition))
    }

    /// 非 Linux 平台没有 sysfs，块设备判定恒 false，本入口不会到达。非 Linux 上没有块
    /// 设备的生产打开路径，此实现只服务测试的命名与锁落点断言，故随测试一同编译
    #[cfg(all(not(target_os = "linux"), test))]
    pub(crate) fn resolve_block(path: &Path) -> Result<Self, String> {
        let k = file_name_lossy(path);
        Ok(Self::from_block_keys(path, k))
    }

    /// 块设备身份构造（测试用固定身份）：锁按整盘 `disk_key` 派生，不带记录键；
    /// 分区号属于操作，不进锁的落点。需要记录目录的身份经 [`Self::from_block_parts`]
    /// 构造，记录目录按（身份键, 分区号）各自分开
    /// 生产路径统一从 `resolve_block_at`（非 Linux 测试里是 `resolve_block`）进入，
    /// 这里只供测试构造固定身份
    #[cfg(test)]
    pub(crate) fn from_block_keys(path: &Path, disk_key: String) -> Self {
        let lock_key = crate::dev::LockKey::new(disk_key.clone());
        Self::from_block_parts(path, disk_key, lock_key, None, None)
    }

    /// 块设备身份构造：`record_key` 为 None 表示取不到 WWN / EUI / 可重复 serial
    /// （dm / md / 只有 major:minor）。锁按 `lock_key` 派生，分区号不进锁的落点，
    /// `/dev/sdb` 与 `/dev/sdb1` 由此共用一把锁；记录目录按（身份键, 分区号）各自分开。
    /// 锁键另走一套属性表（见 [`crate::dev::block_lock_key`]）。
    /// 身份不可得的目标另有一个载体占据声明落点（见 `claims`），由整盘锁键派生
    #[cfg(any(target_os = "linux", test))]
    pub(crate) fn from_block_parts(
        path: &Path,
        _disk_key: String,
        lock_key: crate::dev::LockKey,
        record_key: Option<IdentityKey>,
        partition: Option<u32>,
    ) -> Self {
        let dir = host_state_root();
        let lock = dir.join(format!("{}{LOCK_SUFFIX}", key_token(lock_key.as_str())));
        let claim = record_key.is_none().then(|| claim_path(lock_key.as_str(), partition));
        let record = record_key.as_ref().map(IdentityKey::record_path);
        Self {
            kind: TargetKind::Block,
            base: path.to_path_buf(),
            canonical: path.to_path_buf(),
            record,
            record_key,
            // 锁文件是纯运行时 artifact（重启自清也无妨），残留不构成阻挡——
            // 判据是"锁取不取得到"，不是"文件在不在"（见 targetlock）
            lock,
            partition,
            declared: None,
            claim,
            backing_fp: None,
        }
    }

    /// 用户用 `--identity` 给身份不可得的目标声明一个名字。
    ///
    /// 只接受块设备里既没有记录键也没有记录落点的目标（nbd / rbd / dm / md）：
    /// 镜像按文件对象定位，带 WWN / serial 的设备有自己可核对的声明，两者都不该
    /// 再被一个自由文本名字覆盖。记录键、锁键与记录里的现场都改由这个名字派生。
    ///
    /// 锁落在盘级：分区号不进锁键，所以 `/dev/nbd0` 与 `/dev/nbd0p1` 用同一个名字时
    /// 共用一把锁
    pub(crate) fn with_declared(mut self, declared: &DeclaredIdentity) -> Result<Self, String> {
        if self.kind != TargetKind::Block || self.record.is_some() || self.record_key.is_some() {
            return Err(
                "--identity applies only to block devices with no readable WWN, EUI or repeatable \
                 serial; images and identified devices are addressed by their own identity"
                    .to_string(),
            );
        }
        let key = declared_identity_key(declared.as_str(), self.partition);
        let lock_token = key_token(&declared_lock_key(declared.as_str()));
        self.lock = host_state_root().join(format!("{lock_token}{LOCK_SUFFIX}"));
        self.record = Some(key.record_path());
        self.record_key = Some(key);
        self.declared = Some(declared.clone());
        Ok(self)
    }

    /// `--identity` 给出的名字：未给出时原样返回；给出时按声明名派生记录、锁与占据声明的落点。
    /// 适用范围由 [`Self::with_declared`] 决定
    pub(crate) fn with_declared_name(self, raw: Option<&str>) -> Result<Self, String> {
        match raw {
            None => Ok(self),
            Some(raw) => {
                let name = DeclaredIdentity::parse(raw)?;
                self.with_declared(&name)
            }
        }
    }

    /// 用户声明的身份（`--identity`）
    pub(crate) fn declared(&self) -> Option<&DeclaredIdentity> {
        self.declared.as_ref()
    }

    /// 载体占据声明的落点。只有身份不可得的块设备有
    pub(crate) fn claim_path(&self) -> Option<&Path> {
        self.claim.as_deref()
    }

    /// 用某个声明名派生的记录落点：占据声明据此判断"那个名字下的现场还在不在"。
    /// 分区号沿用本目标的
    pub(crate) fn record_path_for_declared(&self, declared: &str) -> PathBuf {
        declared_identity_key(declared, self.partition).record_path()
    }

    /// 清理类命令（`recover --release`）的身份解析：目标可能已不存在，而现场文件可能比目标活得
    /// 久（用户删了镜像，记录目录与锁还在）。路径 stat 不了时按镜像身份回退——镜像
    /// 身份就是给定的路径本身，落点由它派生，不需要目标存在。节点消失的块设备身份
    /// 不可恢复（拓扑派生），其 state_dir 下的现场随之不可达：此处无从区分两者，按
    /// 镜像回退时前者得救、后者空跑——报"no recovery record … nothing to recover"而不是按错身份删现场。
    /// 拓扑解析得出但解析失败的块设备仍返回 None（保守方向：宁可漏清，也不按错身份删现场）。
    /// loop 设备的身份绑定 backing 对象：backing 打不开或已不是普通文件时
    /// [`Self::loop_backed`] 返回 Err，这里随之返回 None——它留在原落点上的现场无从定位，
    /// 因为定位它所需的身份在 backing 消失后无法从对象本身得出
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

    /// 单文件恢复记录的落点；块设备在身份为 `Unusable` 时为 `None`
    pub(crate) fn record_path(&self) -> Option<&Path> {
        self.record.as_deref()
    }

    /// 独占锁的落点。恒有值：取锁失败即拒绝，不提供"没有锁落点"这种状态
    pub(crate) fn lock_path(&self) -> &Path {
        &self.lock
    }

    /// 再次打开**同一个目标**时用的路径：镜像取打开时刻由 fd 解析出的名字，
    /// 块设备取节点路径（身份本就由 fd 的 rdev 派生）。目标锁会对这条路径打开的
    /// 句柄再加一道 inode 级锁，硬链接与符号链接别名因此收敛到同一把锁
    pub(crate) fn data_path(&self) -> &Path {
        if matches!(self.kind, TargetKind::Image) {
            &self.canonical
        } else {
            &self.base
        }
    }

    /// 日志落点：镜像 = 目标路径旁的兄弟文件；块设备 = `<state_dir>/<名>` 加同一后缀，
    /// 名取可读的 GPT Disk GUID，读不到表时退到 devname。
    ///
    /// 日志在单个文件内只追加、不参与恢复（打开时按上限
    /// 轮转一代，见 `support.rs` 的 `Logger`），名字也保持既有约定
    /// 不变——改名会打断已写下日志的连续性。落点只在此处拼装，调用方不得自行拼 `state_dir()`
    pub(crate) fn log_path(&self, disk_guid: Option<[u8; 16]>) -> PathBuf {
        if self.kind == TargetKind::Image {
            return suffix_path(&self.base, LOG_SUFFIX);
        }
        let name = match disk_guid {
            Some(g) => guid_hex(&g),
            // 无 GPT（MBR / 裸盘）：devname 是这类目标上唯一稳定的标识
            None => file_name_lossy(&self.base),
        };
        state_dir().join(format!("{name}{LOG_SUFFIX}"))
    }
}

/// 目标是否为**分区节点**（sysfs `partition` 属性存在且可解析）。镜像恒 false。
/// `new` 用它做整盘校验：分区表写在节点最前部，落到分区节点上会覆盖该分区的
/// 数据（512B 扇区下 ext 超级块就在 +1024）。拓扑解析或属性读取失败时按
/// 分区节点处理——判据读不出来就拒，是设备层一贯的 fail-closed 口径
pub(crate) fn target_is_partition_node(path: &Path) -> bool {
    #[cfg(target_os = "linux")]
    {
        match std::fs::metadata(path).map(|m| m.file_type().is_block_device()) {
            Ok(true) => {}
            _ => return false,
        }
        // audit-allow: S2-4
        let Ok(node) = sysfs_node(path) else { return true };
        // 只有"属性不存在"才是整设备；值存在（无论能否解析成数字）与读失败都按分区
        // 节点处理——判据读不出来就拒，与上面的口径一致
        !matches!(read_sysfs_attr(&node.join("partition")), Ok(None))
    }
    #[cfg(not(target_os = "linux"))]
    {
        // audit-allow: D-22
        let _ = path;
        false
    }
}
