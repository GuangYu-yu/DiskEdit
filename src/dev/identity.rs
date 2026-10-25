//! TargetIdentity：盘级身份，以及 journal/checkpoint/lock/log 的落点派生。

use super::*;

/// 目标载体的稳定指纹：镜像用 (st_dev, st_ino, size)；真块设备用 (整盘容量, 设备 ID 哈希)。
/// 恢复现场用它确认"还是当时那个目标"。字段的证据强度按载体分级，比对时**逐字段**判断
/// （见 [`scene_mismatch`]）：`size` 镜像取文件长度、块设备取整盘容量；`dev` / `ino` 是
/// Unix 扩展接口，非 Unix 平台与真块设备的节点恒零；`id` 只在块设备上有值。零值表示该字段
/// **没有证据**，不是"匹配"
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct TargetFingerprint {
    pub dev: u64,
    pub ino: u64,
    pub size: u64,
    /// 块设备整盘稳定 ID（`dm/uuid` / `md/uuid` / `wwid` / `device/wwid` / `device/serial`
    /// 的 FNV-1a 64 哈希）；镜像与无 ID 可读的盘为 0
    pub id: u64,
}

impl TargetFingerprint {
    /// 有没有 **inode 级**证据：dev / ino 任一非零。两者同源（`MetadataExt`），且只在 Unix 上、
    /// 且目标不是真块设备节点时才有意义（节点 inode 不承诺稳定），故成对判断
    pub fn has_inode_evidence(&self) -> bool {
        self.dev != 0 || self.ino != 0
    }

    /// 有没有**尺寸**证据：size 非零。镜像取 `Metadata::len()`，块设备取整盘容量
    pub fn has_size_evidence(&self) -> bool {
        self.size != 0
    }

    /// 有没有**设备 ID**证据：id 非零。只有真块设备的整盘节点能给出该字段
    pub fn has_id_evidence(&self) -> bool {
        self.id != 0
    }

    /// 有没有**任一**可比字段。全零的指纹出自"这个载体什么也给不出来"（任何指纹恒零的载体）；
    /// undo 在回放前据此报出"一个字段都没比"的身份边界
    pub fn has_any_evidence(&self) -> bool {
        self.has_inode_evidence() || self.has_size_evidence() || self.has_id_evidence()
    }

    pub fn from_meta(meta: &std::fs::Metadata) -> Self {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            Self { dev: meta.dev(), ino: meta.ino(), size: meta.len(), id: 0 }
        }
        #[cfg(not(unix))]
        {
            // size 出自 `Metadata::len()`，非 Unix 平台同样可读；dev / ino 没有对应接口，留零
            Self { dev: 0, ino: 0, size: meta.len(), id: 0 }
        }
    }

    /// `path` 现在是否仍指向这份指纹描述的对象。无 inode 证据（真块设备节点、非 Unix）
    /// 时无从判断，按"是"处理——此时路径本身就是唯一凭据。调用点都是"把路径交给外部
    /// 工具之前"或"删除自己建的伴随文件之前"的最后一道核对
    #[cfg(unix)]
    pub(crate) fn still_names(&self, path: &Path) -> io::Result<bool> {
        if !self.has_inode_evidence() {
            return Ok(true);
        }
        use std::os::unix::fs::MetadataExt;
        let meta = std::fs::metadata(path)?;
        Ok((meta.dev(), meta.ino()) == (self.dev, self.ino))
    }

    /// 非 Unix 平台取不到 dev / ino（见本类型的字段说明）：路径是唯一凭据，
    /// 与 [`Self::has_inode_evidence`] 恒假同口径
    #[cfg(not(unix))]
    pub(crate) fn still_names(&self, _path: &Path) -> io::Result<bool> {
        Ok(true)
    }
}

