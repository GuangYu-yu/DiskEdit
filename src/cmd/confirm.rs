//! 确认门：按几何推导出的影响面决定是否需要用户授权，而不是按命令名。
//!
//! 与能力门（`cmd::capability_gap`）正交：能力门问"本平台有没有这条命令需要的原语"（6），
//! 确认门问"用户是否授权这次破坏"（3）。分派时能力门在外，这里在内——命令函数
//! 在几何与影响范围算完之后、任何持久化副作用（目标字节、journal 记录、checkpoint、
//! 日志；锁文件除外）之前调用 [`gate`]。

use crate::support::{bail_fail, RunOutcome};

/// 一次操作对盘上数据的影响面，由命令层在几何算完后填入。
/// 任一字段为真都意味着越过一个 `undo` 回不去的点。
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Impact {
    /// 位置会改变的分区数（delta 为 0 的条目不挪位置，故不计入）。
    /// swap 只重建内容，旧位置上的字节同样不可回滚
    pub(crate) relocating: usize,
    /// 目标分区被缩容，其文件系统同缩
    pub(crate) shrinking: bool,
    /// 把数据复制到一段新落点，源分区不动
    pub(crate) copying: bool,
    /// 在新落点上创建文件系统
    pub(crate) formatting: bool,
}

/// 确认门的唯一判据：需要 `--yes` 时返回点名破坏对象的一句话。
pub(crate) fn required(impact: &Impact) -> Option<String> {
    let mut parts = Vec::new();
    if impact.relocating > 0 {
        let (noun, verb) = if impact.relocating == 1 {
            ("partition", "would have its data relocated in place")
        } else {
            ("partitions", "would have their data relocated in place")
        };
        parts.push(format!("{} {noun} {verb}", impact.relocating));
    }
    if impact.shrinking {
        parts.push("the partition would be shrunk together with its filesystem".to_string());
    }
    if impact.copying {
        parts.push("the destination range would be overwritten with the copied bytes".to_string());
    }
    if impact.formatting {
        parts.push("a filesystem would be written over the new partition's range".to_string());
    }
    if parts.is_empty() {
        return None;
    }
    Some(format!(
        "{} — this cannot be rolled back with `undo`; re-run with --yes to confirm",
        parts.join("; ")
    ))
}

/// 在首个不可回滚的副作用之前调用：影响面非空且缺 `--yes` 时拒绝（10）。
pub(crate) fn gate(yes: bool, impact: Impact) {
    let Some(what) = required(&impact) else { return };
    if !yes {
        bail_fail(RunOutcome::refused(what));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_impact_needs_no_confirmation() {
        assert_eq!(required(&Impact::default()), None);
    }

    #[test]
    fn every_irreversible_impact_names_the_loss_and_the_undo_limit() {
        for impact in [
            Impact { relocating: 1, ..Default::default() },
            Impact { shrinking: true, ..Default::default() },
            Impact { copying: true, ..Default::default() },
            Impact { formatting: true, ..Default::default() },
        ] {
            let text = required(&impact).expect("a non-empty impact must ask for confirmation");
            assert!(text.contains("cannot be rolled back with `undo`"), "{text}");
            assert!(text.contains("--yes"), "{text}");
        }
    }

    #[test]
    fn relocation_count_is_pluralized() {
        let one = required(&Impact { relocating: 1, ..Default::default() }).unwrap();
        assert!(one.contains("1 partition would have its data relocated"), "{one}");
        let two = required(&Impact { relocating: 2, ..Default::default() }).unwrap();
        assert!(two.contains("2 partitions would have their data relocated"), "{two}");
    }
}
