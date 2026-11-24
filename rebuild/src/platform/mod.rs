//! 平台端口：中立类型、语义化错误与工厂。
//!
//! 本文件不出现具体操作系统接口、系统路径或外部程序名。`decide` 只读这里的
//! [`Occupancy`] 与 [`Partition`]，`execute` 只经 [`TargetHandle`] 与 [`Platform`]
//! 落盘，`report` 只读 [`PlatformError::io_kind`] 的语义分类；任何"这里该跑哪个
//! 命令、该读哪个虚拟文件"的判断都留在 `linux` 子树里。
//!
//! 实现按操作系统分目录：当前只有 `linux`；新增平台时补同形状的子树并扩展
//! [`open`]，上层不需要改动。

use std::fmt;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use diskedit_core::fingerprint::Fingerprint;
use diskedit_core::fs::{FsKind, Program};

#[cfg(target_os = "linux")]
pub mod linux;
#[cfg(target_os = "macos")]
pub mod macos;
#[cfg(target_os = "windows")]
pub mod windows;

pub mod probe;

/// 为尚未接入的宿主生成显式"不支持"实现。
///
/// 每个方法都返回 [`PlatformError::NotSupported`]，不返回空数据、不假装成功；
/// 接入某个端口时在对应子树里替换该方法即可。
#[cfg(any(target_os = "windows", target_os = "macos", test))]
macro_rules! unsupported_platform {
    ($ty:ty, $os:literal) => {
        impl $crate::platform::Platform for $ty {
            fn open_target(
                &self,
                _path: &std::path::Path,
            ) -> $crate::platform::PlatformResult<Box<dyn $crate::platform::TargetHandle>> {
                Err($crate::platform::PlatformError::not_supported(concat!(
                    $os,
                    " 的块设备端口尚未接入"
                )))
            }

            fn open_target_read_only(
                &self,
                _path: &std::path::Path,
            ) -> $crate::platform::PlatformResult<Box<dyn $crate::platform::TargetHandle>> {
                Err($crate::platform::PlatformError::not_supported(concat!(
                    $os,
                    " 的只读目标端口尚未接入"
                )))
            }

            fn fingerprint(
                &self,
                _path: &std::path::Path,
            ) -> $crate::platform::PlatformResult<diskedit_core::fingerprint::Fingerprint> {
                Err($crate::platform::PlatformError::not_supported(concat!(
                    $os,
                    " 的身份端口尚未接入"
                )))
            }

            fn partitions(
                &self,
                _path: &std::path::Path,
            ) -> $crate::platform::PlatformResult<Vec<$crate::platform::Partition>> {
                Err($crate::platform::PlatformError::not_supported(concat!(
                    $os,
                    " 的拓扑端口尚未接入"
                )))
            }

            fn occupancy(
                &self,
                _path: &std::path::Path,
            ) -> $crate::platform::PlatformResult<$crate::platform::Occupancy> {
                Err($crate::platform::PlatformError::not_supported(concat!(
                    $os,
                    " 的占用端口尚未接入"
                )))
            }

            fn reread(&self, _path: &std::path::Path) -> $crate::platform::PlatformResult<()> {
                Err($crate::platform::PlatformError::not_supported(concat!(
                    $os,
                    " 不支持分区表重读"
                )))
            }

            fn update_table(
                &self,
                _path: &std::path::Path,
            ) -> $crate::platform::PlatformResult<()> {
                Err($crate::platform::PlatformError::not_supported(concat!(
                    $os,
                    " 不支持在线更新分区表"
                )))
            }

            fn btrfs_show(
                &self,
                _path: &std::path::Path,
            ) -> $crate::platform::PlatformResult<$crate::platform::BtrfsShow> {
                Err($crate::platform::PlatformError::not_supported(concat!(
                    $os,
                    " 不支持读取 btrfs 信息"
                )))
            }

            fn pvs(
                &self,
                _path: &std::path::Path,
            ) -> $crate::platform::PlatformResult<Vec<$crate::platform::PvFacts>> {
                Err($crate::platform::PlatformError::not_supported(concat!(
                    $os,
                    " 不支持读取物理卷"
                )))
            }

            fn loop_attach(
                &self,
                _backing: &std::path::Path,
                _offset: u64,
                _size: u64,
            ) -> $crate::platform::PlatformResult<std::path::PathBuf> {
                Err($crate::platform::PlatformError::not_supported(concat!(
                    $os,
                    " 没有 loop 设备"
                )))
            }

            fn loop_detach(
                &self,
                _device: &std::path::Path,
            ) -> $crate::platform::PlatformResult<()> {
                Err($crate::platform::PlatformError::not_supported(concat!(
                    $os,
                    " 没有 loop 设备"
                )))
            }

            fn mount(
                &self,
                _source: &std::path::Path,
                _mount_point: &std::path::Path,
                _fstype: Option<&str>,
                _options: &[&str],
            ) -> $crate::platform::PlatformResult<()> {
                Err($crate::platform::PlatformError::not_supported(concat!(
                    $os,
                    " 的挂载端口尚未接入"
                )))
            }

            fn umount(
                &self,
                _mount_point: &std::path::Path,
            ) -> $crate::platform::PlatformResult<()> {
                Err($crate::platform::PlatformError::not_supported(concat!(
                    $os,
                    " 的挂载端口尚未接入"
                )))
            }

            fn require(
                &self,
                program: diskedit_core::fs::Program,
            ) -> $crate::platform::PlatformResult<std::path::PathBuf> {
                Err($crate::platform::PlatformError::not_supported(format!(
                    concat!($os, " 不提供外部程序：{:?}"),
                    program
                )))
            }

            fn run(
                &self,
                _program: diskedit_core::fs::Program,
                _args: &[&str],
            ) -> $crate::platform::PlatformResult<$crate::platform::Output> {
                Err($crate::platform::PlatformError::not_supported(concat!(
                    $os,
                    " 不提供外部程序执行"
                )))
            }
        }
    };
}

