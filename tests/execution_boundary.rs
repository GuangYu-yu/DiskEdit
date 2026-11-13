//! 执行边界。
//!
//! 三条断言分别钉住：入口只收已确认计划、写前比对先于第一笔落盘、记录必须自证
//! 才被当作恢复凭据。它们是结构守卫——行为侧的注入测试在 crate 内单测与
//! `tests/recovery_crash.rs`，本文件不重复造崩溃窗口。

mod guards;

use guards::{assert_order, rust_sources, source};

#[test]
fn the_execution_entry_takes_a_confirmed_plan_only() {
    let module = source("execution/mod.rs");
    assert!(
        module.contains("pub(crate) fn execute_plan("),
        "there must be exactly one named plan executor"
    );
    assert!(
        module.contains("plan: &ConfirmedPlan"),
        "the executor must receive a confirmed plan, not a raw request or a target path"
    );
    assert_order(
        &module,
        &[
            "pub(crate) fn execute_plan(",
            "let steps = prepare_steps(",
            "verify_tables_before_write(src, &steps)?;",
        ],
        "the whole preflight must complete before the step loop",
    );
    let other_entrances = rust_sources()
        .into_iter()
        .filter(|(rel, text)| {
            rel.starts_with("execution/")
                && !rel.ends_with("mod.rs")
                && text.contains("pub(crate) fn execute_plan(")
        })
        .map(|(rel, _)| rel)
        .collect::<Vec<_>>();
    assert!(
        other_entrances.is_empty(),
        "a second plan executor appeared: {other_entrances:?}"
    );
}

#[test]
fn the_write_gate_opens_only_once_the_record_is_durable() {
    let table = source("execution/table.rs");
    assert_order(
        &table,
        &[
            "src.write_converged(",
            "src.sync_all()",
            "verify_regions_after(src, &step.regions, expect_after)",
        ],
        "the table step must compare before writing, write, then compare again after writing",
    );
    assert_order(
        &source("dev/source.rs"),
        &[
            "pub(crate) fn write_guarded(",
            "self.read_at(off, &mut current)",
            "self.write_at(&guard, off, buf)",
            "pub(crate) fn write_converged(",
            "self.write_guarded(off, buf, expect, step)",
            "self.read_back(off, buf)",
        ],
        "write_converged must compare before writing and read back after",
    );
    // 写事实只有一个来源：4 / 5 的分流都读进程级标志
    assert!(
        source("outcome.rs").contains("fn target_may_have_been_written"),
        "the process write fact must stay in the result type"
    );
    assert!(
        source("execution/fs.rs").contains("outcome::attribute("),
        "the fs step must take its 4/5 split from the write fact"
    );
    assert!(
        !source("execution/mod.rs").contains("state.wrote_target()"),
        "the executor must not derive the write boundary from the record's progress"
    );
}

#[test]
fn an_unverified_record_does_not_arm_the_boundary() {
    let record = source("recovery/record.rs");
    // 解码顺序不可交换：先认 magic，再验头 CRC，再用可信的字段核对长度，
    // 最后验载荷 CRC 才允许解析内容。任意一步提前信任载荷都会让伪造记录进入清理路径
    assert_order(
        &record,
        &[
            "pub(crate) fn decode(",
            "magic != MAGIC",
            "crc32(&bytes[HEADER_CRC_RANGE]) != header_crc",
            "total_len > MAX_RECORD_BYTES",
            "crc32(payload) != payload_crc",
            "decode_payload(payload)",
        ],
        "record decode order",
    );
    // 格式自证，不靠版本号
    assert!(
        !record.contains("format_version") && !record.contains("version: u"),
        "a format version field would reintroduce a second source of truth"
    );
    let cleanup = source("recovery/cleanup.rs");
    assert!(
        cleanup.contains("tx_id") && cleanup.contains("classify"),
        "deleting a record still requires the identity predicate to pass"
    );
}
