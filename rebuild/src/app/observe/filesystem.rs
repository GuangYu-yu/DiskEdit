//! 分区拓扑与分区内容类别。
//!
//! 拓扑来自宿主端口，内容类别来自签名识别。拓扑读不到时整体是 `Unknown`，
//! 不用空列表冒充"没有分区"；单个分区内容读不到时只有该分区是 `Unknown`。

use std::path::Path;

use diskedit_core::evidence::{Evidence, Generation, Observation, Source};
use diskedit_core::fs::ContentKind;

use super::disk::DiskFacts;
use super::snapshot::{PartitionFacts, Topology};
use crate::platform::{Partition, Platform, TargetHandle, probe};

/// 分区拓扑的产生点。
pub const SOURCE_TOPOLOGY: Source = Source::new("platform.partitions");
/// 分区内容类别的产生点。
pub const SOURCE_CONTENT: Source = Source::new("probe.filesystem");

/// 识别内容所需的前缀：覆盖 btrfs 在 64 KiB 处的签名。
pub const CONTENT_PREFIX_BYTES: usize = 0x10048;

/// 读取内核拓扑并为每个分区识别内容。
pub fn observe_topology(
    platform: &dyn Platform,
    path: &Path,
    disk: &DiskFacts,
    handle: Option<&dyn TargetHandle>,
    generation: Generation,
) -> Observation<Topology> {
    match platform.partitions(path) {
        Ok(partitions) => {
            let facts = partitions
                .into_iter()
                .map(|partition| {
                    let content = observe_content(handle, &partition, disk, generation);
                    PartitionFacts { partition, content }
                })
                .collect();
            Observation::known(Topology { partitions: facts }, SOURCE_TOPOLOGY, generation)
        }
        Err(error) => Observation::unknown(format!("读取分区拓扑失败：{error}")),
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