/// 一份现场记录的"创建时目标"：指纹、loop 映射，以及**身份证据的种类**。
///
/// 两族现场（checkpoint / journal）都存这三样，比对只有一处（见 [`scene_mismatch`]）。
/// 种类随现场落盘（v6 起）：`id == 0` 在旧格式里是"格式没有这个字段"，在新格式里可能是
/// "录制时取不到"，两者在恢复时的处置不同，不能混为一谈
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SceneIdentity {
    pub fp: TargetFingerprint,
    pub map: Option<(u64, u64)>,
    pub proof: IdentityProof,
}

/// 现场记录里"凭什么认这个目标"。恢复时先比种类、再比同种类的字段：种类不同说明现在的
/// 目标不是记录时那类载体，此时逐字段的零值会把"没法比"读成"相符"
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IdentityProof {
    /// v4 / v5 头：格式里没有这一字节。只出现在解析结果里，不写入
    Legacy,
    /// 整盘节点的稳定设备 ID（[`DEVICE_IDENTITY_ATTRS`] 之一的哈希）
    DeviceId,
    /// inode 身份：镜像文件的 (dev, ino)；loop 设备取 backing 文件的 (dev, ino)，
    /// 即指纹里的 dev / ino
    Inode,
    /// 录制时一个身份都取不到：块设备节点上没有可读的身份属性，也不是 loop
    Unavailable,
}

impl IdentityProof {
    /// 落盘字节。`Legacy` 写 0，而 0 不是合法值（旧格式没有这一字节，解析按损坏拒绝）
    pub(crate) fn tag(self) -> u8 {
        match self {
            Self::Legacy => 0,
            Self::DeviceId => 1,
            Self::Inode => 2,
            Self::Unavailable => 3,
        }
    }

    pub(crate) fn from_tag(tag: u8) -> Option<Self> {
        match tag {
            1 => Some(Self::DeviceId),
            2 => Some(Self::Inode),
            3 => Some(Self::Unavailable),
            _ => None,
        }
    }
}

