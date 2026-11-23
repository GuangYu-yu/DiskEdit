//! 目标身份事实：指纹与其可比较等级。

use std::path::Path;

use diskedit_core::evidence::{Generation, Observation, Source};
use diskedit_core::fingerprint::Fingerprint;

use crate::platform::Platform;

/// 指纹的产生点。
pub const SOURCE: Source = Source::new("platform.fingerprint");

/// 读取目标指纹；读不到时记 `Unknown`，不构造占位指纹。
pub fn observe(
    platform: &dyn Platform,
    path: &Path,
    generation: Generation,
) -> Observation<Fingerprint> {
    match platform.fingerprint(path) {
        Ok(fingerprint) => Observation::known(fingerprint, SOURCE, generation),
        Err(error) => Observation::unknown(format!("读取目标身份失败：{error}")),
    }
}
