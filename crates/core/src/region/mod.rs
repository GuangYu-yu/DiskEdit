//! 区间集合：半开区间、合并、裁剪、求差与重叠判定。
//!
//! 所有偏移与长度都以字节为单位。区间恒为半开形式 `[start, end)`，
//! 因此相邻区间 `[0,4)` 与 `[4,8)` 可以合并为 `[0,8)`。

pub mod diff;
pub mod set;

pub use diff::RegionDiff;
pub use set::{Region, RegionError, RegionSet};
