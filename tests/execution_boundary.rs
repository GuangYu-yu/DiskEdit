//! 执行边界。
//!
//! 三条断言分别钉住：入口只收已确认计划、写前比对先于第一笔落盘、记录必须自证
//! 才被当作恢复凭据。它们是结构守卫——行为侧的注入测试在 crate 内单测与
//! `tests/recovery_crash.rs`，本文件不重复造崩溃窗口。

mod guards;

use guards::{assert_order, pos, rust_sources, source};

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
            "verify_first_table(src, &steps)?;",
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
    let before = pos(&table, "verify_regions_before(", "table step");
    let write = pos(&table, "src.write_at(", "table step");
    let after = pos(&table, "verify_regions_after(", "table step");
    assert!(
        before < write && write < after,
        "the table step must compare before writing and compare again after writing"
    );
    // 写前比对自带「写过目标字节就只剩 5」的分流，不能在调用点另立一套码值
    let stamp = source("table/stamp.rs");
    assert!(
        stamp.contains("wrote_target"),
        "verify_regions_before must know whether earlier steps already wrote"
    );
    let module = source("execution/mod.rs");
    assert!(
        module.contains("past_boundary") || module.contains("state.wrote_target()"),
        "the executor must carry the write boundary through later steps"
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
