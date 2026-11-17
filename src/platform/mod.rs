//! 平台端口：目标探测与身份、打开、扇区来源、恢复文件访问、目录持久化。
//!
//! 本层只回答"平台事实"，不认识 Plan、Request、恢复动作，也不做安全策略。
//! 身份由平台探测产生：路径只出现在 [`TargetSpec`] 的输入文本里，不参与
//! [`TargetId`] 的恒等比较。
//!
//! 端口以 sealed trait 表达：外部 crate 无法为自己的类型实现 [`Platform`] /
//! [`EvidenceStore`]；实现只在本 crate 的平台后端模块内。
//!
//! 探测（`probe::probe_of` / `probe::HostPlatform::identify`）与只读打开
//! （`probe::open_read_only_source`）已在命令层使用；证据目录端口只有 Linux 后端
//! 具备原语。

#![cfg_attr(not(target_os = "linux"), allow(dead_code))]

use std::path::{Component, Path, PathBuf};

use crate::domain::SectorSize;

pub(crate) mod capabilities;
pub(crate) mod probe;
#[cfg(target_os = "linux")]
pub(crate) mod evidence_store;
#[cfg(test)]
pub(crate) mod fake;

pub(crate) use capabilities::{CapabilityId, CapabilitySnapshot, CapabilityState};

mod sealed {
    /// 端口的私有 supertrait：外部 crate 无法命名它，因而无法实现该端口
    pub(crate) trait Sealed {}
}

/// 用户输入的目标描述：路径文本 + 可选的扇区大小覆盖。
/// 它还不是任何具体设备，不能被当作身份
pub(crate) struct TargetSpec {
    path: PathBuf,
    sector_override: Option<SectorSize>,
}

impl TargetSpec {
    pub(crate) fn new(path: impl Into<PathBuf>, sector_override: Option<SectorSize>) -> Self {
        Self { path: path.into(), sector_override }
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn sector_override(&self) -> Option<SectorSize> {
        self.sector_override
    }
}

/// 稳定身份的构成材料。路径不在其中：同一文件改名后仍是同一目标，
/// 不同文件即使路径相同（替换、重新创建）也不是
#[derive(Clone, Debug)]
pub(crate) enum IdentityMaterial {
    /// 块设备：整盘设备号 + 平台给出的稳定设备 ID（序列号 / WWN / DM uuid 等）。
    /// 设备号由内核分配，重启或热插拔后可被复用：双方都有 `device_id` 时只比它，
    /// 设备号不参与；双方都没有稳定 ID 时才退回设备号（这样的材料不可跨恢复现场，
    /// 由 [`crate::recovery::SceneProof::Unavailable`] 拦住）；一方有一方没有即不同目标
    Block { major: u32, minor: u32, device_id: Option<String> },
    /// 镜像：不透明文件 ID（Unix 的 (st_dev, st_ino)、Windows 的卷序列号 + 文件索引）。
    /// 复制或移动会得到新的文件 ID ⇒ 新目标；卷序列号是可被工具重写的画像字段，
    /// 不参与恒等比较
    Image { volume_serial: Option<u32>, file_id: [u8; 16] },
}

/// 设备身份值的命名空间。身份是（命名空间, 值）：值自带前缀时前缀就是命名空间，
/// 属性名只是它的读取位置，同名同值经 `wwid` 与 `device/wwid` 读到仍是同一身份。
/// 值没有前缀时命名空间只能由属性名给出——裸值本身不自述身份
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Namespace {
    /// `naa.`：SCSI VPD 页 0x83 的 NAA 设计符
    Naa,
    /// `eui.`：EUI-64 设计符
    Eui,
    /// `t10.`：T10 vendor-ID 设计符
    T10,
    /// `uuid.`：UUID 设计符
    Uuid,
    /// `mpath-`：DM multipath 的 `dm/uuid`
    Mpath,
    /// 裸值，来自 serial 属性
    Serial,
    /// 裸值，来自 wwid 属性
    Wwid,
    /// 裸值，属性名未给出命名空间
    BareValue,
}

/// multipath-tools 写进 `dm/uuid` 的前缀：`libmultipath/devmapper.h` 的 `UUID_PREFIX`，
/// 创建映射时按 `UUID_PREFIX "%s"` + `mpp->wwid` 写入。该工具自身按这 6 个字节**区分
/// 大小写**比较；本仓库对它的判定同样区分大小写——`mpath-` 归并是放宽（会压掉
/// Ambiguous），未知大小写形态按"不是 mpath"处理。识别与命名空间分类共用这一处
pub(crate) const MPATH_UUID_PREFIX: &str = "mpath-";

impl Namespace {
    /// 值自带前缀即按值定命名空间；否则按属性名。前缀比较不区分大小写（`mpath-` 除外，
    /// 见 [`MPATH_UUID_PREFIX`]），值本身原样参与比较（大小写归一需要设备侧约定，
    /// 本层不做假设）
    fn of(attr: &str, value: &str) -> Self {
        if value.starts_with(MPATH_UUID_PREFIX) {
            return Namespace::Mpath;
        }
        for (prefix, ns) in [
            ("naa.", Namespace::Naa),
            ("eui.", Namespace::Eui),
            ("t10.", Namespace::T10),
            ("uuid.", Namespace::Uuid),
        ] {
            if value.len() >= prefix.len() && value.as_bytes()[..prefix.len()].eq_ignore_ascii_case(prefix.as_bytes()) {
                return ns;
            }
        }
        let attr_tail = attr.rsplit('/').next().unwrap_or(attr);
        if attr_tail.eq_ignore_ascii_case("serial") {
            Namespace::Serial
        } else if attr_tail.eq_ignore_ascii_case("wwid") {
            Namespace::Wwid
        } else {
            Namespace::BareValue
        }
    }
}

/// 一条设备身份声明：`属性名=值` 原串拆出的（命名空间, 值）。
/// 值复制出来：判定要在记录载荷与 `/sys/block` 枚举结果之间做，后者的原串活不过这次比较
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Claim {
    ns: Namespace,
    value: String,
}

impl Claim {
    fn namespace_label(&self) -> &'static str {
        match self.ns {
            Namespace::Naa => "naa-designator",
            Namespace::Eui => "eui-64",
            Namespace::T10 => "t10-designator",
            Namespace::Uuid => "uuid",
            Namespace::Mpath => "multipath-wwid",
            Namespace::Serial => "serial",
            Namespace::Wwid => "wwid",
            Namespace::BareValue => "value",
        }
    }
}

