//! 目标身份事实：指纹与其可比较等级。
//!
//! 自动指纹来自宿主（设备 WWN/serial、镜像 inode 等）。只有自动身份不可得时，
//! 用户用 `--identity` 声明的名字才参与：它派生一个稳定指纹作为记录与锁的命名
//! 空间。能给自身稳定身份的目标不接受声明名，避免两套身份并存。

use std::path::Path;

use diskedit_core::evidence::{Generation, Observation, Source};
use diskedit_core::fingerprint::{Fingerprint, Strength};

use crate::platform::Platform;

/// 自动指纹的产生点。
pub const SOURCE: Source = Source::new("platform.fingerprint");
/// 声明指纹的产生点。
pub const DECLARED_SOURCE: Source = Source::new("--identity");

/// 读取目标指纹；读不到时记 `Unknown`，不构造占位指纹。
///
/// 自动指纹可用（`Strong`）时忽略声明名；自动指纹不可用时才用声明名派生。
pub fn observe(
    platform: &dyn Platform,
    path: &Path,
    generation: Generation,
    declared: Option<&str>,
) -> Observation<Fingerprint> {
    match platform.fingerprint(path) {
        Ok(fingerprint) if fingerprint.strength() == Strength::Strong => {
            Observation::known(fingerprint, SOURCE, generation)
        }
        Ok(_) => match declared {
            Some(name) => {
                Observation::known(Fingerprint::declared(name), DECLARED_SOURCE, generation)
            }
            None => {
                Observation::unknown("automatic identity unavailable and --identity not declared")
            }
        },
        Err(error) => Observation::unknown(format!("failed to read target identity: {error}")),
    }
}
