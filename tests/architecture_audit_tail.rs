//! 审计尾段的读取面只有一个名字。
//!
//! 「放置不影响记录」这条结论的落点不是"记录里没有这个词"，而是"记录里与放置有关的那
//! 一段没有读者"。`Record.audit` 是这一段的家：它装放置、worker 摘要与工具版本，编码进
//! 载荷尾段，续跑与回滚都不读它。为了让"谁在读它"可机械复核，字段本身私有，外部只能经
//! [`Record::audit_tail`] 读、经 [`Record::swap_audit_tail`] 换——于是读取面就是一个名字，
//! 本文按这个名字普查：新增读者必须在这里登记理由，否则失败。
//!
//! 与 `architecture_fact_reads.rs` 的分工：那份管 `Fact` 三态被折成布尔量，这份管"审计
//! 事实被当成判定输入"。两者都靠源码文本，因为编译器只能拦住可见性，拦不住"调用了一个
//! 公开的读取入口"。
//!
//! 尾段不参与判定这件事本身另有行为层判据：`src/recovery/record.rs` 的
//! `the_audit_tail_does_not_move_a_judgement` 拿两条只差尾段的记录逐个比对判定出口。

mod guards;

/// 允许读审计尾段的文件与理由。今天只有一个：把记录呈现给人看
const READERS: &[(&str, &str)] = &[(
    "cmd/inspect.rs",
    "`describe_record` 把尾段打成一行 `audit: placement=…`；只呈现，不参与判定",
)];

/// 调用 `audit_tail()` 的每一行都必须落在登记里
#[test]
fn only_the_inspector_reads_the_audit_tail() {
    let mut found: Vec<(String, usize)> = Vec::new();
    let mut unregistered: Vec<String> = Vec::new();
    for (rel, text) in guards::rust_sources() {
        for (n, line) in guards::production_lines(&text) {
            if !line.contains("audit_tail()") {
                continue;
            }
            if READERS.iter().any(|(file, _)| *file == rel) {
                found.push((rel.clone(), n));
            } else {
                unregistered.push(format!("{rel}:{n}: {}", line.trim()));
            }
        }
    }

    assert!(
        unregistered.is_empty(),
        "these lines read the audit tail but are not registered as readers — the tail is an \
         audit fact, not a decision input; register the reason in READERS or drop the read:\n{}",
        unregistered.join("\n")
    );

    let mut seen: Vec<&str> = found.iter().map(|(file, _)| file.as_str()).collect();
    seen.dedup();
    let mut expected: Vec<&str> = READERS.iter().map(|(file, _)| *file).collect();
    expected.sort_unstable();
    seen.sort_unstable();
    assert_eq!(
        seen, expected,
        "the set of files reading the audit tail changed — every reader needs a READERS row, and \
         every row needs at least one read"
    );
}

/// 字段私有 + 两个入口存在：这是"读取面只有一个名字"的物理前提，缺一条上面那条普查就
/// 退化成"自觉"
#[test]
fn the_tail_is_reachable_only_through_its_two_entry_points() {
    let text = guards::source("recovery/record.rs");
    assert!(
        text.contains("\n    audit: Audit,"),
        "the audit field is no longer a private field of `Record` — its declaration moved, fix \
         this guard before trusting the reader census"
    );
    assert!(
        !text.contains("pub(crate) audit: Audit"),
        "the audit field became visible to every module again: readers can now bypass \
         `audit_tail()` and the census cannot see them"
    );
    assert!(
        text.contains("pub(crate) fn audit_tail(&self) -> &Audit"),
        "the single read entry point `audit_tail` is gone"
    );
    assert!(
        text.contains("pub(crate) fn swap_audit_tail(&mut self, audit: Audit) -> Option<Audit>"),
        "the single write entry point `swap_audit_tail` is gone"
    );

    // 换尾段的调用点只在日志层：落盘失败要能把旧值交还回去，是那里成对的两行
    let mut swappers: Vec<String> = Vec::new();
    for (rel, text) in guards::rust_sources() {
        for (n, line) in guards::production_lines(&text) {
            if line.contains("swap_audit_tail(") && !line.contains("fn swap_audit_tail") {
                swappers.push(format!("{rel}:{n}"));
            }
        }
    }
    assert!(!swappers.is_empty(), "no production site swaps the tail: the entry point is unused");
    let mut wrong: Vec<&String> =
        swappers.iter().filter(|site| !site.starts_with("recovery/journal.rs:")).collect();
    wrong.sort();
    assert!(
        wrong.is_empty(),
        "the tail may be swapped from the record journal only; these call sites bypass it: {wrong:?}"
    );
}