#[cfg(any(target_os = "windows", target_os = "macos"))]
pub(crate) use unsupported_platform;

/// 平台层的字节区间视图：起点与长度都以字节计。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Region {
    pub offset: u64,
    pub len: u64,
}

impl Region {
    pub const fn new(offset: u64, len: u64) -> Self {
        Self { offset, len }
    }

    pub const fn end(self) -> u64 {
        self.offset.saturating_add(self.len)
    }

    pub const fn is_empty(self) -> bool {
        self.len == 0
    }
}

/// 文件交互失败的语义分类。
///
/// 上层按这些类别决定文案与退出码，不接触具体错误码。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IoKind {
    NotFound,
    Permission,
    Busy,
    Unsupported,
    Other,
}

/// 平台层错误。
#[derive(Debug)]
pub enum PlatformError {
    /// 与文件、设备或虚拟文件系统的交互失败。
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    /// 目标存在，但本实现拒绝执行该动作；环境不适用也走这一出口。
    NotSupported { why: String },
    /// 目标上的文件系统不在支持矩阵内。
    UnsupportedFs { why: String },
    /// 目标被占用或被锁住。
    Busy { why: String },
    /// 权限不足。
    Denied { why: String },
    /// 读到的文本或结构不符合预期。
    Parse { what: String },
    /// 外部程序缺失、启动失败或拒绝执行。
    Tools { program: String, why: String },
}

