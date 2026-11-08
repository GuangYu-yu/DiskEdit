//! 证据的四态分类：格式、身份、长度、CRC 各自成态，互不合并。
//!
//! 分类只回答"现场现在是什么"，不产生删除资格；删除资格由
//! [`crate::recovery::RecoveryEngine::commit`] 单独判定。

use crate::platform::PathError;

use super::record::{CrcRegion, DecodeFailure, Record};
use super::scene::SceneIdentity;

/// 不是本工具证据的条目种类
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum EntryKind {
    Symlink,
    Directory,
    OtherFile,
}

/// 读不到证据的成因
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Cause {
    Inaccessible(String),
    Io(String),
    Unsupported(String),
}

/// 恢复现场的判定结果
#[derive(Clone, Debug, PartialEq, Eq)]
// 记录是固定布局的落盘形态（488 字节）；装箱只把同一份字节搬到堆上，
// 而每个匹配臂都要多一次解引用。差异来自"这就是那份字节"，不是设计遗漏
#[allow(clippy::large_enum_variant)]
pub(crate) enum Evidence {
    Absent,
    /// 格式、长度、CRC、身份全部通过
    Recognized(Record),
    UnrecognizedFormat {
        magic: [u8; 4],
        recorded: Option<SceneIdentity>,
    },
    ForeignTarget {
        recorded: SceneIdentity,
        current: SceneIdentity,
    },
    Incomplete {
        observed: u64,
        expected: u64,
    },
    CrcMismatch {
        region: CrcRegion,
    },
    NotOurs {
        kind: EntryKind,
    },
    Unreadable {
        cause: Cause,
    },
}

impl Evidence {
    /// 对已读出的字节分类。只有 `Recognized` 携带完整记录
    pub(crate) fn classify(bytes: &[u8], current: &SceneIdentity) -> Self {
        match super::record::decode(bytes) {
            Ok(record) if record.target.matches(current) => Self::Recognized(record),
            Ok(record) => Self::ForeignTarget { recorded: record.target, current: current.clone() },
            Err(DecodeFailure::Format { magic, recorded }) => {
                Self::UnrecognizedFormat { magic, recorded }
            }
            Err(DecodeFailure::Incomplete { observed, expected }) => {
                Self::Incomplete { observed, expected }
            }
            Err(DecodeFailure::Crc { region }) => Self::CrcMismatch { region },
        }
    }

    /// 按结构分类，不做身份比对：路径式 `inspect` 手上没有"当前目标"，
    /// 此时给出 `ForeignCheckUnavailable` 而不是 `ForeignTarget`。解码成功即 `Recognized`，
    /// 身份结论由调用方另行呈现——本变体不参与清理资格判定
    pub(crate) fn classify_structure(bytes: &[u8]) -> Self {
        match super::record::decode(bytes) {
            Ok(record) => Self::Recognized(record),
            Err(DecodeFailure::Format { magic, recorded }) => {
                Self::UnrecognizedFormat { magic, recorded }
            }
            Err(DecodeFailure::Incomplete { observed, expected }) => {
                Self::Incomplete { observed, expected }
            }
            Err(DecodeFailure::Crc { region }) => Self::CrcMismatch { region },
        }
    }

    #[cfg(test)]
    pub(crate) fn absent() -> Self {
        Self::Absent
    }

    pub(crate) fn not_ours(kind: EntryKind) -> Self {
        Self::NotOurs { kind }
    }

    pub(crate) fn unreadable(cause: Cause) -> Self {
        Self::Unreadable { cause }
    }

    /// 路径解析失败按 §4.7 映射：不存在的路径是 `Absent`，符号链接/目录/非普通文件
    /// 是 `NotOurs`，其余一律 `Unreadable`
    pub(crate) fn from_path_error(err: PathError) -> Self {
        match err {
            PathError::NotFound => Self::Absent,
            PathError::Symlink => Self::not_ours(EntryKind::Symlink),
            PathError::IsDirectory => Self::not_ours(EntryKind::Directory),
            PathError::NotDirectory | PathError::NotAFile => Self::not_ours(EntryKind::OtherFile),
            PathError::Inaccessible(reason) => Self::unreadable(Cause::Inaccessible(reason)),
            refused @ (PathError::Empty | PathError::Absolute | PathError::EscapesAnchor) => {
                Self::unreadable(Cause::Inaccessible(format!("{refused:?}")))
            }
        }
    }

    /// 只有 `Recognized` 携带的记录才可能进入提交与清理路径
    #[cfg(test)]
    pub(crate) fn recognized_record(&self) -> Option<&Record> {
        match self {
            Self::Recognized(record) => Some(record),
            _ => None,
        }
    }
}
