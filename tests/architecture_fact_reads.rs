//! 「缺失不得折叠为放行」的结构守卫。
//!
//! `Fact` 的三态（`Known` / `Absent` / `Undecidable`）由类型本身堵住 `==`：`Fact`
//! 与内嵌它的 `Record` / `Evidence` 只在 `cfg(test)` 下实现
//! `PartialEq` / `Eq`，生产代码里写相等比较编译不过。这条约束由本文件的
//! `fact_types_derive_equality_only_under_test` 钉住，不靠文本规则。
//!
//! 编译器拦不住的三类折法由本文件枚举成封闭清单：
//!
//! 1. `unwrap_or(false|true)`：缺失被折成布尔量；
//! 2. `matches!` 的窗口里出现 `Fact::`：三态被折成一个 `bool`；
//! 3. `Fact::Absent | Fact::Undecidable(..)` 合流成同一个臂：读不出与「确实没有」
//!    走同一条路，正是 `Fact` 存在的理由所在。
//!
//! 每条豁免按「文件 + 站点 id」登记，id 由源码里紧邻折叠点上一行的
//! `// audit-allow: <id>` 标记给出。折叠点换行、重排或改写措辞都不会让登记失配；
//! 新增折叠点没有标记即失败，标记在而折叠点已不在也失败。台账缺失时跳过台账核对
//! （`.trae/` 是工作区资料，不随 crate 分发）；台账在时，每条豁免的 id 必须能查到。
//!
//! `if let Fact::Known(..)` 也在这份清单里：非 `Known` 一律落进 `else`，是否构成
//! 放行由人工逐条判。当前生产代码里没有这种写法，清单为空；新增即失败。
//!
//! `match` 的 `_` 兜底臂**不在**本文件的判据里：它需要知道 scrutinee 的类型，行
//! 文本还原不出来，窗口式扫描会把大量非 `Fact` 的 `match` 一并命中。它由
//! `.trae/documents/读路径台账.md` §5.1 的逐事实臂清单负责——新增对 `Fact` 的
//! `match` 必须在同一处写清三臂归属，任何 `_` 兜底都要在那里登记理由。
//!
//! **项目约定，不属于方法论要求**：`Fact` 三态与「缺失不得折成放行」的接口语义
//! 是方法论内容；本文件只是本仓库把该语义落成可复核断言的方式，换仓库不必照搬。

mod guards;

use guards::{production_lines, rust_sources, source};