impl PlatformError {
    pub fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Self::Io {
            path: path.into(),
            source,
        }
    }

    pub fn not_supported(why: impl Into<String>) -> Self {
        Self::NotSupported { why: why.into() }
    }

    pub fn unsupported_fs(why: impl Into<String>) -> Self {
        Self::UnsupportedFs { why: why.into() }
    }

    /// 当前环境不适用与该能力不支持走同一出口。
    pub fn not_applicable(why: impl Into<String>) -> Self {
        Self::NotSupported { why: why.into() }
    }

    /// 能力不支持的简写。
    pub fn unsupported(why: impl Into<String>) -> Self {
        Self::NotSupported { why: why.into() }
    }

    pub fn busy(why: impl Into<String>) -> Self {
        Self::Busy { why: why.into() }
    }

    pub fn denied(why: impl Into<String>) -> Self {
        Self::Denied { why: why.into() }
    }

    pub fn parse(what: impl Into<String>) -> Self {
        Self::Parse { what: what.into() }
    }

    pub fn tools(program: impl Into<String>, why: impl Into<String>) -> Self {
        Self::Tools {
            program: program.into(),
            why: why.into(),
        }
    }

    /// 文件错误的语义分类；非文件错误为 `None`。
    pub fn io_kind(&self) -> Option<IoKind> {
        match self {
            Self::Io { source, .. } => Some(classify_io(source.kind())),
            Self::NotSupported { .. }
            | Self::UnsupportedFs { .. }
            | Self::Busy { .. }
            | Self::Denied { .. }
            | Self::Parse { .. }
            | Self::Tools { .. } => None,
        }
    }
}

fn classify_io(kind: std::io::ErrorKind) -> IoKind {
    // 外部枚举：未单列的错误统一归为其他，避免逐个跟进标准库新增变体。
    #[allow(clippy::wildcard_enum_match_arm)]
    match kind {
        std::io::ErrorKind::NotFound => IoKind::NotFound,
        std::io::ErrorKind::PermissionDenied => IoKind::Permission,
        std::io::ErrorKind::WouldBlock => IoKind::Busy,
        std::io::ErrorKind::Unsupported => IoKind::Unsupported,
        _ => IoKind::Other,
    }
}

impl fmt::Display for PlatformError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { path, source } => write!(f, "读写 {} 失败：{source}", path.display()),
            Self::NotSupported { why } => write!(f, "能力不支持：{why}"),
            Self::UnsupportedFs { why } => write!(f, "文件系统不受支持：{why}"),
            Self::Busy { why } => write!(f, "目标被占用：{why}"),
            Self::Denied { why } => write!(f, "权限不足：{why}"),
            Self::Parse { what } => write!(f, "解析失败：{what}"),
            Self::Tools { program, why } => write!(f, "外部程序 {program} 执行失败：{why}"),
        }
    }
}

impl std::error::Error for PlatformError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::NotSupported { .. }
            | Self::UnsupportedFs { .. }
            | Self::Busy { .. }
            | Self::Denied { .. }
            | Self::Parse { .. }
            | Self::Tools { .. } => None,
        }
    }
}

pub type PlatformResult<T> = Result<T, PlatformError>;

/// 目标的种类。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TargetKind {
    /// 普通文件，含镜像与 loop 的后备文件。
    File,
    /// 块设备。
    Block,
}

/// 一个分区的起点与长度，单位 512 字节扇区。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Partition {
    pub number: u32,
    pub start_lba: u64,
    pub sectors: u64,
}

/// 一条挂载记录。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MountEntry {
    pub major: u32,
    pub minor: u32,
    pub mount_point: PathBuf,
}

impl MountEntry {
    pub fn device(&self) -> String {
        format!("{}:{}", self.major, self.minor)
    }
}

/// 一个已启用的交换区。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SwapEntry {
    pub path: PathBuf,
    pub kind: String,
}

/// 目标的占用视图。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Occupancy {
    pub mounts: Vec<MountEntry>,
    pub swaps: Vec<SwapEntry>,
    pub holders: Vec<String>,
    pub array_member: bool,
    pub loops: Vec<String>,
}

impl Occupancy {
    /// 任一证据表明目标被占用。
    pub fn is_occupied(&self) -> bool {
        !self.mounts.is_empty()
            || !self.swaps.is_empty()
            || !self.holders.is_empty()
            || self.array_member
            || !self.loops.is_empty()
    }
}