/// 现场记录的"创建时事实"与当前目标比对，不符即外来现场（返回一句可读的原因）。
///
/// **两族现场（checkpoint / journal）共用这一处判据**：同一种异常在两条恢复路径上必须得到
/// 同一个结论，否则拒绝理由会随入口漂移；调用方各自在原因上补齐"是哪份文件"。
///
/// **先比身份种类，再逐字段比**：种类相同才谈得上比较字段。
/// - `Legacy`（v4 / v5）只写了 inode 与尺寸：当前目标能给出 inode 身份（镜像、带 backing
///   文件的 loop）时照旧可比；目标是整盘身份或什么身份都给不出时，旧现场没有可比的字段，
///   一律拒绝——那种现场无法与"同名换过的盘"区分，只允许诊断与 abandon。
/// - `Unavailable` 是"录制时就知道取不到身份"：同样无法与同名换过的盘区分，拒绝消费。
/// - 整盘身份与 inode 身份互换（例如同一节点从磁盘变成 loop）说明载体变了，拒绝。
///
/// 同一种类内，各字段只在两侧都有证据时参与（零值 = 该平台或该载体给不出这个字段，
/// 不等于"匹配"）：尺寸镜像取文件长度、块设备取整盘容量；inode 只在 Unix 且载体有文件
/// 可指时有值（镜像文件、loop 的 backing 文件）；`id` 只在块设备整盘节点上有值。
/// **尺寸与 ID 都不是唯一标识**：同尺寸、同 ID 的克隆盘照样能通过这条检查。
///
/// 映射的有无本身参与比对：经 loop 写入的现场不允许改从裸镜像继续（反之亦然）——
/// 两种途径下同一 LBA 指向的字节不同，映射差异不是可忽略的记法差
pub(crate) fn scene_mismatch(recorded: &SceneIdentity, actual: &SceneIdentity) -> Option<String> {
    match (recorded.proof, actual.proof) {
        (IdentityProof::Inode, IdentityProof::Inode)
        | (IdentityProof::DeviceId, IdentityProof::DeviceId)
        | (IdentityProof::Legacy, IdentityProof::Inode) => {}
        (IdentityProof::Legacy, _) => {
            return Some(
                "this scene was recorded before device identities were stored, and the target now exposes \
                 no file inode to compare — a block-device scene without identity cannot be matched \
                 against the device present"
                    .to_string(),
            )
        }
        (IdentityProof::Unavailable, _) => {
            return Some(
                "this scene was recorded without any device identity (no stable attribute was readable), \
                 so it cannot be matched against the device now present"
                    .to_string(),
            )
        }
        (IdentityProof::DeviceId, IdentityProof::Unavailable) => {
            return Some(format!(
                "the device identity recorded by this scene ({:#018x}) is not readable on the target now",
                recorded.fp.id,
            ))
        }
        (IdentityProof::DeviceId, _) => {
            return Some(format!(
                "the target now exposes a file inode identity instead of the whole-disk identity this scene \
                 was recorded with ({:#018x})",
                recorded.fp.id,
            ))
        }
        (IdentityProof::Inode, IdentityProof::Unavailable) => {
            return Some(
                "the target now exposes no stable identity at all, so the file inode this scene was \
                 recorded with cannot be matched"
                    .to_string(),
            )
        }
        (IdentityProof::Inode, _) => {
            return Some(
                "the target now exposes a whole-disk identity instead of the file inode this scene was \
                 recorded with"
                    .to_string(),
            )
        }
    }
    if recorded.fp.has_inode_evidence()
        && actual.fp.has_inode_evidence()
        && (recorded.fp.dev, recorded.fp.ino) != (actual.fp.dev, actual.fp.ino)
    {
        return Some(format!(
            "the file fingerprint changed since this recovery scene was recorded (was dev:{} ino:{}, \
             now dev:{} ino:{})",
            recorded.fp.dev, recorded.fp.ino, actual.fp.dev, actual.fp.ino,
        ));
    }
    if recorded.fp.has_size_evidence() && actual.fp.has_size_evidence() && recorded.fp.size != actual.fp.size {
        return Some(format!(
            "the file size changed since this recovery scene was recorded (was {} bytes, now {})",
            recorded.fp.size, actual.fp.size,
        ));
    }
    // 种类同为 `DeviceId` 时两侧都有 ID 证据（`FileSource::proof` 只在该字段非零时给出它）
    if recorded.proof == IdentityProof::DeviceId && recorded.fp.id != actual.fp.id {
        return Some(format!(
            "the device identity changed since this recovery scene was recorded (was {:#018x}, now {:#018x})",
            recorded.fp.id, actual.fp.id,
        ));
    }
    match (recorded.map, actual.map) {
        (Some((ro, rl)), Some((co, cl))) if (ro, rl) != (co, cl) => Some(format!(
            "loop mapping changed since this recovery scene was recorded (was offset={ro} sizelimit={rl}, \
             now offset={co} sizelimit={cl})"
        )),
        (Some(_), None) => {
            Some("this recovery scene was recorded through a loop device, the target is not one".to_string())
        }
        (None, Some(_)) => {
            Some("this recovery scene was recorded on a non-loop target, this one is reached through a loop device".to_string())
        }
        _ => None,
    }
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
    pub(crate) kind: TargetKind,
    /// 用户可见的目标路径：镜像日志沿用它的既有命名（canonicalize 不打断日志连续性），
    /// 块设备只取其文件名作无表时的日志名
    pub(crate) base: PathBuf,
    /// 打开时刻**由已打开的句柄**解析出的数据路径（镜像：`/proc/self/fd` 的名字；块设备：
    /// 命令行给出的节点路径）。伴随文件与锁都按它派生，需要再次打开数据文件的调用点
    /// （loop attach、第二道 inode 锁）也用它——按 `base` 重新打开会落到命令返回后
    /// 被改名或替换的另一个文件上
    pub(crate) canonical: PathBuf,
    pub(crate) journal: Vec<PathBuf>,
    pub(crate) checkpoint: Vec<PathBuf>,
    /// 独占锁的落点（见 `targetlock`）。**恒有值**：镜像放在目标旁，块设备由设备身份
    /// 派生到 `state_dir()` 下。类型上没有"没有锁落点"这一状态——取不到锁就是拒绝，
    /// 不存在无锁继续跑的路径
    pub(crate) lock: PathBuf,
    /// 旧版按块设备语义（kind == Block）写下的 GUID 命名 checkpoint 是否可能存在。
    /// loop 归一后本身份已是 Image kind，但历史落点按 Block 规则生成，枚举时需补列
    pub(crate) legacy_guid_checkpoints: bool,
}

