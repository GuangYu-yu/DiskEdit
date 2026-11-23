//! 分区拓扑与分区内容类别。
//!
//! 拓扑来自宿主端口，内容类别来自签名识别。拓扑读不到时整体是 `Unknown`，
//! 不用空列表冒充"没有分区"；单个分区内容读不到时只有该分区是 `Unknown`。

use std::path::Path;

use diskedit_core::evidence::{Evidence, Generation, Observation, Source};
use diskedit_core::fs::ContentKind;
use diskedit_core::table::GptEvidence;

use super::disk::DiskFacts;
use super::snapshot::{PartitionFacts, Topology};
use super::table::TableFacts;
use crate::platform::{Partition, Platform, TargetHandle, TargetKind, probe};

/// 块设备分区拓扑的产生点。
pub const SOURCE_TOPOLOGY: Source = Source::new("platform.partitions");
/// 镜像文件分区拓扑的产生点：从盘上分区表读出。
pub const SOURCE_TABLE: Source = Source::new("target.table");
/// 分区内容类别的产生点。
pub const SOURCE_CONTENT: Source = Source::new("probe.filesystem");

/// 识别内容所需的前缀：覆盖 btrfs 在 64 KiB 处的签名。
pub const CONTENT_PREFIX_BYTES: usize = 0x10048;

/// 读取内核拓扑并为每个分区识别内容。
pub fn observe_topology(
    platform: &dyn Platform,
    path: &Path,
    disk: &DiskFacts,
    table: &TableFacts,
    handle: Option<&dyn TargetHandle>,
    generation: Generation,
) -> Observation<Topology> {
    // 镜像文件没有内核分区视图，拓扑直接取盘上分区表；块设备以内核视图为准。
    let is_file = matches!(
        disk.kind,
        Observation::Known {
            value: TargetKind::File,
            ..
        }
    );
    let (partitions, source) = if is_file {
        match file_partitions(table) {
            Ok(partitions) => (partitions, SOURCE_TABLE),
            Err(why) => return Observation::unknown(why),
        }
    } else {
        match platform.partitions(path) {
            Ok(partitions) => (partitions, SOURCE_TOPOLOGY),
            Err(error) => return Observation::unknown(format!("读取分区拓扑失败：{error}")),
        }
    };
    let facts = partitions
        .into_iter()
        .map(|partition| {
            let content = observe_content(handle, &partition, disk, generation);
            PartitionFacts { partition, content }
        })
        .collect();
    Observation::known(Topology { partitions: facts }, source, generation)
}

/// 从盘上分区表推出现有条目；表不可用时给出原因，不返回空列表。
fn file_partitions(table: &TableFacts) -> Result<Vec<Partition>, String> {
    match &table.gpt {
        Observation::Known {
            value: GptEvidence::Usable(copy),
            ..
        } => Ok(copy
            .entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| !entry.partition_type_guid.is_zero())
            .map(|(index, entry)| Partition {
                number: index as u32 + 1,
                start_lba: entry.starting_lba,
                sectors: entry.ending_lba - entry.starting_lba + 1,
            })
            .collect()),
        Observation::Known {
            value: GptEvidence::Damaged(error),
            ..
        } => Err(format!("GPT 损坏，无法给出分区拓扑：{error}")),
        Observation::Known {
            value: GptEvidence::Absent,
            ..
        } => match &table.mbr {
            Observation::Known {
                value: Evidence::Present(mbr),
                ..
            } => Ok(mbr
                .entries
                .iter()
                .enumerate()
                .filter(|(_, entry)| !entry.is_empty())
                .map(|(index, entry)| Partition {
                    number: index as u32 + 1,
                    start_lba: u64::from(entry.start_lba),
                    sectors: u64::from(entry.size_lba),
                })
                .collect()),
            Observation::Known {
                value: Evidence::Absent,
                ..
            } => Ok(Vec::new()),
            Observation::Known {
                value: Evidence::UnknownFormat { why },
                ..
            } => Err(format!("MBR 无法解释：{why}")),
            Observation::Known {
                value: Evidence::Unreadable { why },
                ..
            } => Err(format!("MBR 读不出：{why}")),
            Observation::Unknown { why } => Err(format!("MBR 未知：{why}")),
            Observation::NotApplicable { why } => Err(format!("MBR 不适用：{why}")),
        },
        Observation::Unknown { why } => Err(format!("GPT 未知：{why}")),
        Observation::NotApplicable { why } => Err(format!("GPT 不适用：{why}")),
    }
}

fn observe_content(
    handle: Option<&dyn TargetHandle>,
    partition: &Partition,
    disk: &DiskFacts,
    generation: Generation,
) -> Observation<ContentKind> {
    let Some(handle) = handle else {
        return Observation::unknown("目标未能打开，分区内容未读取");
    };
    let Some(sector) = disk.sector_size.value().copied() else {
        return Observation::unknown("逻辑块大小未知，无法定位分区起点");
    };
    let offset = match partition.start_lba.checked_mul(sector) {
        Some(offset) => offset,
        None => return Observation::unknown("分区起点超出可寻址范围"),
    };
    let capacity = partition.sectors.saturating_mul(sector);
    let want = CONTENT_PREFIX_BYTES.min(usize::try_from(capacity).unwrap_or(usize::MAX));
    if want == 0 {
        return Observation::unknown("分区长度为零，没有可识别的签名");
    }
    let prefix = match super::read_exact_at(handle, offset, want) {
        Ok(bytes) => bytes,
        Err(why) => return Observation::unknown(why),
    };
    match probe::identify(&prefix) {
        Evidence::Present(kind) => Observation::known(kind, SOURCE_CONTENT, generation),
        Evidence::UnknownFormat { why } | Evidence::Unreadable { why } => Observation::unknown(why),
        Evidence::Absent => Observation::unknown("签名识别未给出结论"),
    }
}