/// 拒绝信息里的写法：命名空间在前，值原样
impl std::fmt::Display for Claim {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} {}", self.namespace_label(), self.value)
    }
}

/// 全零设计符：`0x` 前缀与 `naa.` / `eui.` / `uuid.` 命名空间前缀、UUID 的分隔连字符
/// 都不改变"值是全零"。`t10.` 是 ASCII 供应商串，不按十六进制处理
pub(crate) fn is_zero_designator(value: &str) -> bool {
    let mut v = value;
    if let Some(rest) = v.strip_prefix("0x").or_else(|| v.strip_prefix("0X")) {
        v = rest;
    }
    for prefix in ["naa.", "eui.", "uuid."] {
        if v.len() > prefix.len() && v.as_bytes()[..prefix.len()].eq_ignore_ascii_case(prefix.as_bytes()) {
            v = &v[prefix.len()..];
            break;
        }
    }
    !v.is_empty() && v.bytes().all(|b| b == b'0' || b == b'-')
}

/// 只有设计符前缀、没有载荷（`t10.`、`naa.`）：与全零同义，设备没有给出身份
pub(crate) fn is_bare_designator(value: &str) -> bool {
    ["t10.", "naa.", "eui.", "uuid."].iter().any(|prefix| value.eq_ignore_ascii_case(prefix))
}

/// 把 `属性名=值` 原串拆成一条声明。空白、空串、全零设计符（见 [`is_zero_designator`]）
/// 都表示"设备没有给出身份"，不构成声明。没有 `=` 的原串按裸值处理，属性名为空。
///
/// `t10.` 设计符是定长 ASCII 字段，尾部以空格填满；比较前去掉这些填充空格，去掉后
/// 没有内容即"没有给出身份"
pub(crate) fn claim_of(raw: &str) -> Option<Claim> {
    let (attr, value) = match raw.split_once('=') {
        Some((attr, value)) => (attr.trim(), value.trim()),
        None => ("", raw.trim()),
    };
    let value = value.trim_end_matches(' ');
    if value.is_empty() || is_zero_designator(value) || is_bare_designator(value) {
        return None;
    }
    Some(Claim { ns: Namespace::of(attr, value), value: value.to_string() })
}

