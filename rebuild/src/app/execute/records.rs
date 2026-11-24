//! 记录检视：把记录目录里的条目读成可判定的行。
//!
//! 只读，不写记录、不碰目标字节。每条条目先严格解码：解出来的记录才有标识；
//! 解不出的按原因分成"不是本实现的格式"与"损坏"，读不出字节记为"读不出"，不把
//! 读失败折成空条目。

use std::path::Path;

use diskedit_core::fingerprint::Fingerprint;
use diskedit_core::record::{self, FrameKind, RecordError};

use crate::platform::{Platform, RecordDir};

use super::ExecError;

/// 一条记录的结构判定。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecordState {
    /// 完整且事务已收尾。
    Valid,
    /// 完整但事务未收尾，可经 `recover` 处理。
    Unfinished,
    /// 记录里的目标身份与当前目标不符。
    IdentityMismatch,
    /// 标识不属于本实现的格式。
    UnknownFormat,
    /// 本实现的格式但校验不成立。
    Damaged,
    /// 条目读不出。
    Unreadable,
}

impl RecordState {
    /// 报告用的稳定名称。
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Valid => "valid",
            Self::Unfinished => "unfinished",
            Self::IdentityMismatch => "identity_mismatch",
            Self::UnknownFormat => "unknown_format",
            Self::Damaged => "damaged",
            Self::Unreadable => "unreadable",
        }
    }
}

/// 记录目录中的一条条目及其判定。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecordRow {
    pub path: std::path::PathBuf,
    pub name: String,
    pub bytes: u64,
    pub active: bool,
    /// 严格解码成功时的记录标识。
    pub id: Option<String>,
    pub state: RecordState,
}

/// 一次校验的结论。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VerifyVerdict {
    /// 唯一命中且已收尾。
    Valid,
    /// 唯一命中但未收尾。
    Unfinished,
    /// 唯一命中但不可用。
    Unusable,
    /// 没有命中。
    NotFound,
    /// 命中多条。
    NotUnique,
}

impl VerifyVerdict {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Valid => "valid",
            Self::Unfinished => "unfinished",
            Self::Unusable => "unusable",
            Self::NotFound => "not_found",
            Self::NotUnique => "not_unique",
        }
    }
}

/// 校验结果：命中的条目与全部检视过的条目。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifyReport {
    pub token: String,
    pub verdict: VerifyVerdict,
    pub hits: Vec<RecordRow>,
    pub rows: Vec<RecordRow>,
}

/// 列出目标记录目录中的条目。
pub fn list(
    platform: &dyn Platform,
    target: &Path,
    identity: Fingerprint,
) -> Result<Vec<RecordRow>, ExecError> {
    let store = platform.records(target, identity)?;
    let location = store.location().to_path_buf();
    let entries = store.entries()?;
    Ok(rows_of(
        platform,
        &RecordDir { location, entries },
        Some(identity),
    ))
}

/// 校验目标记录目录中与 `token` 相符的那一条。
pub fn verify(
    platform: &dyn Platform,
    target: &Path,
    identity: Fingerprint,
    token: &str,
) -> Result<VerifyReport, ExecError> {
    let rows = list(platform, target, identity)?;
    let hits: Vec<RecordRow> = rows
        .iter()
        .filter(|row| row.id.as_deref() == Some(token))
        .cloned()
        .collect();
    let verdict = match hits.as_slice() {
        [] => VerifyVerdict::NotFound,
        [row] => match row.state {
            RecordState::Valid => VerifyVerdict::Valid,
            RecordState::Unfinished => VerifyVerdict::Unfinished,
            RecordState::IdentityMismatch
            | RecordState::UnknownFormat
            | RecordState::Damaged
            | RecordState::Unreadable => VerifyVerdict::Unusable,
        },
        _ => VerifyVerdict::NotUnique,
    };
    Ok(VerifyReport {
        token: token.to_owned(),
        verdict,
        hits,
        rows,
    })
}

