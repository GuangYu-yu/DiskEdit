//! 目标现场身份的只读采集：命令层唯一的「打开目标 → 套声明名 → 探测 → 三态结论」入口。
//!
//! 三态沿用 [`crate::domain::Fact`]：
//! - [`crate::domain::Fact::Known`] 读到了可比的现场身份；
//! - [`crate::domain::Fact::Absent`] 是确认不适用——平台给不出稳定身份证据，或声明名
//!   按目标形态套不上；不适用本身已证明，不是读不出；
//! - [`crate::domain::Fact::Undecidable`] 是打不开、探测失败等读不出的情形，附带原因。
//!
//! 各命令只消费这一份三态，不再各自把 `Result` 折成 `Option` 后让出口把「读不出」
//! 说成「没有身份 / 不适用」。退出码仍由命令层按三态各自归责，本模块不产生出口结论。

#![cfg_attr(not(target_os = "linux"), allow(dead_code, unused_imports))]

use std::path::Path;

use crate::dev::{DeclaredIdentity, FileSource};
use crate::domain::Fact;
use crate::recovery::SceneIdentity;

/// 按节点形态只读打开目标；`stat` 判不出形态时带原因失败，不按镜像去开。
/// 镜像与块设备两条打开路径在这里收敛成一处，命令层不再各写一遍形态匹配。
#[cfg(target_os = "linux")]
pub(crate) fn open_read_only_target(
    path: &Path,
    sector_size: Option<u64>,
) -> Result<FileSource, String> {
    let opened = match crate::dev::target_node_is_block(path) {
        Fact::Known(true) => FileSource::open_read_only(path),
        Fact::Known(false) | Fact::Absent => FileSource::open_read_only_image(path, sector_size),
        Fact::Undecidable(why) => {
            return Err(format!("the target's node shape cannot be determined: {why}"))
        }
    };
    opened.map_err(|e| format!("the target cannot be opened read-only: {e}"))
}

/// 已打开句柄的现场身份三态。探测读不出落 [`Fact::Undecidable`]；平台确认无稳定身份
/// 证据落 [`Fact::Absent`]——前者是环境故障，后者是目标形态上给不出可比身份。
/// 错误归类只经 [`crate::recovery::classify_probe`] 一处，本函数不再另写 match。
#[cfg(target_os = "linux")]
pub(crate) fn scene_from_source(src: &FileSource) -> Fact<SceneIdentity> {
    match crate::recovery::classify_probe(crate::platform::probe::probe_of(src)) {
        Fact::Known(probe) => Fact::Known(SceneIdentity::of_source(&probe, src)),
        Fact::Absent => Fact::Absent,
        Fact::Undecidable(why) => Fact::Undecidable(why),
    }
}

/// 按路径采集现场身份三态。打开失败、探测失败是 [`Fact::Undecidable`]；声明名套不上
/// 目标形态、平台给不出稳定身份证据是 [`Fact::Absent`]。
#[cfg(target_os = "linux")]
pub(crate) fn scene_by_path(
    path: &Path,
    declared: Option<&DeclaredIdentity>,
    sector_size: Option<u64>,
) -> Fact<SceneIdentity> {
    let mut src = match open_read_only_target(path, sector_size) {
        Ok(src) => src,
        Err(why) => return Fact::Undecidable(why),
    };
    if let Some(name) = declared {
        match src.identity.with_declared(name) {
            Ok(identity) => src.identity = identity,
            // 声明名只适用于无稳定身份证据的块设备：套不上说明这个名字对本目标不适用，
            // 是已证明的不适用，不是读不出
            Err(_) => return Fact::Absent,
        }
    }
    scene_from_source(&src)
}
