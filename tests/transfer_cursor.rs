//! 传输游标的跨条目前进规则。
//!
//! 结构守卫钉住三件事：记录里存的是有序跨分区条目 + 双游标；执行器续跑时按全局
//! 累计块数复原 `transfer_cursor`，跨条目与跨步骤都不归零；自重叠由「位移 + 长度」
//! 判定并从同一份几何推出块布局。崩溃点在每一步之后是否真能续跑，由 crate 内
//! 的 fault 注入用例验证，本文件不重复构造崩溃窗口。

mod guards;

use guards::source;

#[test]
fn the_cursor_is_global_across_entries_and_steps() {
    let record = source("recovery/record.rs");
    for marker in [
        "entries: Vec<RelocEntry>",
        "entry_index: u32",
        "chunks_in_entry: u64",
        "chunk_bytes: u64",
    ] {
        assert!(
            record.contains(marker),
            "the cross-partition record lost {marker}"
        );
    }
    let executor = source("execution/mod.rs");
    for marker in [
        "state.entry_index = reloc.entry_index",
        "state.chunks_in_entry = reloc.chunks_in_entry",
        "let mut recomputed = 0u64",
        "recomputed.checked_add(count)",
        "state.transfer_cursor = state.transfer_cursor.max(recomputed)",
        "state.entry_index += 1",
        "state.chunks_in_entry = 0",
    ] {
        assert!(
            executor.contains(marker),
            "the executor no longer reconstructs a global cursor: {marker}"
        );
    }
}

#[test]
fn self_overlap_is_detected_from_the_shift() {
    let chunk = source("planner/chunk.rs");
    assert!(
        chunk.contains("pub(crate) fn relocation_overlaps(shift_lba: u64, src_len_lba: u64) -> bool"),
        "overlap must be a pure function of shift and length"
    );
    assert!(
        chunk.contains("if relocation_overlaps(shift_lba, src_len_lba)"),
        "the chunk solver must shrink the chunk when the move overlaps itself"
    );
    let relocate = source("execution/relocate.rs");
    assert!(
        relocate.contains("move_chunk_layout(sectors, sector_size, shift, sectors, chunk_bytes)"),
        "execution must reuse the planner's chunk layout, not derive a second one"
    );
    assert!(
        relocate.contains("relocation_overlaps(shift, sectors)"),
        "execution must apply the same self-overlap predicate for per-chunk sync"
    );
    assert!(
        relocate.contains("CHECKPOINT_FLOOR"),
        "the checkpoint cadence is part of the redo window, not a free-standing constant"
    );
}

#[test]
fn injected_crash_points_cover_both_sides_of_the_boundary() {
    let faults = source("movepart/faults.rs");
    for tag in [
        "fault_before_any_write() = \"before-any-write\"",
        "fault_after_data_sync(n: u64) = \"after-data-sync\"",
        "fault_before_entry_commit() = \"before-entry-commit\"",
        "fault_after_entry_commit(mi: usize) = \"after-entry-commit\"",
        "fault_rs_before_commit() = \"rs-before-commit\"",
        "fault_copy_before_commit() = \"copy-before-commit\"",
        "fault_record_persisted() = \"record-persisted\"",
    ] {
        assert!(
            faults.contains(tag),
            "a crash point on one side of the durable boundary is missing: {tag}"
        );
    }
}
