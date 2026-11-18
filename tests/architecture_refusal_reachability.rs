//! 拒绝分支的生产可达性守卫。
//!
//! 三个「挡住错误写入」的枚举（`TableReplay`、`NoPermitReason`、`CommitRefusal`、
//! `CleanupError`）里，每个变体必须满足其一：
//!
//! - 生产代码里有构造点，且喂给它的输入在生产路径上确实会出现；
//! - 登记为「仅测试可达」欠账，附测试锚点，并钉住「生产调用点不得构造它」。
//!
//! 枚举新增变体而台账没跟上、构造点被删、或者欠账变体悄悄被生产路径构造，都会失败。
//! `.trae/` 不在 crate 内，本文件不依赖台账存在。

mod guards;

use guards::{production_lines, source};

/// 变体的可达性登记
struct Variant {
    id: &'static str,
    name: &'static str,
    /// 生产构造点，或测试锚点（`Only` 时）
    anchor: &'static str,
    only_test: bool,
}

const TABLE_REPLAY: &[Variant] = &[
    Variant { id: "T-1", name: "Full", anchor: "return Self::Full;", only_test: false },
    Variant { id: "T-2", name: "CopyTableOnly", anchor: "OpKind::Copy => Self::CopyTableOnly,", only_test: false },
    Variant { id: "T-3", name: "MoveClear", anchor: "(Some(false), _) => Self::MoveClear,", only_test: false },
    Variant { id: "T-4", name: "MoveBlocked", anchor: "(Some(true), false) => Self::MoveBlocked,", only_test: false },
    Variant { id: "T-5", name: "MoveNoPreimage", anchor: "(None, _) => Self::MoveNoPreimage,", only_test: false },
    Variant { id: "T-6", name: "MoveForced", anchor: "(Some(true), true) => Self::MoveForced,", only_test: false },
    Variant { id: "T-7", name: "Refused", anchor: "_ => Self::Refused,", only_test: false },
];

const NO_PERMIT_REASON: &[Variant] = &[
    // 留存状态由盘上记录解码而来；写路径只产出 Active，但记录是判定依据本身
    Variant { id: "N-1", name: "Retention", anchor: "Some(NoPermitReason::Retention(record.retention))", only_test: false },
    // 表步在飞或尾段未解出：每个写入阶段先落盘，崩溃后记录里留下未落定事实
    Variant { id: "N-2", name: "PendingTableChange", anchor: "Some(NoPermitReason::PendingTableChange)", only_test: false },
];

const COMMIT_REFUSAL: &[Variant] = &[
    // 两个生产提交点都只传已识别记录，见下方 commit_callers_pass_recognized
    Variant { id: "C-1", name: "NotRecognized", anchor: "assert!(RecoveryEngine::commit(evidence.clone()).is_err()", only_test: true },
];

const CLEANUP_ERROR: &[Variant] = &[
    Variant { id: "E-1", name: "Io", anchor: "RemoveOutcome::NotRemoved(e) => Err(CleanupError::Io(e)),", only_test: false },
    Variant { id: "E-2", name: "ReadbackMismatch", anchor: "return Err(CleanupError::ReadbackMismatch(", only_test: false },
];

/// 解析 `header` 之后的枚举体，返回变体名。只处理「每个变体独占一行、行首是变体名」
/// 的写法；换行风格或带字段的变体同样按首个标识符取。
fn enum_variants(text: &str, header: &str) -> Vec<String> {
    let start = text
        .find(header)
        .unwrap_or_else(|| panic!("enum header not found: {header}"));
    let body = &text[start + header.len()..];
    let mut depth = 1i32;
    let mut out = Vec::new();
    for raw in body.lines() {
        let line = raw.trim();
        if depth == 1 && !line.is_empty() && !line.starts_with("//") && !line.starts_with('#') {
            if line.starts_with('}') {
                break;
            }
            let name: String = line
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_')
                .collect();
            if !name.is_empty() && !out.contains(&name) {
                out.push(name);
            }
        }
        depth += raw.matches('{').count() as i32 - raw.matches('}').count() as i32;
        if depth <= 0 {
            break;
        }
    }
    out.sort();
    out
}

fn check(file: &str, header: &str, registered: &[Variant]) {
    let text = source(file);
    let declared = enum_variants(&text, header);
    let mut names: Vec<&str> = registered.iter().map(|v| v.name).collect();
    names.sort();
    assert_eq!(
        declared, names,
        "{file} 的 {header} 变体与可达性台账不一致：新增变体必须说明生产调用点或登记仅测试可达"
    );
    for variant in registered {
        let anchor_scope = if variant.only_test { "recovery/tests.rs" } else { file };
        assert!(
            source(anchor_scope).contains(variant.anchor),
            "{} 的锚点消失（{anchor_scope}）：{}",
            variant.id,
            variant.anchor
        );
    }
}

#[test]
fn table_replay_variants_are_reachable() {
    check("recovery/rollback.rs", "pub(crate) enum TableReplay {", TABLE_REPLAY);
}

#[test]
fn no_permit_reason_variants_are_reachable() {
    check("recovery/cleanup.rs", "pub(crate) enum NoPermitReason {", NO_PERMIT_REASON);
}

#[test]
fn commit_refusal_variants_are_reachable() {
    check("recovery/cleanup.rs", "pub(crate) enum CommitRefusal {", COMMIT_REFUSAL);
}

#[test]
fn cleanup_error_variants_are_reachable() {
    check("recovery/cleanup.rs", "pub(crate) enum CleanupError {", CLEANUP_ERROR);
}

/// 生产代码里出现 `RecoveryEngine::commit(` 的文件。新增提交点时先复核它传入的证据，
/// 再把文件登记到这里
const COMMIT_SITE_FILES: &[&str] = &["recovery/journal.rs"];

/// 欠账变体的前提：生产提交点只传已识别记录。任何一处开始传别的证据，
/// `CommitRefusal::NotRecognized` 就从欠账变成可达，必须更新台账。
#[test]
fn commit_callers_pass_recognized() {
    let mut files: Vec<String> = Vec::new();
    for (path, text) in guards::rust_sources() {
        if path.ends_with("tests.rs") || path.contains("/tests/") {
            continue;
        }
        for (line_no, line) in production_lines(&text) {
            if !line.contains("RecoveryEngine::commit(") {
                continue;
            }
            if !files.contains(&path) {
                files.push(path.clone());
            }
            assert!(
                line.contains("Evidence::Recognized"),
                "{path}:{line_no} 提交点只接受已识别记录：{line}"
            );
        }
    }
    files.sort();
    let mut registered: Vec<String> = COMMIT_SITE_FILES.iter().map(|f| f.to_string()).collect();
    registered.sort();
    assert_eq!(
        files, registered,
        "生产提交点的文件集合与登记不一致：新增提交点时必须复核它传入的证据，并登记到 COMMIT_SITE_FILES"
    );
}
