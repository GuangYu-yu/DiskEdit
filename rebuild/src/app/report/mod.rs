//! 报告：终态到退出码的唯一映射、结构化输出与恢复文案。
//!
//! 退出码只在本模块产生；任何调用点都读 [`exit_of`] 或 [`exit_of_refusal`]，
//! 不得自行选码。文本输出不作契约，机器可读输出由 [`json`] 子模块给出。

pub mod exit;
pub mod json;
pub mod text;
pub mod wording;

pub use exit::{exit_of, exit_of_refusal};
