//! 目标级事实：种类、字节长度、逻辑块大小与占用。
//!
//! 句柄只在这里打开一次，分区表与分区内容的读取复用同一个句柄，避免同一次
//! 观察看到不同时刻的目标。

use std::path::Path;

use diskedit_core::evidence::{Generation, Observation, Source};

use crate::platform::{Occupancy, Platform, TargetHandle, TargetKind};

/// 目标打开的读数点。
pub const SOURCE_OPEN: Source = Source::new("platform.open");
/// 镜像文件的逻辑块宽度读数点。
pub const SOURCE_IMAGE_BLOCK: Source = Source::new("image.logical-block");
/// 占用状态的读数点。
pub const SOURCE_OCCUPANCY: Source = Source::new("platform.occupancy");

/// 镜像文件的逻辑块宽度：分区表按 512 字节逻辑块布局，这不是设备级扇区。
pub const IMAGE_LOGICAL_BLOCK: u64 = 512;

/// 目标自身的三个事实。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiskFacts {
    pub kind: Observation<TargetKind>,
    pub byte_len: Observation<u64>,
    pub sector_size: Observation<u64>,
}

/// 以只读方式打开目标并读出目标级事实。
///
/// 返回的句柄用于后续分区表与分区内容读取；打开失败时三者都是 `Unknown`，
/// 且不返回句柄。
pub fn observe(
    platform: &dyn Platform,
    path: &Path,
    generation: Generation,
) -> (DiskFacts, Option<Box<dyn TargetHandle>>) {
    match platform.open_target_read_only(path) {
        Ok(handle) => {
            let kind = Observation::known(handle.kind(), SOURCE_OPEN, generation);
            let byte_len = Observation::known(handle.len(), SOURCE_OPEN, generation);
            let sector_size = match handle.sector_size() {
                Some(value) => Observation::known(value, SOURCE_OPEN, generation),
                None => match handle.kind() {
                    TargetKind::File => {
                        Observation::known(IMAGE_LOGICAL_BLOCK, SOURCE_IMAGE_BLOCK, generation)
                    }
                    TargetKind::Block => {
                        Observation::unknown("块设备未报告逻辑块大小，无法按扇区定位")
                    }
                },
            };
            (
                DiskFacts {
                    kind,
                    byte_len,
                    sector_size,
                },
                Some(handle),
            )
        }
        Err(error) => {
            let why = format!("打开目标失败：{error}");
            (
                DiskFacts {
                    kind: Observation::unknown(why.clone()),
                    byte_len: Observation::unknown(why.clone()),
                    sector_size: Observation::unknown(why),
                },
                None,
            )
        }
    }
}

/// 读取占用视图；读不到时记 `Unknown`，不用"空占用"冒充空闲。
pub fn observe_occupancy(
    platform: &dyn Platform,
    path: &Path,
    generation: Generation,
) -> Observation<Occupancy> {
    match platform.occupancy(path) {
        Ok(view) => Observation::known(view, SOURCE_OCCUPANCY, generation),
        Err(error) => Observation::unknown(format!("读取占用状态失败：{error}")),
    }
}
