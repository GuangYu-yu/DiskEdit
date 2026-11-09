//! 「计划步骤只有一个执行器」的结构守卫。
//!
//! 计划之外有三类写入：`undo` 的回退方向回放（原语在 `recovery/rollback.rs`）、
//! 已提交态续跑时的 NTFS HiddenSectors 回填、mkfs 前的 `wipe_zero`；另有证据文件
//! 自身的原子替换与外部工具的 stdin。本文件把「出现字节写原语的源码文件集合」
//! 钉成一张封闭清单，任何新增写点都必须先改这张清单。

mod guards;

use guards::{rust_sources, source};

/// 允许出现目标/证据写原语的文件。清单是**文件集合**而不是「生产写点」的判定：
/// `movepart/tests`、`table/tests`、`fsops/tests.rs` 里的写原语都只在 `#[cfg(test)]`
/// 下可见，是测试夹具的独立参照，不在生产调用链上；单列它们是为了让集合保持
/// 可机械复核——新增一个含写原语的文件无论生产还是夹具，都必须先过这张清单。
const WRITE_SITES: &[&str] = &[
    "dev/source.rs",
    "execution/table.rs",
    "fsops/env.rs",
    "fsops/swap.rs",
    "fsops/tests.rs",
    "gpt_policy.rs",
    "movepart/resize.rs",
    "movepart/tests/plan.rs",
    "movepart/tests/resize.rs",
    "movepart/tests/shift.rs",
    "movepart/tests/support.rs",
    "platform/evidence_store.rs",
    "recovery/journal.rs",
    "recovery/rollback.rs",
    "table/create.rs",
    "table/gpt_write.rs",
    "table/mbr.rs",
    "table/tests/create.rs",
    "table/tests/gpt.rs",
    "table/tests/mbr.rs",
    "table/tests/support.rs",
];

#[test]
fn no_production_module_opens_a_new_write_path() {
    let mut found = Vec::new();
    for (rel, text) in rust_sources() {
        let writes = text.contains(".write_at(")
            || text.contains(".write_all(")
            || text.contains("pwrite(")
            || text.contains("renameat(")
            || text.contains("unlinkat(");
        if writes {
            found.push(rel);
        }
    }
    found.sort();
    found.dedup();
    let mut expected = WRITE_SITES.to_vec();
    expected.sort();
    assert_eq!(
        found, expected,
        "the set of files that can write bytes changed — either a second write path appeared, or this guard and the documented write points must be updated together"
    );

    let executor = source("execution/mod.rs");
    assert!(
        !executor.contains("全仓唯一的写盘路径"),
        "the executor must not claim to be the only writer in the whole repository"
    );
    assert!(
        executor.contains("计划步骤"),
        "the executor's scope is plan steps; the off-plan writes are registered separately"
    );
}

#[test]
fn off_plan_writes_are_registered_in_the_executor_header() {
    // 设计文档不在版本控制里，守卫只能读随提交走的源码
    let header = source("execution/mod.rs");
    for marker in [
        "fix_ntfs_hidden_sectors",
        "cmd/undo",
        "wipe_zero",
        "计划步骤",
    ] {
        assert!(
            header.contains(marker),
            "an off-plan write is no longer registered beside the executor: {marker}"
        );
    }
}
