//! 「计划步骤只有一个执行器」的结构守卫。
//!
//! 计划之外有三类写入：`undo` 的回退方向回放（原语在 `recovery/rollback.rs`）、
//! 已提交态续跑时的 NTFS HiddenSectors 回填、mkfs 前的 `wipe_zero`；另有证据文件
//! 自身的原子替换与外部工具的 stdin。本文件把「出现字节写原语的源码文件集合」
//! 钉成一张封闭清单。

mod guards;

use guards::{rust_sources, source};

/// 允许出现目标/证据写原语的文件。清单与 `rust_sources` 的扫描范围一致：
/// `tests/` 目录被跳过，其中的夹具直写只存在于 `#[cfg(test)]`，不在生产调用链上；
/// `fsops/tests.rs` 是文件而非目录，仍在扫描范围内。
const WRITE_SITES: &[&str] = &[
    "dev/source.rs",
    "execution/table.rs",
    "fsops/env.rs",
    "fsops/swap.rs",
    "fsops/tests.rs",
    "gpt_policy.rs",
    "movepart/resize.rs",
    "platform/evidence_store.rs",
    "recovery/journal.rs",
    "recovery/rollback.rs",
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
