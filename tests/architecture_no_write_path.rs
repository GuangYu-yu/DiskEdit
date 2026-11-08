//! 「计划步骤只有一个执行器」的结构守卫。
//!
//! 计划之外有三类写入：`cmd/undo` 的回退方向回放、已提交态续跑时的
//! NTFS HiddenSectors 回填、mkfs 前的 `wipe_zero`；另有证据文件自身的原子替换与
//! 外部工具的 stdin。本文件把「出现字节写原语的源码文件集合」钉成一张封闭清单，
//! 任何新增写点都必须先改这张清单。

mod guards;

use guards::{rust_sources, source};

/// 允许出现目标/证据写原语的文件。清单是**文件集合**而不是「生产写点」的判定：
/// `table/*`、`gpt_policy.rs` 里的直写与整个 `fsops/tests.rs` 都只在 `#[cfg(test)]`
/// 下可见，是测试夹具的独立参照，不在生产调用链上；单列它们是为了让集合保持
/// 可机械复核——新增一个含写原语的文件无论生产还是夹具，都必须先过这张清单。
const WRITE_SITES: &[&str] = &[
    "cmd/undo.rs",
    "dev/source.rs",
    "execution/table.rs",
    "fsops/env.rs",
    "fsops/swap.rs",
    "fsops/tests.rs",
    "gpt_policy.rs",
    "movepart/resize.rs",
    "platform/evidence_store.rs",
    "recovery/journal.rs",
    "table/create.rs",
    "table/gpt_write.rs",
    "table/mbr.rs",
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
