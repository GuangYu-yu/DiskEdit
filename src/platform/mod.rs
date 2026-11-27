//! 宿主端口：上层只通过本模块的中立接口触碰操作系统。
//!
//! 平台选择与具体系统调用只出现在本层；`app` 与纯核不引用任何平台名。
//! 端口只暴露事实与动作：句柄、扇区大小、身份、占用、外部程序与记录根目录。

//! 目录约定：`mod.rs` 只放中立端口与平台选择；每个平台一个子树，系统调用、
//! 虚拟文件系统路径与具体可执行名只出现在对应子树内。新增宿主只加子树并把
//! 它接进 [`host`]，不改上层。

pub mod probe;

#[cfg(target_os = "linux")]
pub mod linux;
#[cfg(target_os = "macos")]
pub mod macos;
#[cfg(target_os = "windows")]
pub mod windows;

use std::fmt;
use std::path::{Path, PathBuf};

use diskedit_core::fingerprint::Fingerprint;
use diskedit_core::fs::{FsKind, Program};

#[cfg(target_os = "linux")]
pub use linux::LinuxPlatform;
#[cfg(target_os = "macos")]
pub use macos::MacOsPlatform;
#[cfg(target_os = "windows")]
pub use windows::WindowsPlatform;

/// 当前宿主的端口实现。平台选择只在这里发生。
#[cfg(target_os = "linux")]
pub fn host() -> LinuxPlatform {
    LinuxPlatform
}

#[cfg(target_os = "macos")]
pub fn host() -> MacOsPlatform {
    MacOsPlatform
}

#[cfg(target_os = "windows")]
pub fn host() -> WindowsPlatform {
    WindowsPlatform
}

/// 没有专属子树的其他宿主。
#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
pub fn host() -> UnsupportedPlatform {
    UnsupportedPlatform
}

/// 尚未接入的宿主：每个端口都显式返回 [`PlatformError::NotSupported`]，
/// 不返回空事实、不假装成功。接入某个能力时在对应平台子树里实现它。
pub struct UnsupportedPlatform;

