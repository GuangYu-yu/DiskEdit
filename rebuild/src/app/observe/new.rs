//! `new` 的写侧现场：表区字节、覆盖探测所需的比较区间、写前摘要与记录状态。
//!
//! 判定层需要"表区长什么样""哪些字节将被覆盖""盘上有没有未收尾的记录"。
//! 这些事实一次采齐，避免判定与执行之间再读同一批扇区。

use std::path::PathBuf;

use diskedit_core::record;
use diskedit_core::region::Region;
use diskedit_core::table::Guid;
use diskedit_core::table::create;
use diskedit_core::table::stamp::{self, table_stamp};

use crate::platform::{Occupancy, Platform, TargetHandle};

use super::records::RecordState;
use super::snapshot::{CaptureError, TargetFacts, open_target, record_state};
use super::table::{self, TableView};
use super::{Command, Request, TableFormat};

/// 一段被采集的表区字节。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CapturedRegion {
    /// 冲突消息里对这段区间的称呼。
    pub label: &'static str,
    pub region: Region,
    pub bytes: Vec<u8>,
}

/// 盘上已有的记录。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PriorRecord {
    /// 记录绑定的摘要。
    pub plan_digest: [u8; 32],
    /// 最后一帧是否已终结。
    pub pending: bool,
}

/// 记录的存在形态。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PriorState {
    Absent,
    Present(PriorRecord),
    /// 记录文件存在但无法解码。
    Unreadable,
}

/// `new` 消费的现场。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewSnapshot {
    pub target: TargetFacts,
    pub occupancy: Occupancy,
    pub record: RecordState,
    pub prior: PriorState,
    pub table: TableView,
    /// 目标本身是不是块设备上的分区节点。
    pub is_partition_node: bool,
    /// 覆盖探测与冲突判定共享的比较区间，按起点升序。
    pub captured: Vec<CapturedRegion>,
    /// 写前首/次/末三扇区的摘要。
    pub stamp: [u8; 32],
    /// 当 `new` 的目标格式为 MBR 时，额外需要清掉的 GPT 签名绝对偏移。
    pub signature_sites: Vec<u64>,
    /// 新 GPT 的磁盘 GUID；由宿主随机源生成，判定层只使用。
    pub disk_guid: Guid,
}

/// 采集 `new` 的现场。
pub fn capture_new(
    platform: &dyn Platform,
    request: &Request,
) -> Result<NewSnapshot, CaptureError> {
    let target = open_target(platform, request)?;
    let handle = platform.open_target_read_only(&target.path)?;
    let sector_size = request.sector_size().unwrap_or(target.sector_size);
    let Command::New { table, .. } = request.command() else {
        return Err(CaptureError::Unreadable {
            why: "capture_new called for a different command".to_owned(),
        });
    };
    let format = *table;
    let table_view = table::observe(handle.as_ref(), request.sector_size(), handle.sector_size())
        .map_err(|error| CaptureError::Unreadable {
        why: error.to_string(),
    })?;
    let record = record_state(platform, &target)?;
    let occupancy = platform.occupancy(&target.path)?;
    let is_partition_node = platform.is_partition_node(&target.path);
    let (captured, signature_sites) = match format {
        TableFormat::Gpt => (
            capture_gpt(handle.as_ref(), target.bytes, sector_size)?,
            Vec::new(),
        ),
        TableFormat::Mbr => capture_mbr(handle.as_ref(), target.bytes, sector_size)?,
    };
    let stamp = stamp_of(handle.as_ref(), target.bytes, sector_size)?;
    let prior = prior_state(&record);
    let disk_guid = random_guid(platform)?;
    Ok(NewSnapshot {
        target: TargetFacts {
            sector_size,
            ..target
        },
        occupancy,
        record,
        prior,
        table: table_view,
        is_partition_node,
        captured,
        stamp,
        signature_sites,
        disk_guid,
    })
}

/// RFC 4122 版本 4 的 GUID；熵只从平台端口取。
fn random_guid(platform: &dyn Platform) -> Result<Guid, CaptureError> {
    let mut bytes = platform
        .entropy(16)
        .map_err(|error| CaptureError::Unreadable {
            why: error.to_string(),
        })?;
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    Ok(Guid(bytes.try_into().expect("熵长度为 16")))
}

