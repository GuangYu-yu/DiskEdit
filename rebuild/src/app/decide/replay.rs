//! `replay` 的判定：补完已搬移作业的表区写入。
//!
//! 与 `undo` 同一条回滚通路；差别只在两处：数据区步骤不再触发拒绝（B7 的
//! 表区回放语义），缺记录的文案说"nothing to replay"。

use crate::app::observe::{Command, RecoverMode, RecoverSnapshot, Request};

use super::undo;
use super::{Decision, Refusal};

/// 判定 `replay` 的一次调用。
pub fn decide(snapshot: &RecoverSnapshot, request: &Request) -> Decision {
    let Command::Replay = request.command() else {
        return Decision::Refuse(Refusal::RecoveryRefused {
            why: "replay reached with a different command".to_owned(),
        });
    };
    undo::rollback_entry(snapshot, RecoverMode::ReplayTableOnly, "replay")
}