impl PartialEq for IdentityMaterial {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (
                Self::Block { major: amaj, minor: amin, device_id: aid },
                Self::Block { major: bmaj, minor: bmin, device_id: bid },
            ) => match (aid.as_deref().and_then(claim_of), bid.as_deref().and_then(claim_of)) {
                (Some(a), Some(b)) => a == b,
                (None, None) => (amaj, amin) == (bmaj, bmin),
                _ => false,
            },
            (Self::Image { file_id: a, .. }, Self::Image { file_id: b, .. }) => a == b,
            _ => false,
        }
    }
}

impl Eq for IdentityMaterial {}

/// 设备 ID 字符串是否构成事实：解析不出声明的原串（空白、空串、全零）不算，
/// 原串去空白后保留——记录里存的是 `属性名=值` 的原样文本
fn device_id_is_fact(raw: &str) -> Option<String> {
    claim_of(raw).map(|_| raw.trim().to_string())
}

impl IdentityMaterial {
    /// 块设备材料：`raw_device_id` 是平台读到的 `属性名=值` 原串（可为 None）。
    /// 没有事实性设备 ID 时保留设备号
    pub(crate) fn block(major: u32, minor: u32, raw_device_id: Option<&str>) -> Self {
        Self::Block {
            major,
            minor,
            device_id: raw_device_id.and_then(device_id_is_fact),
        }
    }

    /// 镜像材料：文件 ID 缺项时拒绝识别——卷序列号不参与比较，单独它不足以构成身份
    pub(crate) fn image(
        volume_serial: Option<u32>,
        file_id: Option<[u8; 16]>,
    ) -> Result<Self, PlatformError> {
        match file_id {
            Some(file_id) => Ok(Self::Image { volume_serial, file_id }),
            None => Err(PlatformError::Unidentifiable(
                "the platform returned no stable file ID for this image — a path is not an identity"
                    .to_string(),
            )),
        }
    }
}

/// 目标身份。唯一构造点是平台探测（[`Platform::identify`]），字段私有；
/// `Eq` 只比较身份材料
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TargetId(IdentityMaterial);

impl TargetId {
    /// 平台后端产出身份的唯一入口
    pub(crate) fn from_material(material: IdentityMaterial) -> Self {
        Self(material)
    }

    pub(crate) fn material(&self) -> &IdentityMaterial {
        &self.0
    }
}

#[derive(Clone, Debug)]
pub(crate) struct TargetProbe {
    id: TargetId,
    total_bytes: u64,
}

impl TargetProbe {
    /// 平台后端产出探测结果的唯一入口
    pub(crate) fn new(id: TargetId, total_bytes: u64) -> Self {
        Self { id, total_bytes }
    }

    pub(crate) fn id(&self) -> &TargetId {
        &self.id
    }

    pub(crate) fn total_bytes(&self) -> u64 {
        self.total_bytes
    }
}

/// 平台端口。实现只在本 crate 的平台后端模块内（sealed）
pub(crate) trait Platform: sealed::Sealed {
    /// 探测身份与几何；不打开写路径
    fn identify(&self, spec: &TargetSpec) -> Result<TargetProbe, PlatformError>;
}

/// 恢复目录的锚点。Linux 上是已打开的目录 FD：父目录链从它开始逐级解析；
/// 其他平台没有等价原语，类型不可构造，依赖它的写操作只能拒绝
#[cfg(target_os = "linux")]
pub(crate) struct DirFd(std::os::fd::OwnedFd);

#[cfg(target_os = "linux")]
impl DirFd {
    pub(crate) fn as_raw(&self) -> std::os::fd::RawFd {
        use std::os::fd::AsRawFd;
        self.0.as_raw_fd()
    }