fn capture_gpt(
    handle: &dyn TargetHandle,
    total_bytes: u64,
    sector_size: u64,
) -> Result<Vec<CapturedRegion>, CaptureError> {
    let Ok(layout) = create::gpt_layout(total_bytes, sector_size) else {
        return Ok(Vec::new());
    };
    let labels = [
        "protective MBR",
        "primary GPT header",
        "primary entry array",
        "backup entry array",
        "backup GPT header",
    ];
    let mut captured = Vec::new();
    for (label, region) in labels.into_iter().zip(layout.regions()) {
        let bytes = read_region(handle, region)?;
        captured.push(CapturedRegion {
            label,
            region,
            bytes,
        });
    }
    Ok(captured)
}

fn capture_mbr(
    handle: &dyn TargetHandle,
    total_bytes: u64,
    sector_size: u64,
) -> Result<(Vec<CapturedRegion>, Vec<u64>), CaptureError> {
    if total_bytes / sector_size < 3 {
        return Ok((Vec::new(), Vec::new()));
    }
    let last_lba = total_bytes / sector_size - 1;
    let mut regions = Vec::new();
    for (label, lba) in [
        ("the MBR sector", 0u64),
        ("the GPT header sector this msdos table zeros", 1),
        ("the last sector this msdos table zeros", last_lba),
    ] {
        let start = lba.saturating_mul(sector_size);
        let len = sector_size.min(total_bytes.saturating_sub(start));
        let region = Region::sized(start, len).expect("区间自洽");
        regions.push((label, region));
    }
    let mut sites = Vec::new();
    for offset in create::signature_sites(total_bytes, sector_size) {
        let covered = regions
            .iter()
            .any(|(_, region)| region.contains_offset(offset));
        if covered {
            continue;
        }
        sites.push(offset);
        let len = 8u64.min(total_bytes.saturating_sub(offset));
        let region = Region::sized(offset, len).expect("区间自洽");
        regions.push(("the GPT header bytes this msdos table clears", region));
    }
    regions.sort_by_key(|(_, region)| region.start());
    let captured = regions
        .into_iter()
        .map(|(label, region)| {
            let bytes = read_region(handle, region)?;
            Ok(CapturedRegion {
                label,
                region,
                bytes,
            })
        })
        .collect::<Result<Vec<_>, CaptureError>>()?;
    Ok((captured, sites))
}

fn stamp_of(
    handle: &dyn TargetHandle,
    total_bytes: u64,
    sector_size: u64,
) -> Result<[u8; 32], CaptureError> {
    if total_bytes == 0 || sector_size == 0 {
        return Ok(table_stamp(&[]));
    }
    let sectors = stamp::stamp_regions(total_bytes, sector_size)
        .into_iter()
        .map(|region| read_region(handle, region))
        .collect::<Result<Vec<_>, CaptureError>>()?;
    let parts: Vec<&[u8]> = sectors.iter().map(Vec::as_slice).collect();
    Ok(table_stamp(&parts))
}

fn read_region(handle: &dyn TargetHandle, region: Region) -> Result<Vec<u8>, CaptureError> {
    let mut bytes = vec![0u8; region.len() as usize];
    let mut filled = 0;
    while filled < bytes.len() {
        let at =
            region
                .start()
                .checked_add(filled as u64)
                .ok_or_else(|| CaptureError::Unreadable {
                    why: "read offset overflow".to_owned(),
                })?;
        let read =
            handle
                .read_at(at, &mut bytes[filled..])
                .map_err(|error| CaptureError::Unreadable {
                    why: error.to_string(),
                })?;
        if read == 0 {
            return Err(CaptureError::Unreadable {
                why: format!("reading {} bytes at {at} ended early", bytes.len()),
            });
        }
        filled += read;
    }
    Ok(bytes)
}

fn prior_state(record: &RecordState) -> PriorState {
    let Some(bytes) = record.bytes.as_ref() else {
        return PriorState::Absent;
    };
    match record::decode(bytes) {
        Ok(decoded) => {
            let pending = decoded
                .frames
                .last()
                .map(|frame| !frame.kind.is_terminal())
                .unwrap_or(true);
            PriorState::Present(PriorRecord {
                plan_digest: decoded.body.plan_digest,
                pending,
            })
        }
        Err(_) => PriorState::Unreadable,
    }
}

/// 记录文件的目录与路径；执行器写记录前使用。
pub fn record_paths(
    platform: &dyn Platform,
    identity: diskedit_core::fingerprint::Fingerprint,
) -> Option<(PathBuf, PathBuf)> {
    let root = platform.state_root()?;
    let directory = root.join("images").join(super::records::hex16(identity));
    let file = directory.join(super::records::RECORD_FILE);
    Some((directory, file))
}
