//! 分区几何里与搬移共用的判定：缩容相位、FS 动作与 NTFS 隐藏扇区。
//!
//! 执行不属于这里：`resize-part` 的表与 FS 步骤都经 `assembly` 装配、`execution` 执行，
//! 本模块只保留四面共用的判定与那两个纯函数。

use super::*;

/// 缩容状态。三态而非 bool：`Unverified` 表示缩容命令已返回、落点却
/// 回读不出来——搬数据按它继续就可能切进未缩的元数据，故不能与 `Verified` 等同
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub(crate) enum FsShrink {
    NotShrunk = 0,
    Verified = 1,
    Unverified = 2,
}

impl FsShrink {
    fn from_code(code: u8) -> io::Result<Self> {
        match code {
            0 => Ok(Self::NotShrunk),
            1 => Ok(Self::Verified),
            2 => Ok(Self::Unverified),
            _ => Err(io::Error::new(io::ErrorKind::InvalidData, "implausible resize record shrink state")),
        }
    }
}

/// `progress.fs_phase` 与 [`FsShrink`] 同序：记录里只放这一个字节。
/// 认不出的码按"未缩"处理——重做一次缩容是幂等的，误当作已缩才会切进未缩的元数据
pub(crate) fn fs_shrunk_from_phase(code: u8) -> FsShrink {
    FsShrink::from_code(code).unwrap_or(FsShrink::NotShrunk)
}

/// 本次对 FS 的动作。prepare 算一次，execute 只 match——若两处各自从尺寸重新推导，
/// "要不要缩/扩"就有了两个来源（尺寸没变、缩容已完成、`--no-fs` 都落进 `Leave`）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FsAction {
    /// 不碰 FS
    Leave,
    Shrink,
    Grow,
}
