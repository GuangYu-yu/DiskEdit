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
    /// 设备号由内核分配，重启或热插拔后可被复用，因此**只有** `device_id` 存在时
    /// 才够格跨恢复现场
    Block { major: u32, minor: u32, device_id: Option<String> },
    /// 镜像：不透明文件 ID（Unix 的 (st_dev, st_ino)、Windows 的卷序列号 + 文件索引）。
    /// 复制或移动会得到新的文件 ID ⇒ 新目标；卷序列号是可被工具重写的画像字段，
    /// 不参与恒等比较
    Image { volume_serial: Option<u32>, file_id: [u8; 16] },
}

impl PartialEq for IdentityMaterial {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (
                Self::Block { major: amaj, minor: amin, device_id: aid },
                Self::Block { major: bmaj, minor: bmin, device_id: bid },
            ) => (amaj, amin, aid) == (bmaj, bmin, bid),
            (Self::Image { file_id: a, .. }, Self::Image { file_id: b, .. }) => a == b,
            _ => false,
        }
    }
}

impl Eq for IdentityMaterial {}

/// 设备 ID 字符串是否构成事实：空白、空串、全零（含 `0x` 前缀的十六进制全零）
/// 都表示"设备没有给出身份"，不能当成一个可比较的值
fn device_id_is_fact(raw: &str) -> Option<String> {
    let value = raw.split_once('=').map_or(raw, |(_, v)| v).trim();
    if value.is_empty() {
        return None;
    }
    let digits = value.strip_prefix("0x").or_else(|| value.strip_prefix("0X")).unwrap_or(value);
    if digits.chars().all(|c| c == '0') {
        return None;
    }
    Some(raw.trim().to_string())
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

/// 探测结果：身份 + 目标容量
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
        let raw = unsafe { libc::fcntl(self.as_raw(), libc::F_DUPFD_CLOEXEC, 0) };
        if raw < 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(unsafe { OwnedFd::from_raw_fd(raw) })
    }

    /// 打开一个已存在的目录作为可信锚点（不做符号链接解析）
    pub(crate) fn open_anchor(path: &Path) -> std::io::Result<Self> {
        use std::os::fd::{FromRawFd, OwnedFd};
        use std::os::unix::ffi::OsStrExt;
        let name = std::ffi::CString::new(path.as_os_str().as_bytes())
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "the anchor path contains NUL"))?;
        let raw = unsafe {
            libc::open(
                name.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
            )
        };
        if raw < 0 {
            return Err(std::io::Error::last_os_error());
        }
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
/// 只读 FD；删除与改名都复用这两个句柄，不再按路径二次解析
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
}

/// 证据路径解析失败：每一类都来自 §4.7 的一次显式拒绝
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

/// 证据读写的失败形态
#[derive(Debug)]
pub(crate) enum EvidenceIo {
    Io(std::io::Error),
    Unsupported(String),
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
    /// 本原语不接受缺任一凭据的调用
    fn remove(
        &self,
        anchor: &DirFd,
        file: EvidenceFile,
        committed: &crate::recovery::CommittedRecord,
        permit: &crate::recovery::CleanupPermit,
    ) -> Result<DirDurability, EvidenceIo>;
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
    /// 这样的材料与不带设备 ID 的材料相等；设备自己声明的任一稳定 ID 则不同
    #[test]
    fn a_falsy_device_id_is_no_identity_evidence() {
        for raw in [None, Some(""), Some("   "), Some("device/serial=0000000000000000"), Some("wwid=0x0000000000000000")] {
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

    /// 探测结果携带身份与容量
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