/// 为尚未接入的宿主生成显式"不支持"实现。
macro_rules! unsupported_platform {
    ($ty:ty, $os:literal) => {
        impl $crate::platform::Platform for $ty {
            fn open_target(
                &self,
                _path: &std::path::Path,
            ) -> $crate::platform::PlatformResult<Box<dyn $crate::platform::TargetHandle>> {
                Err($crate::platform::PlatformError::not_supported(concat!(
                    $os,
                    " 的打开端口尚未接入"
                )))
            }

            fn open_target_read_only(
                &self,
                _path: &std::path::Path,
            ) -> $crate::platform::PlatformResult<Box<dyn $crate::platform::TargetHandle>> {
                Err($crate::platform::PlatformError::not_supported(concat!(
                    $os,
                    " 的打开端口尚未接入"
                )))
            }

            fn path_kind(&self, _path: &std::path::Path) -> $crate::platform::TargetKind {
                $crate::platform::TargetKind::Other
            }

            fn is_partition_node(&self, _path: &std::path::Path) -> bool {
                false
            }

            fn identity(
                &self,
                _path: &std::path::Path,
            ) -> $crate::platform::PlatformResult<diskedit_core::fingerprint::Fingerprint> {
                Err($crate::platform::PlatformError::not_supported(concat!(
                    $os,
                    " 的身份端口尚未接入"
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

            fn state_root(&self) -> Option<std::path::PathBuf> {
                None
            }

            fn is_directory(&self, _path: &std::path::Path) -> bool {
                false
            }

            fn list_record_entries(
                &self,
                _root: &std::path::Path,
                _name: &str,
            ) -> $crate::platform::PlatformResult<Vec<std::path::PathBuf>> {
                Err($crate::platform::PlatformError::not_supported(concat!(
                    $os,
                    " 的记录目录端口尚未接入"
                )))
            }

            fn read_record(
                &self,
                _path: &std::path::Path,
            ) -> $crate::platform::PlatformResult<$crate::platform::RecordEntry> {
                Err($crate::platform::PlatformError::not_supported(concat!(
                    $os,
                    " 的记录读取端口尚未接入"
                )))
            }

            fn write_record(
                &self,
                _path: &std::path::Path,
                _bytes: &[u8],
            ) -> $crate::platform::PlatformResult<()> {
                Err($crate::platform::PlatformError::not_supported(concat!(
                    $os,
                    " 的记录写入端口尚未接入"
                )))
            }

            fn remove_record(
                &self,
                _path: &std::path::Path,
            ) -> $crate::platform::PlatformResult<()> {
                Err($crate::platform::PlatformError::not_supported(concat!(
                    $os,
                    " 的记录删除端口尚未接入"
                )))
            }

            fn rename_record(
                &self,
                _from: &std::path::Path,
                _to: &std::path::Path,
            ) -> $crate::platform::PlatformResult<()> {
                Err($crate::platform::PlatformError::not_supported(concat!(
                    $os,
                    " 的记录改名端口尚未接入"
                )))
            }

            fn run(
                &self,
                _program: diskedit_core::fs::Program,
                _args: &[String],
            ) -> $crate::platform::PlatformResult<$crate::platform::ToolRun> {
                Err($crate::platform::PlatformError::not_supported(concat!(
                    $os,
                    " 没有外部工具端口"
                )))
            }

            fn tool_available(&self, _program: diskedit_core::fs::Program) -> bool {
                false
            }

            fn tool_name(&self, _program: diskedit_core::fs::Program) -> &'static str {
                "unavailable"
            }

            fn bind_loop(
                &self,
                _path: &std::path::Path,
                _offset: u64,
                _len: u64,
            ) -> $crate::platform::PlatformResult<$crate::platform::LoopBinding> {
                Err($crate::platform::PlatformError::not_supported(concat!(
                    $os,
                    " 没有 loop 设备"
                )))
            }

            fn partition_device(
                &self,
                _disk: &std::path::Path,
                _number: u32,
            ) -> $crate::platform::PlatformResult<std::path::PathBuf> {
                Err($crate::platform::PlatformError::not_supported(concat!(
                    $os,
                    " 的分区设备端口尚未接入"
                )))
            }

            fn entropy(&self, _len: usize) -> $crate::platform::PlatformResult<Vec<u8>> {
                Err($crate::platform::PlatformError::not_supported(concat!(
                    $os,
                    " 的随机源尚未接入"
                )))
            }

            fn create_directory_all(
                &self,
                _path: &std::path::Path,
            ) -> $crate::platform::PlatformResult<()> {
                Err($crate::platform::PlatformError::not_supported(concat!(
                    $os,
                    " 的目录端口尚未接入"
                )))
            }

            fn remove_directory(
                &self,
                _path: &std::path::Path,
            ) -> $crate::platform::PlatformResult<()> {
                Err($crate::platform::PlatformError::not_supported(concat!(
                    $os,
                    " 的目录端口尚未接入"
                )))
            }

            fn temporary_directory(
                &self,
                _tag: &str,
            ) -> $crate::platform::PlatformResult<std::path::PathBuf> {
                Err($crate::platform::PlatformError::not_supported(concat!(
                    $os,
                    " 的临时目录端口尚未接入"
                )))
            }

            fn lock_state(
                &self,
                _path: &std::path::Path,
            ) -> $crate::platform::PlatformResult<Box<dyn $crate::platform::StateLock>> {
                Err($crate::platform::PlatformError::not_supported(concat!(
                    $os,
                    " 的状态锁尚未接入"
                )))
            }
        }
    };
}

#[cfg(not(target_os = "linux"))]
pub(crate) use unsupported_platform;

unsupported_platform!(UnsupportedPlatform, "当前宿主");

/// 端口操作失败。
#[derive(Debug)]
pub enum PlatformError {
    /// 目标或系统调用返回的 I/O 错误。
    Io { path: String, message: String },
    /// 本平台不提供该能力。
    NotSupported(String),
    /// 无法解释系统返回的数据。
    Parse(String),
    /// 当前对象不适用该操作。
    NotApplicable(String),
}

impl PlatformError {
    /// 以路径与底层错误构造 I/O 失败。
    pub fn io(path: &Path, error: std::io::Error) -> Self {
        Self::Io {
            path: path.display().to_string(),
            message: error.to_string(),
        }
    }

    /// 本平台不提供该能力。
    pub fn not_supported(why: impl Into<String>) -> Self {
        Self::NotSupported(why.into())
    }

    /// 无法解释系统返回的数据。
    pub fn parse(why: impl Into<String>) -> Self {
        Self::Parse(why.into())
    }

    /// 当前对象不适用该操作。
    pub fn not_applicable(why: impl Into<String>) -> Self {
        Self::NotApplicable(why.into())
    }
}

impl fmt::Display for PlatformError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { path, message } => write!(f, "{path}: {message}"),
            Self::NotSupported(why) | Self::Parse(why) | Self::NotApplicable(why) => {
                f.write_str(why)
            }
        }
    }
}

