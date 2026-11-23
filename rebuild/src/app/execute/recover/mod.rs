//! 记录恢复：回滚表区原像、放弃回滚与清理记录。
//!
//! 恢复是目标之外的写路径：读记录目录、按纯恢复器的结论决定动作，只在
//! [`rollback`] 中按记录里的表区原像写回目标字节。所有目标写入同样经过唯一
//! 写通道与 [`crate::app::execute::Recorded`] 令牌。

pub mod abandon;
pub mod cleanup;
pub mod engine;
pub mod list;
pub mod rollback;

pub use engine::{Action, Mode, RecoveryFailure, Summary, run};
