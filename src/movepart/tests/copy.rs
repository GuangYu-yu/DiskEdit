#![allow(clippy::let_underscore_must_use)] // 测试的清理步骤有意忽略失败
use super::*;

/// 新 PARTUUID 的版本/variant 位落在磁盘字节序的 raw[7]/raw[8]
#[test]
pub(crate) fn new_partuuid_sets_v4_version_and_rfc4122_variant() {
    let g = new_partuuid().unwrap();
    assert_eq!(g[7] & 0xF0, 0x40, "version nibble must be 4");
    assert_eq!(g[8] & 0xC0, 0x80, "variant bits must be 10");
}