impl std::error::Error for PlatformError {}

/// 端口调用的结果。
pub type PlatformResult<T> = Result<T, PlatformError>;

/// 目标在宿主上的存在形态。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TargetKind {
    /// 普通文件（含镜像）。
    File,
    /// 块设备（整盘或单个分区）。
    BlockDevice,
    /// 其他对象：目录、管道、字符设备等。
    Other,
}

/// 镜像文件没有内核扇区事实时采用的固定块大小。
pub const IMAGE_SECTOR_SIZE: u64 = 512;

impl TargetKind {
    pub const fn is_block_device(self) -> bool {
        matches!(self, Self::BlockDevice)
    }
}

/// 目标在宿主上的占用结论。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Occupancy {
    /// 没有任何占用者或占用来源。
    Idle,
    /// 已被占用，附带来源说明。
    Used { evidence: String },
    /// 无法判定，附带原因。
    Unknown { why: String },
}

/// 报表里报出的设备与目标设备的关系。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DeviceMatch {
    Same,
    Different,
    /// 无法证明相同：调用方据此拒绝，而不是当作"不同"。
    Unknown(String),
}

/// 外部程序的一次运行结果。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolRun {
    /// 进程退出码；被信号终止时为 `None`。
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

/// 记录位置上是什么。
///
/// "没有记录"与"位置上有别的东西"必须分开：后者说明记录目录被动过，调用方要
/// 按不可信现场处理，而不是当作空目录继续。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RecordEntry {
    /// 位置为空，没有记录。
    Absent,
    /// 位置上是普通文件，附字节。
    Contents(Vec<u8>),
    /// 位置上不是普通文件：符号链接、目录、设备、FIFO、Unix socket 等。
    NotARecoveryFile,
}

/// 临时绑定到目标的块设备视图；销毁时解除绑定。
#[derive(Debug)]
pub struct LoopBinding {
    device: PathBuf,
}

impl LoopBinding {
    pub fn device(&self) -> &Path {
        &self.device
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn new(device: PathBuf) -> Self {
        Self { device }
    }
}

/// 运行时把目标交给虚拟机载体的入口。
///
/// 只有 Linux 提供实现；其余宿主没有载体时返回 `None`，上层照常走本机路径。
#[cfg(target_os = "linux")]
pub fn worker_entry(args: &[std::ffi::OsString]) -> Option<u8> {
    linux::carrier::worker::invoked(args).then(linux::carrier::worker::run)
}

#[cfg(not(target_os = "linux"))]
pub fn worker_entry(_args: &[std::ffi::OsString]) -> Option<u8> {
    None
}

/// 一个挂载点反查出的挂载事实。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MountFacts {
    /// 路径是挂载点，挂载源是某块整盘上的分区。
    Mounted {
        mount_point: PathBuf,
        device: PathBuf,
        disk: PathBuf,
        partition: u32,
    },
    /// 路径不是任何挂载点。
    NotMounted,
    /// 挂载源不是分区：整盘、或非块设备的挂载源。
    NotPartition { device: PathBuf },
    /// 同一路径有多条挂载记录，无法确定是哪一条。
    Ambiguous,
    /// 挂载源无法映射到单个整盘。
    Unmapped { why: String },
}

/// 一个分区在挂载命名空间里的使用情况。
///
/// 占用判定必须取全部可读命名空间：别的命名空间里的挂载同样在用这块分区，只看本
/// 命名空间会把那种占用读成"未挂载"，随后在仍被挂载的盘上写表。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PartitionMount {
    /// 本进程所在的命名空间里挂着，给出唯一挂载点。
    Here { mount_point: PathBuf },
    /// 只在另一个挂载命名空间里挂着；本命名空间既不能用也不能卸。
    Elsewhere {
        mount_point: PathBuf,
        /// 读到记录时所在的进程；取不到命名空间身份时为 `None`。
        pid: Option<u32>,
    },
    /// 任何可读命名空间里都没有挂载。
    NotMounted,
    /// 本命名空间里同一分区有多条挂载记录，无法确定生效层。
    Ambiguous,
}

