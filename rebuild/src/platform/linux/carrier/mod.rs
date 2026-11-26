//! Linux 虚拟机载体：清单校验、QEMU 启动、串口会话与来宾工作进程。
//!
//! 宿主做不了的在线文件系统操作交给来宾；记录、锁与退出码仍留在宿主。来宾是同一
//! 二进制的隐藏入口 `__worker`，不对用户开放。只有配置了 `DISKEDIT_VM_BUNDLE`
//! 才存在载体，未配置时上层照常走本机路径。

pub mod manifest;
pub mod protocol;
pub mod qemu;
pub mod session;
pub mod worker;

use crate::platform::{Carrier, OnlineReport, OnlineRequest, PlatformResult};

/// 载体 bundle 的环境变量名。
pub const BUNDLE_ENV: &str = "DISKEDIT_VM_BUNDLE";

/// 从环境读取并校验载体；未配置时为 `None`，配置了但任何一项不符都是失败。
pub fn from_environment() -> PlatformResult<Option<Box<dyn Carrier>>> {
    match manifest::Bundle::resolve()? {
        Some(bundle) => Ok(Some(Box::new(VmCarrier { bundle }))),
        None => Ok(None),
    }
}

/// 一台校验通过的载体。
struct VmCarrier {
    bundle: manifest::Bundle,
}

impl Carrier for VmCarrier {
    fn online_grow(&self, request: &OnlineRequest) -> PlatformResult<OnlineReport> {
        let disk = session::Disk::open(&request.disk)?;
        let mut guest = session::Session::open(&self.bundle, &disk, request, carrier_token())?;
        let report = guest.online_grow(request)?;
        Ok(report)
    }
}

/// 会话令牌：只用来发现宿主与来宾接错通道，不承担安全边界。
fn carrier_token() -> String {
    use std::hash::{BuildHasher, Hasher};
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_nanos())
        .unwrap_or(0);
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    hasher.write_u128(nanos);
    let seed = hasher.finish();
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    hasher.write_u64(seed ^ u64::from(std::process::id()));
    format!("{seed:016x}{:016x}", hasher.finish())
}
