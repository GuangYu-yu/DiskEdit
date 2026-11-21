//! 载体步骤的结构守卫：计划步骤的写入只能经 `exec::run_plan` / `exec::run_step`。
//!
//! 命令层负责参数、策略与呈现，出现写入原语或直接调用 `execution` 的单步实现都说明
//! 又开了一条绕过载体的路径。执行器的单步实现只允许 `exec/native.rs` 调用，探测与
//! 复核共用同一套步骤分类。

mod guards;

use guards::{production_lines, rust_sources, source};

/// 命令层可用的载体入口
const CARRIER_ENTRIES: &[&str] = &[
    "crate::exec::run_plan(",
    "crate::exec::run_plan_complete(",
    "crate::exec::backfill_hidden_sectors(",
];

/// 命令层不得出现的写入形态与单步实现
const FORBIDDEN: &[&str] = &[
    "crate::execution::table::",
    "crate::execution::relocate::",
    "crate::execution::copy::",
    "crate::execution::fs::",
    "crate::execution::mkfs::",
    "crate::execution::set::",
    "crate::execution::swap::",
    "crate::execution::fix_ntfs_hidden_sectors",
    "crate::execution::execute_plan",
    ".write_at(",
    ".write_guarded(",
    ".write_converged(",
    "fsops::wipe_zero",
];

#[test]
fn carrier_writes_go_through_run_step() {
    let mut offenders = Vec::new();
    for (rel, text) in rust_sources() {
        if !rel.starts_with("cmd/") {
            continue;
        }
        for (line, text) in production_lines(&text) {
            let hit = FORBIDDEN.iter().find(|form| text.contains(*form));
            if let Some(form) = hit {
                offenders.push(format!("{rel}:{line}: {form}"));
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "the command layer bypasses the carrier steps:\n{}",
        offenders.join("\n")
    );

    // 计划入口必须在命令层被真实调用：清单为空时，上面的排除条件覆盖不到任何东西
    let mut entries = 0usize;
    for (rel, text) in rust_sources() {
        if !rel.starts_with("cmd/") {
            continue;
        }
        entries += CARRIER_ENTRIES
            .iter()
            .map(|entry| text.matches(entry).count())
            .sum::<usize>();
    }
    assert!(entries > 0, "no command layer call site reaches the carrier entry");
}

#[test]
fn the_step_implementations_have_one_caller() {
    // 单步实现由 Native 映射统一调用；出现第二个调用文件就说明有旁路
    let native = source("exec/native.rs");
    for call in [
        "table::execute",
        "relocate::execute",
        "copy::execute",
        "fs::execute",
        "mkfs::execute",
        "set::execute",
        "swap::rebuild",
    ] {
        assert!(native.contains(call), "the native mapping does not cover {call}");
    }
    for (rel, text) in rust_sources() {
        if rel == "exec/native.rs" || rel.starts_with("execution/") || rel == "support.rs" {
            continue;
        }
        for (line, text) in production_lines(&text) {
            if text.contains("relocate::execute")
                || text.contains("copy::execute(")
                || text.contains("mkfs::execute(")
                || text.contains("swap::rebuild(")
            {
                panic!("{rel}:{line} calls a step implementation outside the native mapping");
            }
        }
    }
}

#[test]
fn the_driver_lives_in_the_carrier_module() {
    // 驱动循环只有一份：`exec` 里的 run_plan，`execution` 只剩单步实现
    let executor = source("execution/mod.rs");
    assert!(
        !executor.contains("fn execute_plan"),
        "the execution module still owns a plan driver"
    );
    let driver = source("exec/mod.rs");
    assert!(driver.contains("pub(crate) fn run_plan("), "the carrier driver is missing");
    // 单步入口只是"执行、不判定"，只对分派层可见
    assert!(
        driver.contains("pub(super) fn run_step("),
        "the carrier step entry must stay inside the carrier module"
    );
    assert!(
        !driver.contains("pub(crate) fn run_step("),
        "the raw step entry must not be reachable from the command layer"
    );
}

#[test]
fn the_raw_dispatch_has_one_caller_and_one_judgement() {
    // 原始分派只对分派层可见：计划循环拿不到它，因此不存在"执行了却不判定"的第二条路
    assert!(
        !source("exec/mod.rs").contains(".run("),
        "the plan loop must not reach the raw dispatch"
    );
    let dispatch = source("exec/dispatch.rs");
    assert!(
        dispatch.contains("    fn run("),
        "the dispatch module must own the raw dispatch"
    );
    assert!(
        !dispatch.contains("pub(super) fn run("),
        "the raw dispatch must not be reachable from the plan loop"
    );
    // 判定与记录动作合成一次：结果、凭据与推进都在 `step` 里
    assert!(
        dispatch.contains("advance.record(applied"),
        "the step entry must be where the applied credential is spent"
    );
    // 游标推进以凭据入参：没有 `StepApplied` 就没有推进入口
    let executor = source("execution/mod.rs");
    assert!(
        executor.contains("fn advance_entry(&mut self, _: crate::exec::StepApplied)"),
        "advance_entry must require the applied credential"
    );
    assert!(
        executor.contains("fn note_step(&mut self, _: crate::exec::StepApplied)"),
        "note_step must require the applied credential"
    );
    // 凭据的唯一构造点
    let plan = source("exec/mod.rs");
    assert!(
        plan.contains("pub(super) fn evidence()"),
        "the applied credential must be built only where the verdict is decided"
    );
}