/// 交给载体的在线扩容请求。载体在来宾里把文件系统撑满所在分区。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OnlineRequest {
    pub mount_point: PathBuf,
    pub device: PathBuf,
    pub disk: PathBuf,
    pub partition: u32,
    pub filesystem: FsKind,
    /// 分区容量；文件系统应长到这个字节数。
    pub bytes: u64,
    pub sector_size: u64,
}

/// 载体报回的在线扩容结果：工具报告的文件系统几何。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OnlineReport {
    /// 块数；载体没能读出时为 `None`。
    pub blocks: Option<u64>,
    /// 块大小；载体没能读出时为 `None`。
    pub block_size: Option<u64>,
}

/// 交给载体的一段绝对偏移写入。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CarrierWrite {
    pub offset: u64,
    pub bytes: Vec<u8>,
}

/// 交给载体的整盘离线写入：来宾把各段按绝对偏移写进同一块盘。
///
/// 用于宿主缺块层能力、本机无法安全写表的块设备目标；记录、锁、摘要与退出码都
/// 留在宿主，来宾只落字节并重读分区表。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiskWriteRequest {
    pub disk: PathBuf,
    pub sector_size: u64,
    pub writes: Vec<CarrierWrite>,
}

/// 虚拟机载体端口：把宿主做不了的在线操作交给来宾。
pub trait Carrier: Send + Sync {
    /// 在来宾里把 `request.device` 上的文件系统撑满分区。
    fn online_grow(&self, request: &OnlineRequest) -> PlatformResult<OnlineReport>;

    /// 在来宾里按绝对偏移写整盘；默认按"不支持"返回。
    fn write_disk(&self, _request: &DiskWriteRequest) -> PlatformResult<()> {
        Err(PlatformError::not_supported(
            "this carrier cannot write a whole disk",
        ))
    }
}

/// 目标的打开方式由具体平台决定，上层只拿到这个按偏移读写的句柄。
pub trait TargetHandle {
    /// 打开时使用的路径。
    fn path(&self) -> &Path;

    /// 打开时读到的字节长度。
    fn len(&self) -> u64;

    /// 目标的宿主形态。
    fn kind(&self) -> TargetKind;

    /// 块设备的逻辑扇区字节数；普通文件没有这一事实。
    fn sector_size(&self) -> Option<u64>;

    /// 解析目标的逻辑块大小。
    ///
    /// 块设备以内核回报为准，内核读不到时这里是 `None`：没有可信值，调用方必须
    /// 显式处理，不能当成 512。镜像没有内核扇区事实，按镜像格式的固定块大小。
    fn resolved_sector_size(&self) -> Option<u64> {
        self.sector_size()
            .or_else(|| matches!(self.kind(), TargetKind::File).then_some(IMAGE_SECTOR_SIZE))
    }

    /// 目标是否为空。
    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// 按偏移读取，返回实际读到的字节数。
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> PlatformResult<usize>;

    /// 按偏移写入，返回实际写入的字节数。
    fn write_at(&mut self, offset: u64, buf: &[u8]) -> PlatformResult<usize>;

    /// 提交目标缓存。
    fn barrier(&mut self) -> PlatformResult<()>;

    /// 让内核重读分区表与块设备尺寸；普通文件无事可做。
    fn refresh_partitions(&mut self) -> PlatformResult<()>;
}

/// 记录目录上的独占锁；销毁时释放。
pub trait StateLock {}

/// 宿主端口。
pub trait Platform: Send + Sync {
    /// 以读写方式打开目标。
    fn open_target(&self, path: &Path) -> PlatformResult<Box<dyn TargetHandle>>;

    /// 以只读方式打开目标。
    fn open_target_read_only(&self, path: &Path) -> PlatformResult<Box<dyn TargetHandle>>;

    /// 目标的宿主形态；只读元数据，不打开文件。
    fn path_kind(&self, path: &Path) -> TargetKind;