/// 一个可直接交给外部工具的分区设备视图。
///
/// 块设备目标对应的分区节点直接可用；普通文件目标对应的区间由 `loop_device`
/// 承载，用后必须经 [`Platform::loop_detach`] 释放。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PartitionDevice {
    pub path: PathBuf,
    pub loop_device: Option<PathBuf>,
}

/// 文件系统的尺寸事实：当前大小与可收缩到的最小大小。
///
/// `min_bytes` 为 `None` 表示该族不能收缩或最小尺寸尚未探出；调用方不得据此
/// 推断为 0 或"可任意收缩"。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FsSize {
    pub bytes: u64,
    pub min_bytes: Option<u64>,
}

/// btrfs 文件系统的顶层信息。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BtrfsShow {
    pub fsid: [u8; 16],
    /// 该文件系统当前跨的设备数。
    pub devices: u32,
}

/// 物理卷的关键字段。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PvFacts {
    pub name: String,
    pub size_bytes: u64,
    pub pe_start_bytes: u64,
    pub vg_name: String,
}

/// 外部程序的退出结果。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ExitKind {
    Succeeded,
    Failed(i32),
    Signaled(i32),
    #[default]
    Unknown,
}

/// 外部程序的输出。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Output {
    pub exit: ExitKind,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

impl Output {
    pub fn succeeded(&self) -> bool {
        self.exit == ExitKind::Succeeded
    }

    pub fn stdout_text(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }

    pub fn stderr_text(&self) -> String {
        String::from_utf8_lossy(&self.stderr).into_owned()
    }
}

/// 记录目录中的一项。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecordEntry {
    pub name: String,
    pub bytes: u64,
    pub active: bool,
}

/// 一个记录目录及其中条目。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecordDir {
    pub location: PathBuf,
    pub entries: Vec<RecordEntry>,
}

/// 目标对应的记录目录。
///
/// 目录位置与目录权限由平台决定；上层只按 `load`/`store`/`clear`/`quarantine`
/// 操作整份记录，不拼接文件路径。`store` 必须整份替换并提交，`load` 读出最后一次
/// 成功提交的内容；不存在与读不出分属 `Ok(None)` 与 `Err`。
pub trait RecordStore {
    fn location(&self) -> &Path;
    fn load(&self) -> PlatformResult<Option<Vec<u8>>>;
    fn store(&mut self, bytes: &[u8]) -> PlatformResult<()>;
    fn clear(&mut self) -> PlatformResult<()>;
    fn entries(&self) -> PlatformResult<Vec<RecordEntry>>;
    /// 读出目录中的一个条目；名字不是普通文件或读不出时返回错误。
    fn read_entry(&self, name: &str) -> PlatformResult<Vec<u8>>;
    /// 把活跃记录改名归档，返回归档后的文件名。
    fn quarantine(&mut self) -> PlatformResult<String>;
}

/// 已打开目标的读写句柄。
pub trait TargetHandle {
    fn path(&self) -> &Path;
    fn kind(&self) -> TargetKind;
    fn len(&self) -> u64;
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
    fn sector_size(&self) -> Option<u64>;
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> PlatformResult<usize>;
    fn write_at(&mut self, offset: u64, buf: &[u8]) -> PlatformResult<usize>;
    fn barrier(&mut self) -> PlatformResult<()>;
    /// 取得整盘独占租约；已被占用时返回 [`PlatformError::Busy`]。
    fn lock_exclusive(&mut self) -> PlatformResult<()>;
}

/// 按偏移读取。
pub trait ReadAt {
    fn len(&self) -> PlatformResult<u64>;
    fn is_empty(&self) -> PlatformResult<bool> {
        self.len().map(|len| len == 0)
    }
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> PlatformResult<usize>;
}

/// 按偏移写入并提交。
pub trait WriteAt: ReadAt {
    fn write_at(&mut self, offset: u64, buf: &[u8]) -> PlatformResult<usize>;
    fn barrier(&mut self) -> PlatformResult<()>;
}

