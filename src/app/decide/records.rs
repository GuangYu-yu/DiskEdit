//! `records` 的判定：列出、校验、扫描记录与呈现记录现场。
//!
//! 记录字节由纯核解码；本模块只决定"有哪些记录、某条是否可信"，不碰目录内容以外
//! 的任何宿主状态。

use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::app::observe::snapshot::{RecordSiteSnapshot, RecordSnapshot};

use super::report::{self, RecordEntry, RecordSiteReport, RecordVerdict, RecordsReport, Report};
use super::{Decision, Refusal, Step, StepKind};

/// 列出目标的记录。
pub fn list(snapshot: Option<&RecordSnapshot>, root: Option<PathBuf>) -> Decision {
    let mut records = Vec::new();
    if let Some(snapshot) = snapshot {
        if snapshot.record.directory.is_none() {
            return Decision::Refuse(Refusal::RecordDirectoryUnresolved {
                target: snapshot.target.display.clone(),
            });
        }
        if let (Some(bytes), Some(path)) = (&snapshot.record.bytes, &snapshot.record.file) {
            records.push(entry(path, bytes));
        }
    }
    let report = RecordsReport {
        root,
        records,
        scanned: None,
        complete: true,
    };
    report::decision(
        Report::Records(report),
        vec![Step::new(StepKind::ListRecords, Vec::new())],
    )
}

/// 校验一条记录。
pub fn verify(snapshot: &RecordSnapshot, token: &str) -> Decision {
    let Some(bytes) = &snapshot.record.bytes else {
        return Decision::Refuse(Refusal::RecordNotFound {
            token: token.to_owned(),
        });
    };
    let id = record_id(bytes);
    if id != token {
        return Decision::Refuse(Refusal::RecordNotFound {
            token: token.to_owned(),
        });
    }
    let Some(path) = snapshot.record.file.clone() else {
        return Decision::Refuse(Refusal::RecordNotFound {
            token: token.to_owned(),
        });
    };
    let report = RecordsReport {
        root: snapshot.record.directory.clone(),
        records: vec![entry(&path, bytes)],
        scanned: None,
        complete: true,
    };
    report::decision(
        Report::Records(report),
        vec![Step::new(
            StepKind::VerifyRecord {
                token: token.to_owned(),
            },
            Vec::new(),
        )],
    )
}

/// 扫描一个目录树下的记录文件；字节由命令层读好传入。
pub fn scan_with(root: &Path, records: &[(PathBuf, Vec<u8>)]) -> Decision {
    let records: Vec<RecordEntry> = records
        .iter()
        .map(|(path, bytes)| entry(path, bytes))
        .collect();
    let report = RecordsReport {
        root: None,
        records,
        scanned: Some(root.to_path_buf()),
        complete: true,
    };
    report::decision(
        Report::Records(report),
        vec![Step::new(StepKind::ScanRecords, Vec::new())],
    )
}