    /// 本宿主能否对目标所在的块设备执行分区表与视图操作（BLKPG/BLKRRPART、挂载）。
    ///
    /// 普通文件经 loop 得到分区视图，不受该能力限制；缺能力的宿主不能假装能做，
    /// 没有可用载体时上层据此按"不支持"拒绝，而不是照常执行再得到内核 `EPERM`。
    fn filesystem_ready(&self, path: &Path) -> bool {
        let _ = path;
        false
    }

    /// 把一个挂载点反查成挂载事实；本平台不做反查时按"不支持"返回。
    fn mount_facts(&self, path: &Path) -> PlatformResult<MountFacts> {
        let _ = path;
        Err(PlatformError::not_supported(
            "this host does not resolve mount points",
        ))
    }

    /// 整盘上第 N 个分区当前的挂载情况，取全部可读挂载命名空间。
    ///
    /// 不能返回空事实猜测"未挂载"：读不出来就是 `Err`，由调用方按未知处理。
    fn partition_mount(&self, _disk: &Path, _partition: u32) -> PlatformResult<PartitionMount> {
        Err(PlatformError::not_supported(
            "this host does not resolve partition mounts",
        ))
    }

    /// 整盘上第 N 个分区是否是活动 swap 区域。
    fn swap_active(&self, _disk: &Path, _partition: u32) -> PlatformResult<bool> {
        Err(PlatformError::not_supported(
            "this host does not report active swap",
        ))
    }

    /// 让内核把已存在分区的几何改成给定的起点与长度（`BLKPG_RESIZE_PARTITION`）。
    ///
    /// 用于已挂载分区：整盘重读（`BLKRRPART`）在分区被占用时返回 `EBUSY`，只能按
    /// 分区增量调整。缺能力时返回 `NotSupported`，调用方据此落"表已写、视图未同步"。
    fn resize_partition_live(
        &self,
        _disk: &Path,
        _partition: u32,
        _start_bytes: u64,
        _len_bytes: u64,
    ) -> PlatformResult<()> {
        Err(PlatformError::not_supported(
            "this host cannot resize a partition in the kernel view",
        ))
    }

    /// 当前配置的虚拟机载体；未配置或本平台不提供时为 `None`。
    fn carrier(&self) -> PlatformResult<Option<Box<dyn Carrier>>> {
        Ok(None)
    }

    /// 目标本身是不是块设备上的分区节点（`/dev/sda1`），而不是整盘。
    fn is_partition_node(&self, path: &Path) -> bool;

    /// 目标的稳定身份；镜像取文件对象，块设备取设备本身。
    fn identity(&self, path: &Path) -> PlatformResult<Fingerprint>;

    /// 目标的占用结论。
    fn occupancy(&self, path: &Path) -> PlatformResult<Occupancy>;

    /// LVM2 PV 收尾可接受的占用：DM 栈持有镜像的 loop 不算冲突（pvresize 只改 PV
    /// 元数据，持有它的 dm 栈不解析这部分字节）。默认与 [`Self::occupancy`] 相同。
    fn occupancy_for_lvm(&self, path: &Path) -> PlatformResult<Occupancy> {
        self.occupancy(path)
    }

    /// 记录根目录；宿主没有可写状态目录时返回 `None`。
    fn state_root(&self) -> Option<PathBuf>;

    /// 路径是否为目录。
    fn is_directory(&self, path: &Path) -> bool;

    /// 递归列出记录目录里指定文件名的条目，不进入符号链接目录。
    ///
    /// 根不存在或不可读时返回错误。这是记录位置的唯一目录入口；不存在通用的
    /// "列目录"端口，记录扫描无法改走跟随链接的路径。
    fn list_record_entries(&self, root: &Path, name: &str) -> PlatformResult<Vec<PathBuf>>;

    /// 读取记录位置。文件不存在返回 [`RecordEntry::Absent`]，不是普通文件返回
    /// [`RecordEntry::NotARecoveryFile`]；只有真的读不到才是 `Err`。
    fn read_record(&self, path: &Path) -> PlatformResult<RecordEntry>;

    /// 以覆盖方式写记录位置：父目录 fd 锚定，同目录临时名落盘后原子改名。
    fn write_record(&self, path: &Path, bytes: &[u8]) -> PlatformResult<()>;