    /// 锚点的独立句柄：解析父目录链时每一级都要有自己的所有权
    pub(crate) fn try_clone(&self) -> std::io::Result<std::os::fd::OwnedFd> {
        use std::os::fd::{FromRawFd, OwnedFd};
        // SAFETY: `self.0` 在整个调用期间持有打开的描述符，`as_raw()` 取到的 fd 因此有效；
        // `F_DUPFD_CLOEXEC` 只读描述符编号并分配一个新 fd，不读写调用方内存
        let raw = unsafe { libc::fcntl(self.as_raw(), libc::F_DUPFD_CLOEXEC, 0) };
        if raw < 0 {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: `raw` 是上面刚由 `fcntl` 返回的非负 fd，此前没有别的所有者，交给
        // `OwnedFd` 独占
        Ok(unsafe { OwnedFd::from_raw_fd(raw) })
    }

    /// 打开一个已存在的目录作为可信锚点：按路径直接 open，锚点本身不逐级校验分量
    pub(crate) fn open_anchor(path: &Path) -> std::io::Result<Self> {
        use std::os::fd::{FromRawFd, OwnedFd};
        use std::os::unix::ffi::OsStrExt;
        let name = std::ffi::CString::new(path.as_os_str().as_bytes())
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "the anchor path contains NUL"))?;
        // SAFETY: `name.as_ptr()` 指向的 `CString` 在整个调用期间存活、以 NUL 结尾；
        // 标志是 O_RDONLY | O_DIRECTORY | O_CLOEXEC，不含 O_CREAT / O_TMPFILE，因此
        // `open` 的可变参数（mode）不会被读取
        let raw = unsafe {
            libc::open(
                name.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
            )
        };
        if raw < 0 {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: `raw` 是上面刚由 `open` 返回的非负 fd，此前没有别的所有者，交给
        // `OwnedFd` 独占
        Ok(Self(unsafe { OwnedFd::from_raw_fd(raw) }))
    }
}

#[cfg(not(target_os = "linux"))]
pub(crate) enum DirFd {}

/// 相对锚点的证据路径：拒绝绝对路径、父目录分量与空路径。
/// 调用方不能自行拼接绝对路径
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RelativePath(PathBuf);

impl RelativePath {
    pub(crate) fn try_new(path: impl Into<PathBuf>) -> Result<Self, PathError> {
        let path = path.into();
        if path.as_os_str().is_empty() {
            return Err(PathError::Empty);
        }
        if path.is_absolute() || path.has_root() {
            return Err(PathError::Absolute);
        }
        for c in path.components() {
            match c {
                Component::Normal(_) => {}
                Component::CurDir => {}
                _ => return Err(PathError::EscapesAnchor),
            }
        }
        Ok(Self(path))
    }

    pub(crate) fn as_path(&self) -> &Path {
        &self.0
    }
}

/// 已按锚点解析、仍由后端持有的证据文件。Linux 上同时持有父目录 FD、文件名与
/// 只读 FD；删除与改名都复用这两个句柄，不按路径二次解析
pub(crate) struct EvidenceFile {
    relative: RelativePath,
    #[cfg(target_os = "linux")]
    state: evidence_store::OpenedFile,
}

impl EvidenceFile {
    /// 后端在某级解析通过后构造
    #[cfg(target_os = "linux")]
    pub(crate) fn from_linux(relative: RelativePath, state: evidence_store::OpenedFile) -> Self {
        Self { relative, state }
    }

    pub(crate) fn relative(&self) -> &RelativePath {
        &self.relative
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn state(&self) -> &evidence_store::OpenedFile {
        &self.state
    }
}

/// 目录项持久化结论：调用返回成功时目录项的创建/删除/改名已随目录 `fsync` 落盘
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DirDurability {
    Persisted,
    /// 目录项已改变、目录 `fsync` 失败：名字空间里的变化可能随掉电回退
    Unconfirmed,
}

/// 证据路径解析失败：每一类都对应一次显式拒绝
#[derive(Debug)]
pub(crate) enum PathError {
    Empty,
    Absolute,
    EscapesAnchor,
    Symlink,
    NotFound,
    /// 请求的最终分量是目录
    IsDirectory,
    /// 中间分量不是目录
    NotDirectory,
    /// 最终分量不是普通文件
    NotAFile,
    Inaccessible(String),
}

#[derive(Debug)]
pub(crate) enum EvidenceIo {
    Io(std::io::Error),
    /// 改名/创建已发生、目录 `fsync` 失败：目录项已指向新记录，掉电后是否保留未确认
    RenamedNotDurable(std::io::Error),
    Unsupported(String),
}

/// 删除的落点：`unlink` 是否发生与目录项是否持久化分开陈述。
/// 下游按变体分流，不从 I/O 错误反推"记录还在"
#[derive(Debug)]
pub(crate) enum RemoveOutcome {
    /// 名字空间已移除，目录 `fsync` 成功
    RemovedAndDurable,
    /// 名字空间已移除，目录 `fsync` 失败：掉电后可能重新出现
    RemovedDurabilityUnconfirmed,
    /// `unlink` 未发生：记录仍在落点上
    NotRemoved(EvidenceIo),
}

/// 恢复记录存储端口：整份原子替换 + 目录持久化。
/// 实现只在本 crate 的平台后端模块内（sealed）
pub(crate) trait EvidenceStore: sealed::Sealed {
    /// 从锚点逐级 `O_NOFOLLOW` 解析相对路径
    fn open_at(&self, anchor: &DirFd, relative: &RelativePath) -> Result<EvidenceFile, PathError>;

