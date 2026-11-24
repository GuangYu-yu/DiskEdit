//! `records` 的判定：列出、校验与扫描记录。
//!
//! 记录字节由纯核解码；本模块只决定"有哪些记录、某条是否可信"，不碰目录内容以外
//! 的任何宿主状态。

use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::app::observe::snapshot::RecordSnapshot;

use super::report::{self, RecordEntry, RecordsReport, Report};
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
    let path = snapshot.record.file.clone().unwrap_or_default();
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
        Ok(decoded) => format!(
            "geometry generation {}, plan digest {}",
            decoded.body.geometry_generation,
            hex(&decoded.body.plan_digest)
        ),
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