    /// 删除记录位置：父目录 fd 锚定，删除后同步目录；不存在视为成功。
    fn remove_record(&self, path: &Path) -> PlatformResult<()>;

    /// 在同一目录内改名：两个父目录 fd 锚定，改名后同步目录。
    fn rename_record(&self, from: &Path, to: &Path) -> PlatformResult<()>;

    /// 运行一个外部程序。程序身份到可执行名与参数的映射留在本层。
    fn run(&self, program: Program, args: &[String]) -> PlatformResult<ToolRun>;

    /// 只读检查类程序必须带上的固定旗标（例如 `e2fsck -f -n`）；其他调用为空。
    fn check_arguments(&self, _program: Program) -> &'static [&'static str] {
        &[]
    }

    /// 仅当 `program` 在宿主上可执行时返回真。
    fn tool_available(&self, program: Program) -> bool;

    /// 程序在宿主上的可执行名，用于提示文案。
    fn tool_name(&self, program: Program) -> &'static str;

    /// 把文件的字节区间绑定为块设备视图。
    fn bind_loop(&self, path: &Path, offset: u64, len: u64) -> PlatformResult<LoopBinding>;

    /// 整盘块设备上第 N 个分区的设备节点。
    fn partition_device(&self, disk: &Path, number: u32) -> PlatformResult<PathBuf>;

    /// LVM 报表里报出的设备与目标设备是不是同一段存储。
    ///
    /// 默认按"无法证明"返回：没有该能力的宿主不能让调用方把不确定当成不同。
    fn compare_devices(&self, _target: &Path, _reported: &Path) -> DeviceMatch {
        DeviceMatch::Unknown("this host cannot compare device identities".to_owned())
    }

    /// 宿主随机字节，用于新表的 GUID。
    fn entropy(&self, len: usize) -> PlatformResult<Vec<u8>>;

    /// 递归建立目录。
    fn create_directory_all(&self, path: &Path) -> PlatformResult<()>;

    /// 删除一个空目录；不存在时视为成功。
    fn remove_directory(&self, path: &Path) -> PlatformResult<()>;

    /// 建立一个宿主临时目录，名字带 `tag` 且不与其他调用冲突。
    fn temporary_directory(&self, tag: &str) -> PlatformResult<PathBuf>;

    /// 对状态文件取独占锁；离开作用域即释放。
    fn lock_state(&self, path: &Path) -> PlatformResult<Box<dyn StateLock>>;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 只提供形态与扇区事实的句柄，用于校验块大小解析规则。
    struct Probe {
        kind: TargetKind,
        sector: Option<u64>,
    }

    impl TargetHandle for Probe {
        fn path(&self) -> &Path {
            Path::new("probe")
        }

        fn len(&self) -> u64 {
            0
        }

        fn kind(&self) -> TargetKind {
            self.kind
        }

        fn sector_size(&self) -> Option<u64> {
            self.sector
        }

        fn read_at(&self, _offset: u64, _buf: &mut [u8]) -> PlatformResult<usize> {
            Ok(0)
        }

        fn write_at(&mut self, _offset: u64, _buf: &[u8]) -> PlatformResult<usize> {
            Ok(0)
        }

        fn barrier(&mut self) -> PlatformResult<()> {
            Ok(())
        }

        fn refresh_partitions(&mut self) -> PlatformResult<()> {
            Ok(())
        }
    }

    // covers: I5
    #[test]
    fn an_image_without_a_kernel_sector_fact_uses_the_image_default() {
        let probe = Probe {
            kind: TargetKind::File,
            sector: None,
        };
        assert_eq!(probe.resolved_sector_size(), Some(IMAGE_SECTOR_SIZE));
    }

    // covers: I5
    #[test]
    fn a_block_device_without_a_reported_sector_has_no_trusted_value() {
        let probe = Probe {
            kind: TargetKind::BlockDevice,
            sector: None,
        };
        assert_eq!(probe.resolved_sector_size(), None);
    }

    // covers: I5
    #[test]
    fn a_reported_sector_size_wins_over_the_image_default() {
        let probe = Probe {
            kind: TargetKind::BlockDevice,
            sector: Some(4096),
        };
        assert_eq!(probe.resolved_sector_size(), Some(4096));
    }
}
