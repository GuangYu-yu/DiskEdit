//! 虚拟机载体：清单校验、协议、工作进程、远端会话与放置。
//!
//! 载体只改变"步骤在哪里写目标"，记录、锁、放置判定与退出码仍全部留在宿主。
//! `__worker` 是同一二进制里的来宾入口，不对用户开放。

pub mod dispatch;
pub mod manifest;
pub mod placement;
pub mod protocol;
pub mod remote;
pub mod worker;

use std::hash::{BuildHasher, Hasher};
use std::time::{SystemTime, UNIX_EPOCH};

/// 生成会话令牌与会话号。
///
/// 它只用来发现宿主与来宾接错通道，不承担安全边界；因此不需要操作系统随机源，
/// 也不在应用层引入平台路径。
pub(crate) fn token() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_nanos())
        .unwrap_or(0);
    let mut first = std::collections::hash_map::RandomState::new().build_hasher();
    first.write_u128(nanos);
    let mut second = std::collections::hash_map::RandomState::new().build_hasher();
    second.write_u64(std::process::id().into());
    second.write_u128(nanos);
    format!("{:016x}{:016x}", first.finish(), second.finish())
}