/// 白名单：(文件, 站点 id, 为什么它不是「缺失折成放行」)。id 同时是台账行号，
/// 也是折叠点上一行 `// audit-allow:` 标记里的那个词
const ALLOWED: &[(&str, &str, &str)] = &[
    (
        "online/part.rs",
        "W-4",
        "partx 状态读不出时走 BLKPG 回退，回退失败即报错（站点 S1-10，待复核）",
    ),
    (
        "recovery/record.rs",
        "W-7",
        "受控访问器 `table_step_in_flight`：只有 Known 且等于该条目才答「在飞」，Absent/Undecidable \
         都答否。调用点据此返回 Stage::Undecidable，不签发写许可",
    ),
    (
        "recovery/journal.rs",
        "W-8",
        "`source()` 把「没有源区间」与「源区间读不出」都答成 None；唯一调用点在首写前拒绝 \
         None，不构成放行",
    ),
    (
        "recovery/journal.rs",
        "W-9",
        "`mark_table_step_started`：围栏读不出时不猜「表步未开始」，按拒绝返回",
    ),
    (
        "recovery/journal.rs",
        "W-10",
        "`repair()` 的 `None` 有两条来源：`Absent` 是确实没有修复动作，`apply_repair` 据此\
         继续是正常路径；`Undecidable` 只由前缀解码产生。本例外以「前缀记录不进入写路径」\
         为前提：写路径的日志只来自新建记录或完整解码的 `Recognized`（`open_image` 走\
         `Evidence::classify`），两者尾段都可解，所以该臂不可达。前提一旦改变，\
         `Undecidable` 会与 `Absent` 合流成「无需修复」，`apply_repair` 随即写修复，\
         必须拆臂",
    ),
    (
        "recovery/record.rs",
        "W-11",
        "`derive_stage` 把围栏读不出与「没有围栏」都折成 Undecidable 阶段——两者都不签发\
         写许可",
    ),
    (
        "recovery/record.rs",
        "W-12",
        "载荷编码：尾段读不出的记录不得重写，编码直接失败，不把 Undecidable 写成 Absent",
    ),
    (
        "recovery/rollback.rs",
        "W-13",
        "`copy` 回滚前的源复核：拿不到源区间就拒绝写回，两条 `None` 都走同一条拒绝分支，\
         不构成放行",
    ),
    (
        "recovery/record.rs",
        "W-14",
        "`settle_table_step_after_rollback`：只在回滚把记录登记的表区原文（含修复写入）\
         写回并核对之后调用，围栏明确记着 `InFlight` 才退回 `Pending`，修复事实无条件清空。\
         `Absent` / `Undecidable` / `Pending` / `Landed` 的围栏都不动，清理许可仍按记录原值判——\
         读不出的围栏不会被当成「已落定」",
    ),
    (
        "recovery/record.rs",
        "W-15",
        "`copy` 记录的载荷必须带源区间：`Absent` 是记录自相矛盾，`Undecidable` 是尾段没解出来，\
         两者都拒绝编码，不写出缺段的记录",
    ),
    (
        "recovery/journal.rs",
        "W-16",
        "`mark_repair_landed` 要有可标记的表区修复：`Absent`（没有修复）与 `Undecidable`（修复\
         事实读不出）都按「没有可标记的修复」返回 `NotWritten`，调用方不得据此推进进度",
    ),
    (
        "recovery/journal.rs",
        "W-17",
        "落盘失败后的内存补偿：只有重读到 `Known` 才把 `repair.state` 退回 `InFlight`；\
         `Absent` / `Undecidable` 没有可补偿的状态，且无论哪条都照常返回 `persist` 的错误",
    ),
    (
        "recovery/record.rs",
        "W-18",
        "写出前的尾段对账只在围栏给定时进行：`Absent` / `Undecidable` 没有声明的围栏，\
         不参与该对账，也不据此签发任何许可",
    ),
    (
        "recovery/record.rs",
        "W-19",
        "编码前的在飞位校验：只有 `Known` 且等于搬移游标才通过，`Absent` / `Undecidable` \
         与不相符的 `Known` 都返回错误，不写出记录、不签发写许可",
    ),
    (
        "recovery/record.rs",
        "W-20",
        "源区间段的形状与所属操作只在 `Known` 时对账：`copy` 缺段在编码侧由 W-15 拒绝、\
         在解码侧由段缺失拒读；`Undecidable` 只出现在前缀记录上，写路径不接纳前缀记录",
    ),
];

/// 去掉 `mod tests` 之后的行。`#[cfg(test)]` 访问器不算测试模块，仍留在生产行里
fn production(text: &str) -> Vec<(usize, &str)> {
    production_lines(text)
}

/// 同一个臂里同时出现 `Fact::Absent` 与 `Fact::Undecidable`
fn merges_absent_with_undecidable(line: &str) -> bool {
    let Some(pipe) = line.find('|') else { return false };
    let (left, right) = line.split_at(pipe);
    (left.contains("Fact::Absent") && right.contains("Fact::Undecidable"))
        || (left.contains("Fact::Undecidable") && right.contains("Fact::Absent"))
}

/// 这段代码是否用 `if let` / `let-else` 只接住 `Known`，把其余两态推进同一出口。
/// 两种写法都让 `Absent` 与 `Undecidable` 走同一条路，区别只在出口是 `else` 块还是
/// `else` 分叉
fn known_only_guard(line: &str) -> bool {
    line.contains("if let Fact::Known")
        || line.contains("if let crate::domain::Fact::Known")
        || line.contains("let Fact::Known")
        || line.contains("let crate::domain::Fact::Known")
}

/// 这一段是否是 `match` 一个 `Fact` 且带 `_` 臂——`_` 会把 `Absent` 与 `Undecidable`
/// 一并接住，等价于显式合并两臂。窗口按物理行取
fn match_with_wildcard_arm(prod: &[(usize, &str)], idx: usize, line: &str) -> bool {
    if !line.contains("match ") {
        return false;
    }
    let window: Vec<&str> = prod.iter().skip(idx).take(14).map(|(_, l)| *l).collect();
    let mentions_fact = window.iter().any(|l| l.contains("Fact::"));
    let wildcard = window.iter().any(|l| l.trim_start().starts_with("_ =>"));
    mentions_fact && wildcard
}

