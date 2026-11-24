//! 放置结果到实际执行者的转换。
//!
//! 本机执行不需要会话；虚拟机执行在写入任何字节之前启动并完成握手，启动失败
//! 因此仍属于"写前不支持"，不会留下需要恢复的记录。

use std::path::Path;

use super::placement::Placement;
use super::remote::{self, Session};
use crate::app::execute::ExecError;
use crate::platform::Platform;

/// 按放置启动执行者；本机放置返回 `None`。
#[allow(clippy::too_many_arguments)]
pub fn open(
    platform: &dyn Platform,
    placement: &Placement,
    target: &Path,
    sector_size: u64,
    disk_size: u64,
    plan_digest: &[u8; 32],
    job: &str,
) -> Result<Option<Session>, ExecError> {
    match placement {
        Placement::Native => Ok(None),
        Placement::Vm(bundle) => {
            let launch = remote::Launch {
                target,
                sector_size,
                disk_size,
                plan_digest,
                job,
            };
            Session::open(platform, bundle, launch)
                .map(Some)
                .map_err(ExecError::Unsupported)
        }
    }
}
