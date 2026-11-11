//! 领域值类型：只依赖 std，构造即校验。

use std::fmt;

/// 目标载体的种类：能力矩阵按它与平台共同裁定
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MediaKind {
    Block,
    Image,
}

/// 逻辑扇区大小（字节）。
///
/// 块设备只用内核报告值；镜像使用 `--sector-size` 的显式覆盖值。上限是实现边界，
/// 由校验算术、I/O 对齐与目标能力共同决定；GPT 头里的扇区大小字段是 UINT32，
/// 字段宽度只表示可编码范围，不是设备兼容性承诺。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct SectorSize(u64);

impl SectorSize {
    pub(crate) const MIN_BYTES: u64 = 512;
    pub(crate) const MAX_BYTES: u64 = 65536;

    pub(crate) fn try_new(bytes: u64) -> Result<Self, SectorSizeError> {
        if bytes < Self::MIN_BYTES {
            return Err(SectorSizeError::BelowMinimum(bytes));
        }
        if !bytes.is_power_of_two() {
            return Err(SectorSizeError::NotPowerOfTwo(bytes));
        }
        if bytes > Self::MAX_BYTES {
            return Err(SectorSizeError::AboveImplementationLimit(bytes));
        }
        Ok(Self(bytes))
    }

    pub(crate) fn get(self) -> u64 {
        self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SectorSizeError {
    BelowMinimum(u64),
    NotPowerOfTwo(u64),
    AboveImplementationLimit(u64),
}

impl fmt::Display for SectorSizeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::BelowMinimum(n) => write!(f, "logical sector size {n} is below the 512-byte minimum"),
            Self::NotPowerOfTwo(n) => write!(f, "logical sector size {n} must be a power of two"),
            Self::AboveImplementationLimit(n) => write!(
                f,
                "logical sector size {n} exceeds this tool's {}-byte implementation limit \
                 (the GPT sector-size field is UINT32, so the limit is not a format requirement)",
                SectorSize::MAX_BYTES
            ),
        }
    }
}

impl std::error::Error for SectorSizeError {}

/// 放置对齐策略。`Mib(n)` 为 n 个 MiB，`n >= 1`；`None` 按给定 LBA 落位。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Align(AlignKind);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AlignKind {
    Mib(u32),
    None,
}

impl Align {
    #[cfg(test)]
    pub(crate) fn mib(mib: u32) -> Option<Self> {
        (mib >= 1).then_some(Self(AlignKind::Mib(mib)))
    }

    pub(crate) const fn none() -> Self {
        Self(AlignKind::None)
    }

    pub(crate) fn parse(text: &str) -> Result<Self, AlignError> {
        match text {
            "mib" => Ok(Self(AlignKind::Mib(1))),
            "none" => Ok(Self::none()),
            other => Err(AlignError(other.to_string())),
        }
    }

    /// LBA 所属表的扇区数；`Mib(MiB)` 在 1 LBA 不足一个单位时不为齐。
    pub(crate) fn unit_sectors(self, table_sector_size: u64) -> Option<u64> {
        match self.0 {
            AlignKind::None => None,
            AlignKind::Mib(mib) => {
                let unit = (u64::from(mib) << 20).checked_div(table_sector_size).unwrap_or(0);
                (unit > 0).then_some(unit)
            }
        }
    }
}

impl fmt::Display for Align {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            AlignKind::Mib(mib) => write!(f, "{mib}MiB"),
            AlignKind::None => f.write_str("none"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AlignError(String);

impl fmt::Display for AlignError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid alignment {:?} (mib|none)", self.0)
    }
}

impl std::error::Error for AlignError {}

/// 可识别的文件系统种类。只列可挂载文件系统；swap 区与 LVM2 物理卷不是文件系统，
/// 由 [`ContentKind`] 的独立变体表达。
///
/// ext2/3/4 收敛为一个 `Ext`：0xEF53 区分不出 2/3/4，而 e2fsck/resize2fs/tune2fs 对
/// 三者通用；`--fs` 请求的具体 extN 由 `MkfsFs` 承载
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FsKind {
    Squashfs,
    Erofs,
    Ext,
    Xfs,
    Btrfs,
    F2fs,
    Exfat,
    Vfat,
    Ntfs,
    HfsPlus,
    Apfs,
}

impl FsKind {
    /// 对外规范名（JSON、文案、支持清单都由它生成）
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Squashfs => "squashfs",
            Self::Erofs => "erofs",
            Self::Ext => "ext",
            Self::Xfs => "xfs",
            Self::Btrfs => "btrfs",
            Self::F2fs => "f2fs",
            Self::Exfat => "exfat",
            Self::Vfat => "vfat",
            Self::Ntfs => "ntfs",
            Self::HfsPlus => "hfsplus",
            Self::Apfs => "apfs",
        }
    }
}

impl fmt::Display for FsKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 一次内容探测的结论。`Filesystem` 以外的变体都不是文件系统，故不放进 [`FsKind`]。
///
/// `Unknown` 只表示"本次探测没有认出受支持的内容"，不表示区域为空，也不表示可以安全
/// 跳过——"可供识别的字节不足"同样落在这里。它与记录侧的 `Absent`（没有记录）是两件
/// 事，不得合并。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ContentKind {
    Filesystem(FsKind),
    /// swap 区（签名命中且头内自洽；是否能在本机激活由探测侧的 `SwapHit` 另述）
    Swap,
    /// LVM2 物理卷标签（`LABELONE` + `LVM2 001`）：不是可挂载文件系统
    Lvm2Pv,
    Unknown,
}

impl ContentKind {
    /// 文件系统类别；非文件系统内容返回 `None`
    pub(crate) fn filesystem(self) -> Option<FsKind> {
        match self {
            Self::Filesystem(kind) => Some(kind),
            Self::Swap | Self::Lvm2Pv | Self::Unknown => None,
        }
    }

    /// 对外规范名；`Unknown` → `"unknown"`
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Filesystem(kind) => kind.as_str(),
            Self::Swap => "swap",
            Self::Lvm2Pv => "lvm2_pv",
            Self::Unknown => "unknown",
        }
    }
}

impl fmt::Display for ContentKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}