/// 呈现 `records show` 的现场：记录状态与占用来源。
pub fn show(snapshot: &RecordSiteSnapshot) -> Decision {
    let Some(directory) = &snapshot.record.directory else {
        // 目标给不出记录目录的键：没有可检查的记录位置，读侧如实报告而不是拒绝。
        let report = RecordSiteReport {
            evidence: None,
            record: RecordVerdict::Unresolved {
                why: snapshot
                    .record
                    .unavailable
                    .clone()
                    .unwrap_or_else(|| "the target exposes no stable identity".to_owned()),
            },
            occupancy: snapshot.occupancy.clone(),
        };
        return report::decision(
            Report::Show(report),
            vec![Step::new(StepKind::Inspect, Vec::new())],
        );
    };
    if !snapshot.record.directory_exists {
        return Decision::Refuse(Refusal::RecordStoreMissing {
            path: directory.display().to_string(),
        });
    }
    let record = match (&snapshot.record.bytes, &snapshot.record.unavailable) {
        (Some(bytes), _) => {
            let decoded = match diskedit_core::record::decode(bytes) {
                Ok(decoded) => decoded,
                Err(error) => {
                    return Decision::Refuse(Refusal::RecordDamaged {
                        why: damage(&error),
                    });
                }
            };
            // 记录里写着它属于哪个目标：能自行证明身份的目标必须与之相符，否则记录是
            // 别的工作留下的证据，不能被当作本目标的现状。
            if let Some(identity) = snapshot.target.identity
                && decoded.body.target != identity
            {
                return Decision::Refuse(Refusal::RecordForeign {
                    why: "identity: the record belongs to a different target — it must not be \
                          cleaned up or replayed for this target"
                        .to_owned(),
                });
            }
            RecordVerdict::Present {
                bytes: bytes.len() as u64,
                decoded: decode_summary(&decoded),
            }
        }
        (None, Some(_)) => {
            let path = snapshot
                .record
                .file
                .clone()
                .unwrap_or_else(|| directory.clone());
            return Decision::Refuse(Refusal::RecordUnreadable {
                path: path.display().to_string(),
            });
        }
        (None, None) => RecordVerdict::Absent,
    };
    let report = RecordSiteReport {
        evidence: snapshot.record.file.clone(),
        record,
        occupancy: snapshot.occupancy.clone(),
    };
    report::decision(
        Report::Show(report),
        vec![Step::new(StepKind::Inspect, Vec::new())],
    )
}

fn decode_summary(decoded: &diskedit_core::record::DecodedRecord) -> String {
    format!("plan digest {}", hex(&decoded.body.plan_digest))
}

/// 把解码失败归类成三种结构结论：截断、校验不符、格式不认。
fn damage(error: &diskedit_core::record::RecordError) -> String {
    use diskedit_core::record::RecordError;
    match error {
        RecordError::TooShort { .. } | RecordError::FrameIncomplete => {
            "structure: the record is shorter than its declared length — keep the file for manual \
             inspection"
                .to_owned()
        }
        RecordError::HeaderCrcMismatch
        | RecordError::PayloadCrcMismatch
        | RecordError::BodyCrcMismatch
        | RecordError::FrameCrcMismatch { .. } => {
            "structure: the record's CRC does not match — keep the file for manual inspection"
                .to_owned()
        }
        RecordError::ForeignFormat { .. }
        | RecordError::PayloadTooLarge(_)
        | RecordError::TrailingBytes
        | RecordError::FrameSequence { .. }
        | RecordError::UnknownFrameKind(_)
        | RecordError::ReservedFrameFlags(_)
        | RecordError::FramesAfterTerminal
        | RecordError::IntentTooLong
        | RecordError::TooManyRegions
        | RecordError::PreimageLengthMismatch
        | RecordError::PostimageLengthMismatch
        | RecordError::PreimageTooLarge => {
            "structure: the record's magic is not recognized or its payload does not match the \
             layout this build reads — keep the file; `diskedit release` archives it as \
             `record.<N>.abandoned` when the layout can be attributed to this build (pass \
             `--discard-replay` when the recorded ranges cannot be enumerated) and refuses \
             otherwise, and nothing may write to this target until then"
                .to_owned()
        }
    }
}

/// 一条记录的稳定标识：记录字节的 SHA-256 前 16 字节。
pub fn record_id(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut text = String::with_capacity(32);
    for byte in &digest[..16] {
        text.push_str(&format!("{byte:02x}"));
    }
    text
}

fn entry(path: &Path, bytes: &[u8]) -> RecordEntry {
    let decoded = match diskedit_core::record::decode(bytes) {
        Ok(decoded) => format!("plan digest {}", hex(&decoded.body.plan_digest)),
        Err(error) => format!("record is unreadable: {error:?}"),
    };
    RecordEntry {
        id: record_id(bytes),
        path: path.to_path_buf(),
        bytes: bytes.len() as u64,
        decoded,
    }
}

fn hex(bytes: &[u8]) -> String {
    let mut text = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        text.push_str(&format!("{byte:02x}"));
    }
    text
}
