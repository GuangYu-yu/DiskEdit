//! 规划层纯洁性。
//!
//! L3 只把值类型映射成计划，不打开目标、不取锁、不调外部工具、不读恢复记录。
//! 这两个用例从源码层钉住这条边界——一旦 planner 里出现 I/O 入口或句柄参数，
//! 编译能过、行为测试也未必覆盖，但架构已经破了。

mod guards;

use guards::{rust_sources, source};

#[test]
fn planner_layer_has_no_io_entries() {
    let forbidden = [
        "OpenOptions",
        "File::open",
        "std::fs::",
        "std::process",
        "Command::new",
        "write_at(",
        "read_at(",
        "WriteSource",
        "FileSource",
        "RecordJournal",
        "EvidenceStore",
    ];
    let planner: Vec<(String, String)> = rust_sources()
        .into_iter()
        .filter(|(rel, _)| rel.starts_with("planner/"))
        .collect();
    assert!(
        planner.len() >= 2,
        "the planner layer must still be a separate module tree: {planner:?}"
    );
    for (rel, text) in &planner {
        for token in forbidden {
            assert!(
                !text.contains(token),
                "{rel} leaks an I/O entry point into the planner layer: {token}"
            );
        }
    }
}

#[test]
fn planner_entry_points_take_values_only() {
    let module = source("planner/mod.rs");
    assert!(
        module.contains("pub(crate) fn needs_for(capabilities: &[Capability]) -> Vec<CapabilityId>"),
        "the capability mapping must take a value slice and return value ids"
    );
    assert!(
        module.contains("pub(crate) struct ConfirmedPlan"),
        "the planner must own the type the executor is the only consumer of"
    );
    assert!(
        !module.contains("&mut WriteSource") && !module.contains("&mut FileSource"),
        "a planner entry point must never receive a writable target handle"
    );

    let chunk = source("planner/chunk.rs");
    assert!(
        chunk.contains("pub(crate) fn relocation_overlaps(shift_lba: u64, src_len_lba: u64) -> bool"),
        "self-overlap must be derived from the shift and the length, not from disk state"
    );
    assert!(
        chunk.contains("pub(crate) fn move_chunk_layout(") && chunk.contains("copy_lba: u64"),
        "chunk layout must remain a pure function of numeric geometry"
    );
}