/// `|` 折行时把 or-pattern 接成一条逻辑行（行尾悬着单个 `|`，或下一个非注释行以单个 `|`
/// 开头），夹在两者之间的整行注释是 trivia，不并入拼接文本。`||` 是逻辑或，不参与拼接。
/// 返回 (首行在 `prod` 中的下标, 拼接文本)——`matches!` 的窗口仍按物理行取
fn logical_lines(prod: &[(usize, &str)]) -> Vec<(usize, String)> {
    let mut out = Vec::with_capacity(prod.len());
    let mut i = 0;
    while i < prod.len() {
        let mut text = prod[i].1.trim().to_string();
        let mut j = i + 1;
        if !text.starts_with("//") {
            loop {
                let mut k = j;
                while k < prod.len() && prod[k].1.trim_start().starts_with("//") {
                    k += 1;
                }
                let dangling = text.ends_with('|') && !text.ends_with("||");
                let next = prod.get(k).map(|(_, l)| l.trim_start()).unwrap_or_default();
                let leading = next.starts_with('|') && !next.starts_with("||");
                if !dangling && !leading {
                    break;
                }
                text.push(' ');
                text.push_str(next);
                j = k + 1;
            }
        }
        out.push((i, text));
        i = j.max(i + 1);
    }
    out
}

fn lines(input: &[&str]) -> Vec<(usize, String)> {
    let prod: Vec<(usize, &str)> =
        input.iter().enumerate().map(|(i, line)| (i + 1, *line)).collect();
    logical_lines(&prod)
}

#[test]
fn a_line_ending_with_a_single_pipe_is_joined_with_the_next() {
    assert_eq!(
        lines(&["Fact::Absent |", "Fact::Undecidable(_) => None,"]),
        vec![(0, "Fact::Absent | Fact::Undecidable(_) => None,".to_string())]
    );
}

#[test]
fn a_line_starting_with_a_single_pipe_is_joined_with_the_previous() {
    assert_eq!(
        lines(&["Fact::Absent", "| Fact::Undecidable(_) => None,"]),
        vec![(0, "Fact::Absent | Fact::Undecidable(_) => None,".to_string())]
    );
}

#[test]
fn logical_or_is_not_joined() {
    let trailing = lines(&["if a ||", "b {", "}"]);
    assert_eq!(trailing.len(), 3, "{trailing:?}");
    assert_eq!(trailing[0].1, "if a ||");
    let leading = lines(&["if a", "|| b {"]);
    assert_eq!(leading.len(), 2, "{leading:?}");
    assert_eq!(leading[1].1, "|| b {");
}

#[test]
fn a_comment_line_is_never_joined_to_the_next() {
    let out = lines(&["//! | 条件 | 落点 |", "| Fact::Undecidable(_) => None,"]);
    assert_eq!(out.len(), 2, "{out:?}");
    assert_eq!(out[0].1, "//! | 条件 | 落点 |");
    assert_eq!(out[1].1, "| Fact::Undecidable(_) => None,");
}

#[test]
fn a_comment_between_pattern_operands_is_skipped() {
    assert_eq!(
        lines(&[
            "Fact::Absent",
            "// 读不出与「确实没有」不是同一件事",
            "| Fact::Undecidable(_) => None,",
        ]),
        vec![(0, "Fact::Absent | Fact::Undecidable(_) => None,".to_string())]
    );
}

/// 这一行（逻辑行）是否把三态折成布尔量。`matches!` 的窗口按物理行取
fn folds_fact(prod: &[(usize, &str)], idx: usize, line: &str) -> bool {
    if line.contains("unwrap_or(false)")
        || line.contains("unwrap_or(true)")
        || merges_absent_with_undecidable(line)
        || known_only_guard(line)
        || match_with_wildcard_arm(prod, idx, line)    {
        return true;
    }
    line.contains("matches!(")
        && prod
            .iter()
            .skip(idx)
            .take(6)
            .map(|(_, l)| *l)
            .collect::<String>()
            .contains("Fact::")
}