/// 扫描一个记录容器下的全部记录目录。
pub fn scan(platform: &dyn Platform, root: &Path) -> Result<Vec<RecordRow>, ExecError> {
    let dirs = platform.record_dirs(root)?;
    let mut rows = Vec::new();
    for dir in &dirs {
        rows.extend(rows_of(platform, dir, None));
    }
    Ok(rows)
}

fn rows_of(
    platform: &dyn Platform,
    dir: &RecordDir,
    expected: Option<Fingerprint>,
) -> Vec<RecordRow> {
    dir.entries
        .iter()
        .map(|entry| {
            let path = dir.location.join(&entry.name);
            let (id, state) = match platform.read_record(&dir.location, &entry.name) {
                Ok(bytes) => classify(&bytes, expected),
                Err(_) => (None, RecordState::Unreadable),
            };
            RecordRow {
                path,
                name: entry.name.clone(),
                bytes: entry.bytes,
                active: entry.active,
                id,
                state,
            }
        })
        .collect()
}

/// 严格解码一条记录，并给出结构判定。
fn classify(bytes: &[u8], expected: Option<Fingerprint>) -> (Option<String>, RecordState) {
    match record::decode(bytes) {
        Ok(decoded) => {
            let id = Some(record::id_of(&decoded));
            if expected.is_some_and(|expected| !decoded.body.target.matches(expected)) {
                return (id, RecordState::IdentityMismatch);
            }
            let terminal = decoded
                .frames
                .last()
                .is_some_and(|frame| frame.kind == FrameKind::Committed);
            if terminal {
                (id, RecordState::Valid)
            } else {
                (id, RecordState::Unfinished)
            }
        }
        Err(RecordError::ForeignFormat { .. }) => (None, RecordState::UnknownFormat),
        Err(_) => (None, RecordState::Damaged),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use diskedit_core::record::{Frame, RecordBody};

    fn record_bytes(target: Fingerprint, terminal: bool, digest: [u8; 32]) -> Vec<u8> {
        let body = RecordBody {
            target,
            geometry_generation: 3,
            plan_digest: digest,
        };
        let mut frames = Vec::new();
        if terminal {
            frames.push(Frame {
                seq: 0,
                kind: FrameKind::Committed,
                completed: true,
                irreversible: false,
                intent: b"commit".to_vec(),
                regions: Vec::new(),
            });
        }
        record::encode(&body, &frames).expect("记录可以编码")
    }

    #[test]
    fn committed_records_are_valid_and_carry_the_plan_id() {
        let target = Fingerprint::strong([7; 32]);
        let bytes = record_bytes(target, true, [9; 32]);
        let (id, state) = classify(&bytes, Some(target));
        assert_eq!(state, RecordState::Valid);
        assert_eq!(
            id.as_deref(),
            Some(record::id_of(&record::decode(&bytes).unwrap()).as_str())
        );
    }

    #[test]
    fn records_without_a_terminal_frame_are_unfinished() {
        let target = Fingerprint::strong([7; 32]);
        let (_, state) = classify(&record_bytes(target, false, [9; 32]), Some(target));
        assert_eq!(state, RecordState::Unfinished);
    }

    #[test]
    fn a_record_of_another_target_is_a_identity_mismatch() {
        let stored = Fingerprint::strong([7; 32]);
        let current = Fingerprint::strong([8; 32]);
        let (_, state) = classify(&record_bytes(stored, true, [9; 32]), Some(current));
        assert_eq!(state, RecordState::IdentityMismatch);
    }

    #[test]
    fn foreign_and_truncated_bytes_are_classified_apart() {
        let (id, state) = classify(b"plain text file!", None);
        assert_eq!(id, None);
        assert_eq!(state, RecordState::UnknownFormat);

        let target = Fingerprint::strong([7; 32]);
        let mut bytes = record_bytes(target, true, [9; 32]);
        bytes.truncate(bytes.len() - 1);
        let (id, state) = classify(&bytes, None);
        assert_eq!(id, None);
        assert_eq!(state, RecordState::Damaged);
    }
}
