//! LVM 前置证明：缩容只在元数据区之前才放行。

pub mod proof;

pub use proof::{PvRefusal, prove_shrink};