/// 启动一台虚拟机载体所需的全部事实。
///
/// 目标以独立描述符交给虚拟机，因此启动器按 `target` 重新打开一份句柄；宿主
/// 已持有的写句柄在会话期间不再写入目标。
pub struct CarrierLaunch {
    pub target: PathBuf,
    pub sector_size: u64,
    pub disk_size: u64,
    pub kernel: PathBuf,
    pub initramfs: PathBuf,
    pub arch: String,
    pub token: String,
}

/// 一台已启动的虚拟机载体。
pub trait CarrierProcess: Send {
    /// 取得与载体通信的写端与读端；只能取得一次。
    fn take_io(&mut self) -> Option<(Box<dyn Write + Send>, Box<dyn Read + Send>)>;
    /// 结束载体；重复调用无副作用。
    fn terminate(&mut self);
}

/// 宿主能力端口。
pub trait Platform {
    /// 以读写方式打开目标；写通道只在执行期使用。
    fn open_target(&self, path: &Path) -> PlatformResult<Box<dyn TargetHandle>>;
    /// 以只读方式打开目标；观察与判定只走这条路径。
    fn open_target_read_only(&self, path: &Path) -> PlatformResult<Box<dyn TargetHandle>>;
    fn fingerprint(&self, path: &Path) -> PlatformResult<Fingerprint>;
    fn partitions(&self, path: &Path) -> PlatformResult<Vec<Partition>>;
    fn occupancy(&self, path: &Path) -> PlatformResult<Occupancy>;
    fn reread(&self, path: &Path) -> PlatformResult<()>;
    fn update_table(&self, path: &Path) -> PlatformResult<()>;
    fn btrfs_show(&self, path: &Path) -> PlatformResult<BtrfsShow>;
    /// 目标所在物理卷的关键字段；不是物理卷时返回空集。
    fn pvs(&self, path: &Path) -> PlatformResult<Vec<PvFacts>>;
    fn loop_attach(&self, backing: &Path, offset: u64, size: u64) -> PlatformResult<PathBuf>;
    fn loop_detach(&self, device: &Path) -> PlatformResult<()>;
    fn mount(
        &self,
        source: &Path,
        mount_point: &Path,
        fstype: Option<&str>,
        options: &[&str],
    ) -> PlatformResult<()>;
    fn umount(&self, mount_point: &Path) -> PlatformResult<()>;
    fn require(&self, program: Program) -> PlatformResult<PathBuf>;
    fn run(&self, program: Program, args: &[&str]) -> PlatformResult<Output>;
    /// 取得分区对应的设备视图。
    ///
    /// 目标是块设备时按分区号定位分区节点；目标是普通文件时在 `region` 上挂接
    /// loop。没有任何一种可得时返回 `NotSupported`，不以整盘冒充分区。
    fn partition_device(
        &self,
        target: &Path,
        partition: u32,
        region: Region,
    ) -> PlatformResult<PartitionDevice> {
        let _ = (target, partition, region);
        Err(PlatformError::not_supported("该宿主不提供分区设备视图"))
    }
    /// 读取文件系统的当前大小与最小尺寸。
    ///
    /// 只做只读探测；目标不是可识别文件系统时返回 `UnsupportedFs`。
    fn filesystem_size(
        &self,
        target: &Path,
        partition: u32,
        region: Region,
        kind: FsKind,
    ) -> PlatformResult<FsSize> {
        let _ = (target, partition, region, kind);
        Err(PlatformError::not_supported("该宿主不提供文件系统尺寸探测"))
    }
    /// 读取已取得的分区设备上文件系统的尺寸事实。
    ///
    /// 与 [`Platform::filesystem_size`] 的区别是不再定位设备，供执行侧在已挂接
    /// 的 loop 上回读使用。
    fn filesystem_size_of(
        &self,
        device: &Path,
        device_bytes: u64,
        kind: FsKind,
    ) -> PlatformResult<FsSize> {
        let _ = (device, device_bytes, kind);
        Err(PlatformError::not_supported("该宿主不提供文件系统尺寸探测"))
    }
    /// 把设备上若干字节区间清零并提交。
    ///
    /// 建卷前擦除旧签名副本时使用；区间越界由实现拒绝。
    fn wipe(&self, device: &Path, ranges: &[Region]) -> PlatformResult<()> {
        let _ = (device, ranges);
        Err(PlatformError::not_supported("该宿主不提供设备清零"))
    }
    /// 建一个私有临时挂载点；用后经 [`Platform::remove_mount_point`] 释放。
    fn make_mount_point(&self) -> PlatformResult<PathBuf> {
        Err(PlatformError::not_supported("该宿主不提供临时挂载点"))
    }
    fn remove_mount_point(&self, point: &Path) -> PlatformResult<()> {
        let _ = point;
        Err(PlatformError::not_supported("该宿主不提供临时挂载点"))
    }
    /// 宿主能否直接运行文件系统工具。
    ///
    /// 建卷、扩缩、改标签等步骤要经分区设备视图操作块设备；不满足该条件的
    /// 宿主应改由虚拟机载体执行同一批步骤。
    fn filesystem_ready(&self) -> bool {
        false
    }
    /// 宿主是否提供虚拟机载体。
    fn carrier_available(&self) -> bool {
        false
    }
    /// 来宾看到的整盘设备路径；没有载体时为 `None`。
    fn carrier_target(&self) -> Option<&'static str> {
        None
    }
    /// 启动一台虚拟机载体。
    fn launch_carrier(&self, _launch: &CarrierLaunch) -> PlatformResult<Box<dyn CarrierProcess>> {
        Err(PlatformError::not_supported("该宿主不提供虚拟机载体"))
    }
    /// 目标对应的记录目录；宿主没有该能力时返回 `NotSupported`。
    fn records(
        &self,
        target: &Path,
        identity: Fingerprint,
    ) -> PlatformResult<Box<dyn RecordStore>> {
        let _ = (target, identity);
        Err(PlatformError::not_supported("该宿主不提供记录目录"))
    }
    /// 枚举 `root` 这个容器下的记录目录及其条目。
    ///
    /// `root` 是容器而不是单个记录目录；把记录目录当根、或指向状态根内部却没有
    /// 指到容器，都返回 `NotSupported` 并说明正确位置。容器不存在时返回 I/O 错误。
    fn record_dirs(&self, root: &Path) -> PlatformResult<Vec<RecordDir>> {
        let _ = root;
        Err(PlatformError::not_supported("该宿主不提供记录容器"))
    }
    /// 读出记录目录 `dir` 下名为 `name` 的条目。
    fn read_record(&self, dir: &Path, name: &str) -> PlatformResult<Vec<u8>> {
        let _ = (dir, name);
        Err(PlatformError::not_supported("该宿主不提供记录读取"))
    }
}

/// 打开本平台的端口实现。平台选择只在此处发生。
pub fn open() -> PlatformResult<Box<dyn Platform>> {
    #[cfg(target_os = "linux")]
    {
        Ok(Box::new(linux::Linux::new()))
    }
    #[cfg(target_os = "windows")]
    {
        Ok(Box::new(windows::Windows::new()))
    }
    #[cfg(target_os = "macos")]
    {
        Ok(Box::new(macos::MacOs::new()))
    }
    #[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
    {
        Err(PlatformError::not_supported(
            "当前操作系统没有对应的端口实现",
        ))
    }
}

#[cfg(test)]
mod skeleton_tests {
    use super::*;

    struct PendingHost;

    unsupported_platform!(PendingHost, "测试宿主");

    #[test]
    fn pending_host_reports_not_supported() {
        let host = PendingHost;
        assert!(matches!(
            host.reread(Path::new("/x")),
            Err(PlatformError::NotSupported { .. })
        ));
        assert!(matches!(
            host.require(Program::E2fsck),
            Err(PlatformError::NotSupported { .. })
        ));
    }
}
