//! 「会否决写盘或决定回退走向」的判据只在一处计算的守卫。
//!
//! 三组判据各自只允许一个计算点：`undo` 的表区回放（相交与否）、一次运行改变
//! 了哪类持久状态、以及没有改变持久状态时的那句结论。调用点只许读枚举，不许
//! 自己再算一遍区间或再写一句结论；文案随枚举的 `Display` 走。
//!
//! 扫描范围与 `architecture_no_write_path` 相同（`src/` 下的生产源码，跳过
//! `tests/` 子目录）。断言按「文件集合」而不是行号钉住：行号会随无关改动漂移，
//! 文件集合只有在真的出现第二个计算点时才变化。

mod guards;

use guards::{rust_sources, source};

/// `TableReplay::of(` 的调用者：定义与执行器在 `rollback.rs`，消费点在下列文件。
/// 多一个文件就说明有调用点在自己决定回放走向
const TABLE_REPLAY_CALLERS: &[&str] = &[
    "cmd/abandon.rs",
    "cmd/inspect.rs",
    "cmd/undo.rs",
    "recovery/engine.rs",
    "recovery/rollback.rs",
];

#[test]
fn the_move_replay_overlap_is_computed_only_in_rollback() {
    const JUDGEMENT: &[&str] = &[
        "move_writes_overlap_old_ranges",
        "written_destination_ranges",
        "restore_preimage_ranges",
        "ranges_intersect",
    ];
    let mut found = Vec::new();
    for (rel, text) in rust_sources() {
        if rel == "recovery/rollback.rs" {
            continue;
        }
        for name in JUDGEMENT {
            if text.contains(name) {
                found.push(format!("{rel}: {name}"));
            }
        }
    }
    assert!(
        found.is_empty(),
        "the move-replay overlap is computed outside recovery/rollback.rs:\n{}",
        found.join("\n")
    );
    // 锚：判据本体仍在 rollback.rs，且仍然驱动 TableReplay。上面那条断言在判据被
    // 整体搬走时会空转，这条让它失败而不是静默放行
    let text = source("recovery/rollback.rs");
    assert!(
        text.contains("move_writes_overlap_old_ranges(record)"),
        "TableReplay no longer reads the overlap judgement"
    );
}

#[test]
fn table_replay_is_built_from_one_judgement() {
    let mut callers: Vec<String> = rust_sources()
        .into_iter()
        .filter(|(_, text)| text.contains("TableReplay::of("))
        .map(|(rel, _)| rel)
        .collect();
    callers.sort();
    callers.dedup();
    let mut expected: Vec<String> = TABLE_REPLAY_CALLERS.iter().map(|s| s.to_string()).collect();
    expected.sort();
    assert_eq!(
        callers, expected,
        "the set of files that build a TableReplay changed — a caller must not decide the replay \
         direction on its own"
    );

    // 文案同源：三种结论的关键句子只出现在枚举的 Display 里，调用点不得自写一句
    let needles = [
        "cannot be shown to be clear of the ranges",
        "the recorded ranges were not rolled back",
        "the data they hold is no longer",
        "no destination range the move has written",
    ];
    let rollback = source("recovery/rollback.rs");
    for needle in needles {
        assert!(
            rollback.contains(needle),
            "the judgement's wording left recovery/rollback.rs: {needle}"
        );
        for (rel, text) in rust_sources() {
            if rel == "recovery/rollback.rs" {
                continue;
            }
            assert!(
                !text.contains(needle),
                "{rel} repeats the judgement's wording instead of printing the verdict: {needle}"
            );
        }
    }
}

#[test]
fn persisted_change_is_decided_at_one_point() {
    // `PersistedChange::of(` 的两类事实（目标字节、记录文件）只在一处合起来判定
    let mut callers: Vec<String> = rust_sources()
        .into_iter()
        .filter(|(_, text)| text.contains("PersistedChange::of("))
        .map(|(rel, _)| rel)
        .collect();
    callers.sort();
    callers.dedup();
    assert_eq!(
        callers,
        vec!["cmd/abandon.rs".to_string(), "outcome.rs".to_string()],
        "PersistedChange is built outside its type or the abandon failure path"
    );

    // 已提交记录的释放结论只有一个出口
    let mut releases: Vec<String> = rust_sources()
        .into_iter()
        .filter(|(_, text)| text.contains("partial_records_changed("))
        .map(|(rel, _)| rel)
        .collect();
    releases.sort();
    releases.dedup();
    assert_eq!(
        releases,
        vec!["cmd/undo.rs".to_string(), "outcome.rs".to_string()],
        "the committed-record release conclusion is built outside outcome.rs and undo"
    );

    // 「没有改变持久状态」这句话只在结果呈现层写一次
    let mut claims = Vec::new();
    for (rel, text) in rust_sources() {
        if rel == "outcome.rs" || rel == "docs.rs" {
            continue;
        }
        for line in text.lines() {
            if line.contains("no persistent state") {
                claims.push(format!("{rel}: {}", line.trim()));
            }
        }
    }
    assert!(
        claims.is_empty(),
        "a no-persistent-state claim was written outside the presenter:\n{}",
        claims.join("\n")
    );
    assert!(
        source("outcome.rs").contains("this run changed no persistent state"),
        "the presenter sentence is gone — the single-source wording moved"
    );
}