#[test]
fn facts_are_not_folded_back_into_booleans() {
    let mut found: Vec<(String, String)> = Vec::new();
    let mut unmarked: Vec<String> = Vec::new();
    for (rel, text) in rust_sources() {
        if rel == "tests.rs" || rel.ends_with("/tests.rs") {
            continue;
        }
        let prod = production(&text);
        for (idx, line) in logical_lines(&prod) {
            if !folds_fact(&prod, idx, &line) {
                continue;
            }
            let ids = guards::marked_ids(&prod, idx, "W-");
            if ids.is_empty() {
                unmarked.push(format!("{rel}: {line}"));
                continue;
            }
            for id in ids {
                found.push((rel.clone(), id.to_string()));
            }
        }
    }
    found.sort();

    assert!(
        unmarked.is_empty(),
        "these lines fold a three-state fact but carry no `{}{{id}}` marker on the line directly \
         above — register the site in ALLOWED and mark it:\n{}",
        guards::MARKER,
        unmarked.join("\n")
    );

    let mut expected: Vec<(String, String)> = ALLOWED
        .iter()
        .map(|(file, id, _)| (file.to_string(), id.to_string()))
        .collect();
    expected.sort();
    assert_eq!(
        found, expected,
        "the set of registered fact-fold sites changed — every marker id needs exactly one \
         ALLOWED row (file + id), and every ALLOWED row exactly one marker in that file"
    );
}

/// `==` 由类型堵住：这四类只在 `cfg(test)` 下实现 `PartialEq` / `Eq`。生产代码里写
/// 相等比较编译不过；本守卫的判据因此落在源码里的 `cfg_attr(test, derive(...))`
#[test]
fn fact_types_derive_equality_only_under_test() {
    const TYPES: &[(&str, &str)] = &[
        ("domain.rs", "pub(crate) enum Fact<T> {"),
        ("recovery/record.rs", "pub(crate) struct Record {"),
        ("recovery/evidence.rs", "pub(crate) enum Evidence {"),
    ];
    const TEST_ONLY_EQ: &str = "#[cfg_attr(test, derive(PartialEq, Eq))]";
    for (file, decl) in TYPES {
        let text = source(file);
        let lines: Vec<&str> = text.lines().collect();
        let idx = lines
            .iter()
            .position(|l| l.trim() == *decl)
            .unwrap_or_else(|| panic!("{file}: declaration {decl:?} is gone"));
        let mut attrs: Vec<&str> = Vec::new();
        for line in lines[..idx].iter().rev() {
            let t = line.trim();
            if t.is_empty() || t.starts_with("//") {
                continue;
            }
            if t.starts_with("#[") {
                attrs.push(t);
                continue;
            }
            break;
        }
        assert!(
            attrs.contains(&TEST_ONLY_EQ),
            "{file}: {decl:?} must carry `{TEST_ONLY_EQ}` so equality is unavailable in \
             production builds"
        );
        for attr in attrs.iter().filter(|a| a.contains("PartialEq")) {
            assert_eq!(
                *attr, TEST_ONLY_EQ,
                "{file}: {decl:?} implements equality outside `cfg(test)` — production code could \
                 write `==` on a three-state fact again"
            );
        }
    }
}

#[test]
fn every_allowance_is_anchored_in_the_source_and_the_ledger() {
    let sources: Vec<(String, String)> = rust_sources();
    for (file, id, why) in ALLOWED {
        let text = sources
            .iter()
            .find(|(rel, _)| rel == file)
            .map(|(_, text)| text.as_str())
            .unwrap_or_else(|| panic!("{file} is gone; rewrite the allowance for {id}: {why}"));
        assert!(
            text.lines().any(|line| guards::marker_ids(line).contains(id)),
            "{file}: no `{}{id}` marker — the allowance points at nothing ({id}: {why})",
            guards::MARKER
        );
    }

    let ledger_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join(".trae/documents/读路径台账.md");
    let Ok(ledger_text) = std::fs::read_to_string(&ledger_path) else {
        // `.trae/` 是工作区资料，不随 crate 分发；缺档时只保留源码侧锚点
        return;
    };
    for (_, id, _) in ALLOWED {
        assert!(
            ledger_text.contains(id),
            "the ledger has no row {id} for an allowed fact fold — add the row before allowing it"
        );
    }
}