    /// 读取已解析文件的全部字节
    fn read(&self, file: &EvidenceFile) -> Result<Vec<u8>, EvidenceIo>;

    /// 临时文件写完整份内容 → 改名 → 目录 fsync；`temp` 必须与 `file` 同目录
    fn replace_atomic(
        &self,
        anchor: &DirFd,
        file: &EvidenceFile,
        temp: &RelativePath,
        bytes: &[u8],
    ) -> Result<DirDurability, EvidenceIo>;

    /// 删除已持有的证据文件并再次目录 fsync。两个凭据都由恢复引擎签发，
    /// 本原语不接受缺任一凭据的调用。返回三态：删除是否发生与目录项是否持久化
    /// 分开陈述，下游不从 [`EvidenceIo`] 反推"记录还在"
    fn remove(
        &self,
        anchor: &DirFd,
        file: EvidenceFile,
        committed: &crate::recovery::CommittedRecord,
        permit: &crate::recovery::CleanupPermit,
    ) -> RemoveOutcome;
}

/// 平台探测/打开失败。分类在这里不压成 io：成因决定上层是拒绝还是能力不支持
#[derive(Debug)]
pub(crate) enum PlatformError {
    Io(std::io::Error),
    /// 目标拿不到稳定身份证据：路径相同不构成同一目标
    Unidentifiable(String),
    /// 平台没有该能力
    Unsupported(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 身份只由材料决定：同一材料相等，设备 ID 不同即不同目标；
    /// 类型里没有路径字段，改名不会改变恒等比较
    #[test]
    fn target_id_compares_identity_material_only() {
        let a = TargetId::from_material(IdentityMaterial::block(8, 16, Some("device/serial=S1")));
        let b = TargetId::from_material(IdentityMaterial::block(8, 16, Some("device/serial=S1")));
        assert_eq!(a, b);
        let other = TargetId::from_material(IdentityMaterial::block(8, 16, Some("device/serial=S2")));
        assert_ne!(a, other);
    }

    /// serial 为空 / 空白 / 全零，wwn 又不存在时，设备号之外没有身份材料：
    /// 这样的材料与不带设备 ID 的材料相等；设备自己声明的任一稳定 ID 则不同。
    /// 命名空间前缀与 UUID 连字符不改变全零的判定
    #[test]
    fn a_falsy_device_id_is_no_identity_evidence() {
        for raw in [
            None,
            Some(""),
            Some("   "),
            Some("device/serial=0000000000000000"),
            Some("wwid=0x0000000000000000"),
            Some("wwid=naa.0000000000000000"),
            Some("dm/uuid=uuid.00000000-0000-0000-0000-000000000000"),
        ] {
            assert_eq!(
                IdentityMaterial::block(8, 16, raw),
                IdentityMaterial::block(8, 16, None),
                "raw={raw:?}"
            );
        }
        assert_ne!(
            IdentityMaterial::block(8, 16, Some("device/serial=S1")),
            IdentityMaterial::block(8, 16, None)
        );
        assert_ne!(
            IdentityMaterial::block(8, 16, Some("wwid=0x5000c500a1b2c3d4")),
            IdentityMaterial::block(8, 16, None)
        );
        assert_ne!(
            IdentityMaterial::block(
                8,
                16,
                Some("dm/uuid=uuid.12345678-1234-1234-1234-123456789abc")
            ),
            IdentityMaterial::block(8, 16, None)
        );
    }

    /// 稳定设备 ID 是块设备唯一的跨现场身份：设备号变了（重启/热插拔后重新分配）
    /// 仍是同一目标；双方都没有稳定 ID 时才退回设备号比较；一方有一方没有即不同目标
    #[test]
    fn a_stable_device_id_outranks_the_kernel_device_numbers() {
        let with_id = |maj, min, raw| IdentityMaterial::block(maj, min, Some(raw));
        assert_eq!(with_id(8, 16, "serial=S1"), with_id(8, 32, "serial=S1"));
        assert_ne!(with_id(8, 16, "serial=S1"), with_id(8, 16, "serial=S2"));

        assert_eq!(
            IdentityMaterial::block(8, 16, None),
            IdentityMaterial::block(8, 16, None)
        );
        assert_ne!(
            IdentityMaterial::block(8, 16, None),
            IdentityMaterial::block(8, 32, None)
        );

        // 一侧有稳定 ID、一侧只有设备号：无法证明是同一目标
        assert_ne!(
            with_id(8, 16, "serial=S1"),
            IdentityMaterial::block(8, 16, None)
        );
    }

    /// 身份是（命名空间, 值）：同一个值从不同属性读到仍是同一身份。
    /// 值自带前缀时前缀就是命名空间；裸值没有自述，只能靠属性名区分
    #[test]
    fn an_identity_is_its_namespace_and_value_not_the_attribute_that_carried_it() {
        let block = |raw| IdentityMaterial::block(8, 16, Some(raw));
        assert_eq!(block("wwid=naa.5000c500aabbccdd"), block("device/wwid=naa.5000c500aabbccdd"));
        assert_eq!(block("wwid=eui.0025385812345678"), block("device/wwid=eui.0025385812345678"));
        // 裸值不自述命名空间：serial 与 wwid 下的同值字符串不是同一条声明
        assert_ne!(block("device/serial=SN123456"), block("device/wwid=SN123456"));
        assert_eq!(block("device/serial=SN123456"), block("serial=SN123456"));
        // 命名空间不同即不同身份，即便值看起来一样
        assert_ne!(block("wwid=naa.5000c500aabbccdd"), block("dm/uuid=mpath-5000c500aabbccdd"));
        assert_ne!(block("wwid=eui.0025385812345678"), block("wwid=naa.0025385812345678"));
    }

    /// 镜像拿不到文件 ID 时拒绝识别：卷序列号不参与比较，单独它不足以构成身份
    #[test]
    fn image_identity_without_file_id_is_refused() {
        assert!(matches!(
            IdentityMaterial::image(Some(7), None),
            Err(PlatformError::Unidentifiable(_))
        ));
        assert!(IdentityMaterial::image(None, Some([0; 16])).is_ok());
    }

    /// 复制或移动得到新的文件 ID ⇒ 新目标。旧现场按"身份不符"处理，
    /// 而不是"同一目标的数据已变"
    #[test]
    fn copied_image_is_a_new_target() {
        let original = TargetId::from_material(IdentityMaterial::image(None, Some([1; 16])).unwrap());
        let copied = TargetId::from_material(IdentityMaterial::image(None, Some([2; 16])).unwrap());
        assert_ne!(original, copied);
    }

    /// 卷序列号被工具重写不改变目标身份：文件 ID 相同即同一文件
    #[test]
    fn volume_serial_rewrite_does_not_change_identity() {
        let before = TargetId::from_material(IdentityMaterial::image(Some(7), Some([3; 16])).unwrap());
        let after = TargetId::from_material(IdentityMaterial::image(Some(9), Some([3; 16])).unwrap());
        assert_eq!(before, after);
    }

    #[test]
    fn probe_carries_identity_and_capacity() {
        let material = IdentityMaterial::image(None, Some([1; 16])).unwrap();
        let probe = TargetProbe::new(TargetId::from_material(material.clone()), 8 * 1024 * 1024);
        assert_eq!(probe.id().material(), &material);
        assert_eq!(probe.total_bytes(), 8 * 1024 * 1024);
    }

    /// 证据路径只接受相对锚点的分量序列
    #[test]
    fn relative_path_rejects_escape_forms() {
        assert!(RelativePath::try_new("disk.img.diskedit.journal").is_ok());
        assert!(RelativePath::try_new("sub/file").is_ok());
        assert!(matches!(RelativePath::try_new(""), Err(PathError::Empty)));
        assert!(matches!(
            RelativePath::try_new(std::path::MAIN_SEPARATOR.to_string() + "abs"),
            Err(PathError::Absolute)
        ));
        assert!(matches!(RelativePath::try_new("a/../b"), Err(PathError::EscapesAnchor)));
        assert!(matches!(RelativePath::try_new(".."), Err(PathError::EscapesAnchor)));
    }
}