/// 只用于区分历史命名约定：块设备另有 GUID / devname 两份历史落点，镜像没有
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TargetKind {
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
pub(crate) const DEVICE_ID_ATTRS: &[&str] = &["dm/uuid", "dm/name", "md/uuid", "wwid", "device/wwid", "device/serial"];

/// 进指纹的设备身份：只收**设备自己声明的事实**。`dm/name` 不在列——映射名是名字而非身份，
/// 同名重建的另一个 DM 设备会得到同一个值，用它当"这还是不是当时那块盘"的证据等于没有证据。
/// 属性名与值一起入哈希：不同属性下的同值字符串不构成同一个事实
#[cfg(target_os = "linux")]
pub(crate) const DEVICE_IDENTITY_ATTRS: &[&str] = &["dm/uuid", "md/uuid", "wwid", "device/wwid", "device/serial"];

/// 在目标路径后追加后缀（不适配扩展名，只做串接）：`/a/b.img` + `.bak` ⇒ `/a/b.img.bak`，
/// 与用户可见的目标名保持一一对应
pub(crate) fn suffix_path(base: &Path, suffix: &str) -> PathBuf {
    let mut p = base.to_path_buf().into_os_string();
    p.push(suffix);
    PathBuf::from(p)
}

// 落盘文件名的词表：journal / checkpoint / log / lock 四类伴随文件与 checkpoint 的
// 暂存前缀只在这里定义一次，派生点（身份构造、日志落点）与写入侧（原子写）都取自这里。
// 命名空间只在宏体里出现一次，各常量只提供自己的 stem
macro_rules! sidecar_suffix {
    ($stem:literal) => {
        concat!(".diskedit.", $stem)
    };
}

pub(crate) const JOURNAL_SUFFIX: &str = sidecar_suffix!("journal");

pub(crate) const CHECKPOINT_SUFFIX: &str = sidecar_suffix!("ckpt");

pub(crate) const LOCK_SUFFIX: &str = sidecar_suffix!("lock");

pub(crate) const LOG_SUFFIX: &str = sidecar_suffix!("log");

/// 暂存文件前缀：原子写先写它、再 rename 到正式落点（见 `crate::movepart::atomic_write_ckpt`）；
/// 打开可写目标时也按它清掉崩溃残骸
pub(crate) const CKPT_TMP_PREFIX: &str = sidecar_suffix!("ckpt.tmp.");

/// 旧版块设备现场的命名：GUID 后只有 `.ckpt`，没有命名空间中缀
pub(crate) const LEGACY_GUID_CKPT_SUFFIX: &str = ".ckpt";

/// 16 字节 Disk GUID → 大写无连字符十六进制（历史落点用的就是这种写法）
pub(crate) fn guid_hex(g: &[u8; 16]) -> String {
    g.iter().map(|b| format!("{b:02X}")).collect()
}

/// 身份键 → 文件名安全的 token：保留 ASCII 字母数字与 `.` `-` `_`，其余（含路径分隔符）
/// 换成 `_` 并截断到 32 字符，末尾附值的 CRC32——身份可能是 loop 的 backing 路径，
/// 原样落盘会带分隔符、可能超长，而截断与替换会令两个不同身份撞同一个名字。
/// 消费者是块设备身份链，非 Linux 上只剩测试需要它
#[cfg(any(target_os = "linux", test))]
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
    /// 镜像身份。canonical = 身份主体（journal / checkpoint / lock 落它旁边）；
    /// literal = 用户给的路径（base，日志命名沿用既有约定）。两者不同（路径含符号链接
    /// 或非规范化成分）时字面路径的兄弟文件降为历史候选——只用于发现，发现后照走
    /// 完整校验；两份候选同时有效即歧义拒绝，不猜
    pub(crate) fn image_at(canonical: &Path, literal: &Path) -> Self {
        let mut journal = vec![suffix_path(canonical, JOURNAL_SUFFIX)];
        let mut checkpoint = vec![suffix_path(canonical, CHECKPOINT_SUFFIX)];
        if literal != canonical {
            // 字面路径与 canonical 指同一文件时（相对路径、`./x`、`dir/../x`）两者派生的
            // 是**同一份**候选：两份都列出来会让消费侧把"只有一个现场"判成 Ambiguous 而
            // 全面拒绝。真·不同文件（符号链接、不同挂载点）才保留为双候选
            let j = suffix_path(literal, JOURNAL_SUFFIX);
            if !same_candidate_file(&journal[0], &j) {
                journal.push(j);
            }
            let c = suffix_path(literal, CHECKPOINT_SUFFIX);
            if !same_candidate_file(&checkpoint[0], &c) {
                checkpoint.push(c);
            }
        }
        Self {
            kind: TargetKind::Image,
            base: literal.to_path_buf(),
            canonical: canonical.to_path_buf(),
            journal,
            checkpoint,
            lock: suffix_path(canonical, LOCK_SUFFIX),
            legacy_guid_checkpoints: false,
        }
    }

    /// 镜像身份：Linux 上取 realpath（文件不存在——cleanup 路径的常态之一——退字面
    /// 路径），其余平台字面路径。非 Unix 上同一文件的别名路径（硬链接、符号链接、
    /// junction）因此各有一份现场与一把锁——见 README 的"并发边界"
    pub(crate) fn resolve_image(path: &Path) -> Self {
        #[cfg(target_os = "linux")]
        {
            let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
            Self::image_at(&canonical, path)
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
    /// 打不开或名字已不指向它，一律 `Err`——退到字符串会把 journal / checkpoint / lock
    /// 挂到一个与 loop 无关的文件旁
    ///
    /// 旧版身份（state_dir 下按 backing 串 token 与 devname 命名的落点）列为历史候选
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
        let mut id = crate::dev::FileSource::identity_of_opened_image(&raw, &file, &meta).map_err(|e| {
            format!("cannot bind the recovery state of {} to its backing file: {e}", dev_path.display())
        })?;
        let tok = key_token(&raw.to_string_lossy());
        id.journal.push(state_dir().join(format!("{tok}{JOURNAL_SUFFIX}")));
        id.checkpoint.push(state_dir().join(format!("{tok}{CHECKPOINT_SUFFIX}")));
        // 更旧的历史命名：块设备 journal 曾以 devname 命名
        id.journal.push(state_dir().join(format!("{}{JOURNAL_SUFFIX}", file_name_lossy(dev_path))));
        id.legacy_guid_checkpoints = true;
        Ok(id)
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
        let (self_key, disk_key) = block_keys(node, path).map_err(|e| e.to_string())?;
        Ok(Self::from_block_keys(path, self_key, disk_key))
    }

    /// 非 Linux 平台没有 sysfs，块设备判定恒 false，本入口不会到达。非 Linux 上没有块
    /// 设备的生产打开路径，此实现只服务测试的命名与锁落点断言，故随测试一同编译
    #[cfg(all(not(target_os = "linux"), test))]
    pub(crate) fn resolve_block(path: &Path) -> Result<Self, String> {
        let k = file_name_lossy(path);
        Ok(Self::from_block_keys(path, k.clone(), k))
    }

    /// 非 loop 块设备的身份构造。**盘级**：journal / checkpoint / lock 全部按所在整设备
    /// 派生——分区号属于操作，不进落盘键，`/dev/sdb:2`（整盘节点 + 后缀）与
    /// `/dev/sdb2`（分区节点）由此共享同一现场，旧版按分区落的那份降为历史候选
    #[cfg(any(target_os = "linux", test))] // 非 Linux 上块设备身份只存在于测试
    pub(crate) fn from_block_keys(path: &Path, self_key: String, disk_key: String) -> Self {
        let stable = key_token(&disk_key);
        let dir = state_dir();
        let mut journal = vec![dir.join(format!("{stable}{JOURNAL_SUFFIX}"))];
        let mut checkpoint = vec![dir.join(format!("{stable}{CHECKPOINT_SUFFIX}"))];
        if self_key != disk_key {
            // 历史命名：旧版现场按分区落（self_key 含分区号）
            let self_tok = key_token(&self_key);
            journal.push(dir.join(format!("{self_tok}{JOURNAL_SUFFIX}")));
            checkpoint.push(dir.join(format!("{self_tok}{CHECKPOINT_SUFFIX}")));
        }
        journal.push(dir.join(format!("{}{JOURNAL_SUFFIX}", file_name_lossy(path))));
        Self {
            kind: TargetKind::Block,
            base: path.to_path_buf(),
            canonical: path.to_path_buf(),
            journal,
            checkpoint,
            // 锁按**所在整设备**派生，与 journal / checkpoint 的盘级键一致：
            // 独占权针对盘（分区表属于盘），于是"离线以分区节点为目标"与"在线对同一
            // 分区"落到同一把锁上。锁文件是纯运行时 artifact（重启自清也无妨），残留
            // 不构成阻挡——判据是"锁取不取得到"，不是"文件在不在"（见 targetlock）
            lock: dir.join(format!("{stable}{LOCK_SUFFIX}")),
            legacy_guid_checkpoints: true,
        }
    }

    /// 清理类命令（abandon）的身份解析：目标可能已不存在，而现场文件可能比目标活得
    /// 久（用户删了镜像，journal 还在旁边）。路径 stat 不了时按镜像身份回退——镜像
    /// 身份就是给定的路径本身，兄弟候选的枚举不需要目标存在。节点消失的块设备身份
    /// 不可恢复（拓扑派生），其 state_dir 下的现场随之不可达：此处无从区分两者，按
    /// 镜像回退时前者得救、后者空跑——报"nothing to abandon"而不是按错身份删候选。
    /// 拓扑解析得出但解析失败的块设备仍返回 None（保守方向：宁可漏清，也不按错身份删候选）。
    /// loop 设备的身份绑定 backing 对象：backing 打不开或已不是普通文件时
    /// [`Self::loop_backed`] 返回 Err，这里随之返回 None——它留在原落点上的现场不再可达，
    /// 因为定位它所需的身份在 backing 消失后已经无法从对象本身得出
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

    /// 镜像目标的**数据文件**路径；块设备给 `None`。锁文件按 canonical path 派生，
    /// 硬链接（同一 inode 的另一条路径）因此会各拿一把锁——对数据文件自身再上一把
    /// inode 级锁才把同一份数据收敛到一把锁。块设备没有第二道凭据可加：写打开即
    /// `O_EXCL`，已覆盖同 inode 的不同路径
    #[cfg(unix)] // 消费者是 TargetLock 的 inode 级凭据，仅 unix 上取
    pub(crate) fn image_data_path(&self) -> Option<&Path> {
        matches!(self.kind, TargetKind::Image).then_some(self.canonical.as_path())
    }

    /// 再次打开**同一个数据文件**时用的路径：镜像取打开时刻由 fd 解析出的名字，
    /// 块设备取节点路径（身份本就由 fd 的 rdev 派生）
    pub(crate) fn data_path(&self) -> &Path {
        if matches!(self.kind, TargetKind::Image) {
            &self.canonical
        } else {
            &self.base
        }
    }

    pub(crate) fn journal_candidates(&self) -> &[PathBuf] {
        &self.journal
    }

    /// checkpoint 首选落点。生产路径的写/删一律走 `checkpoint_candidates` 命中的那份
    /// （见 movepart::read_checkpoint），本方法只剩测试在用
    #[cfg(test)]
    pub(crate) fn checkpoint_path(&self) -> &Path {
        &self.checkpoint[0]
    }

    /// 日志落点：镜像 = 目标路径旁的兄弟文件；块设备 = `<state_dir>/<名>` 加同一后缀，
    /// 名取可读的 GPT Disk GUID（与 checkpoint 同源），读不到表时退到 devname。
    ///
    /// 与 journal / checkpoint 的差别只有一处：日志只增、不参与恢复，因此不做候选回退，
    /// 名字也保持既有约定不变——改名会打断已写下日志的连续性。落点只在此处拼装，
    /// 调用方不得自行拼 `state_dir()`
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

    /// checkpoint 的候选落点。块设备语义下的历史落点以 GPT Disk GUID 命名，而 GUID 只
    /// 在表可读时存在，读不到就没有那一条
    pub(crate) fn checkpoint_candidates(&self, legacy_disk_guid: Option<[u8; 16]>) -> Vec<PathBuf> {
        let mut v = self.checkpoint.clone();
        if self.has_block_legacy_naming()
            && let Some(g) = legacy_disk_guid
        {
            v.push(state_dir().join(format!("{}{LEGACY_GUID_CKPT_SUFFIX}", guid_hex(&g))));
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
        let Ok(node) = sysfs_node(path) else { return true };
        // 只有"属性不存在"才是整设备；值存在（无论能否解析成数字）与读失败都按分区
        // 节点处理——判据读不出来就拒，与上面的口径一致
        !matches!(read_sysfs_attr(&node.join("partition")), Ok(None))
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = path;
        false
    }
}

/// 暂存文件算"陈旧"的岁数：一次 ckpt 写入是毫秒级，正常路径下 rename 已把它带走，
/// 超过这个岁数还留在原地的只可能是进程被 SIGKILL 打断留下的残骸
pub(crate) const CKPT_TMP_STALE_AFTER: std::time::Duration = std::time::Duration::from_secs(3600);

/// 打开**可写**目标时清掉落点目录里的陈旧 checkpoint 暂存文件。
/// 按岁数判而不看 PID：state_dir 是所有盘共用的落点，同目录可能有别的进程正在写，
/// 它那份暂存文件是活的——岁数阈值把"正在写"与"崩溃残骸"分开（取不到 mtime，含落在
/// 未来的，一律当不陈旧，宁可不删）。
/// 尽力而为：目录读不到、元数据拿不到、删不掉都不拦打开目标
#[allow(clippy::let_underscore_must_use)] // 有意忽略：删不掉就留到下次打开再清
pub(crate) fn sweep_stale_ckpt_temps(identity: &TargetIdentity) {
    let candidates = identity.checkpoint_candidates(None);
    let mut dirs: Vec<PathBuf> = candidates.iter().filter_map(|p| p.parent().map(Path::to_path_buf)).collect();
    dirs.sort();
    dirs.dedup();
    for dir in dirs {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            if !entry.file_name().to_string_lossy().starts_with(CKPT_TMP_PREFIX) {
                continue;
            }
            let stale = entry
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.elapsed().ok())
                .is_some_and(|age| age >= CKPT_TMP_STALE_AFTER);
            if stale {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
}