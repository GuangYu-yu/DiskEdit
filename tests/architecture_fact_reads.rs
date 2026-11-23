//! 「缺失不得折叠为放行」的结构守卫。
//!
//! `Fact` 的三态（`Known` / `Absent` / `Undecidable`）由类型本身堵住 `==`：`Fact`
//! 与内嵌它的 `Record` / `Evidence` 只在 `cfg(test)` 下实现
//! `PartialEq` / `Eq`，生产代码里写相等比较编译不过。这条约束由本文件的
//! `fact_types_derive_equality_only_under_test` 钉住，不靠文本规则。
//!
//! 编译器拦不住的折法分**两个大类**，各成封闭清单，登记锚与计数口径互不混用：
//!
//! ## 类别一：`Fact 三态折叠`（[`FoldCategory::FactThreeState`]）
//!
//! 1. `unwrap_or(false|true)`：缺失被折成布尔量；
//! 2. `matches!` 的窗口里出现 `Fact::`：三态被折成一个 `bool`；
//! 3. `Fact::Absent | Fact::Undecidable(..)` 合流成同一个臂：读不出与「确实没有」
//!    走同一条路，正是 `Fact` 存在的理由所在。
//!
//! ## 类别二：`Result/IO 折叠`（[`FoldCategory::ResultIo`]）
//!
//! 把 `Result` / `io::Result` 的 `Err` 折成 `Option::None`、默认值或跳过臂。识别形态
//! 列在 [`RESULT_FORMS`]：`.ok()`、`.ok().flatten()`、`if let Ok(`、`let Ok(`、
//! `.unwrap_or(`、`.unwrap_or_default()`、`.map_err(...).ok()`。
//!
//! 每条放行必须带**方向**栏，取值只允许 [`DIRECTION_FAIL_CLOSED`]（读失败走上游拒绝 /
//! `Unknown` 侧）、[`DIRECTION_DIRECTION_SAFE`]（折落后落点保守，不会被当成「相同 /
//! 成立」）、[`DIRECTION_DISPLAY_ONLY`]（值只进输出串或收尾清理，不参与判定分支），
//! 不许留空。
//!
//! 站点数量超过逐条登记上限后按**类归并**：[`RESULT_CLASSES`] 里每个清单类给出一句话
//! 机械理由与逐条**内容锚四元** `(文件, 去注释去空白后的整行内容, 出现次序 k, 本文件
//! 同内容总条数 m)`。行号只进失败报告、不作判据：折叠行上方插行不改内容不应致病；
//! 内容被改/删判 `dangling`，k/m 与实际次序不符判 `count_mismatch`，形态在源码出现却
//! 找不到四元判 `unmarked`。不在登记表、也不在任何清单里的新增折叠点一律判红。
//! `if let Ok(` / `let Ok(` 两类与 `architecture_escape_sites` 的 S2 跳过站点一一
//! 对应，方向按各 S2 上游保证标注。
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

use guards::{join_or_patterns, production_lines, rust_sources, source};

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
    (
        "recovery/record.rs",
        "W-21",
        "修复事实的两个地址字段只在 `Known` 时按 `kind` 对账：`Absent` 没有动作可对，\
         `Undecidable` 是尾部段没读出来、解码阶段已整条拒读，均不在此判定",
    ),
    (
        "cmd/inspect.rs",
        "W-22",
        "inspect 是只读检查：`current_scene` 只喂 `read_evidence` 做身份比对，非 Known 走结构\
         分类；`Absent` 与 `Undecidable` 的区分由下方 declared 分支的两行 unavailable 文案分别\
         陈述，不产生写许可或「核对通过」结论",
    ),
    (
        "cmd/records.rs",
        "W-23",
        "`records list` 只读枚举：`current_ref` 只决定行内是否带当前身份比对；`Absent` 与\
         `Undecidable` 都无可比身份，落结构解码；读不出的原因由紧随其后的 eprintln 单独浮出，\
         不据此签发许可",
    ),
    (
        "cmd/release.rs",
        "W-24",
        "声明名只能套到已打开句柄上：`Absent`（目标不在）/`Undecidable`（读不出）没有句柄可\
         套，跳过绑定；二者在 `release_records` 的空枚举闸与归属比对闸各自拒绝，不据此放行",
    ),
    (
        "cmd/release.rs",
        "W-25",
        "枚举只按身份派生落点列出候选、不改写记录；句柄缺失时 `active_recovery_records` 不做\
         归属比对，`Absent`/`Undecidable` 在枚举之后的空枚举闸与归属比对闸分别拒绝，合流到\
         None 不构成释放或认领",
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

fn lines(input: &[&str]) -> Vec<(usize, String)> {
    join_or_patterns(&input.join("\n"))
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

/// 一条折叠违规：文件、首行行号、逻辑行内容。判定只产清单，报告文案在测试体里拼接。
#[derive(Debug, Clone, PartialEq)]
struct FactViolation {
    file: String,
    line_no: usize,
    content: String,
}

impl FactViolation {
    fn render(&self) -> String {
        format!("{}:{}: {}", self.file, self.line_no, self.content)
    }
}

/// 折叠普查结果：无标记站点、实际标记集合、登记表期望集合。
struct FactCensus {
    unmarked: Vec<FactViolation>,
    found: Vec<(String, String)>,
    expected: Vec<(String, String)>,
}

/// 纯判定：扫全部源码，找出把三态 Fact 折成布尔/单值的站点；无 `audit-allow` 标记的进
/// `unmarked`，带标记的按 (文件, id) 记入 `found`；`expected` 由登记表派生。
fn fact_fold_census(
    allowed: &[(&str, &str, &str)],
    sources: &[(String, String)],
) -> FactCensus {
    let mut unmarked: Vec<FactViolation> = Vec::new();
    let mut found: Vec<(String, String)> = Vec::new();
    for (rel, text) in sources {
        if rel == "tests.rs" || rel.ends_with("/tests.rs") {
            continue;
        }
        let prod = production(text);
        for (idx, line) in join_or_patterns(text) {
            if !folds_fact(&prod, idx, &line) {
                continue;
            }
            let ids = guards::marked_ids(&prod, idx, "W-");
            if ids.is_empty() {
                let line_no = prod.get(idx).map(|(n, _)| *n).unwrap_or(0);
                unmarked.push(FactViolation {
                    file: rel.clone(),
                    line_no,
                    content: line.clone(),
                });
                continue;
            }
            for id in ids {
                found.push((rel.clone(), id.to_string()));
            }
        }
    }
    found.sort();

    let mut expected: Vec<(String, String)> = allowed
        .iter()
        .map(|(file, id, _)| (file.to_string(), id.to_string()))
        .collect();
    expected.sort();

    FactCensus { unmarked, found, expected }
}

#[test]
fn facts_are_not_folded_back_into_booleans() {
    let census = fact_fold_census(ALLOWED, &rust_sources());

    let mut unmarked = census.unmarked;
    unmarked.sort_by(|a, b| a.file.cmp(&b.file).then(a.line_no.cmp(&b.line_no)));
    assert!(
        unmarked.is_empty(),
        "these lines fold a three-state fact but carry no `{}{{id}}` marker on the line directly \
         above — register the site in ALLOWED and mark it:\n{}",
        guards::MARKER,
        unmarked.iter().map(FactViolation::render).collect::<Vec<_>>().join("\n")
    );

    assert_eq!(
        census.found, census.expected,
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

/// 纯判定：每条登记在源码里是否还能找到自己的标记；文件缺失或标记被删都进违规清单。
fn allowance_missing_anchors(
    allowed: &[(&str, &str, &str)],
    sources: &[(String, String)],
) -> Vec<FactViolation> {
    let mut out = Vec::new();
    for (file, id, why) in allowed {
        let Some(text) = sources
            .iter()
            .find(|(rel, _)| rel == file)
            .map(|(_, t)| t.as_str())
        else {
            out.push(FactViolation {
                file: file.to_string(),
                line_no: 0,
                content: format!("{id}: 文件缺失 — {why}"),
            });
            continue;
        };
        if !text.lines().any(|line| guards::marker_ids(line).contains(id)) {
            out.push(FactViolation {
                file: file.to_string(),
                line_no: 0,
                content: format!("{id}: 无标记 — {why}"),
            });
        }
    }
    out
}

#[test]
fn every_allowance_is_anchored_in_the_source_and_the_ledger() {
    let sources: Vec<(String, String)> = rust_sources();
    let missing = allowance_missing_anchors(ALLOWED, &sources);
    assert!(
        missing.is_empty(),
        "these allowances point at nothing in the source:\n{}",
        missing.iter().map(FactViolation::render).collect::<Vec<_>>().join("\n")
    );

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

/// 合成样本自检（仓库外拼样）：`Absent` 与 `Undecidable` 合流到同一个 `None` 臂、又没有
/// `audit-allow` 标记，纯普查必须报出并定位到那一行；判据"会不会红"由 `cargo test` 回答。
#[test]
fn synthetic_violation_is_reported() {
    let sample = "fn classify(f: Fact<u64>) -> Option<u64> {\n    match f {\n        Fact::Absent |\n        Fact::Undecidable(_) => None,\n        Fact::Known(v) => Some(v),\n    }\n}\n";
    let census = fact_fold_census(&[], &[("sample.rs".to_string(), sample.to_string())]);

    assert_eq!(census.unmarked.len(), 1);
    let v = &census.unmarked[0];
    assert_eq!(v.file, "sample.rs");
    assert!(v.content.contains("Fact::Undecidable"));
    assert!(v.line_no >= 3 && v.line_no <= 4);
    assert!(census.found.is_empty());
}

/// 合成样本自检（仓库外拼样）：看着像折叠但按判据不算的写法必须零违规——
/// 带登记标记的 Known 守卫、窗口里没有 `Fact::` 的 `matches!`、显式三臂不含流的 `match`。
#[test]
fn equivalent_writing_is_not_reported() {
    // (a) 带登记标记的 Known 守卫：登记表里有对应行，不算无标记违规
    let marked = "// audit-allow: W-1\nif let Fact::Known(v) = f { Some(v) } else { None }\n";
    let registry = [("sample.rs", "W-1", "自检样本")];
    let census = fact_fold_census(&registry, &[("sample.rs".to_string(), marked.to_string())]);
    assert!(census.unmarked.is_empty());
    assert_eq!(census.found, census.expected);

    // (b) matches! 只匹配非 Fact 类型，窗口里没有 Fact::，不折三态
    let non_fact = "let ready = matches!(state, State::Done);\n";
    let census = fact_fold_census(&[], &[("sample.rs".to_string(), non_fact.to_string())]);
    assert!(census.unmarked.is_empty());
    assert!(census.found.is_empty());

    // (c) 显式三臂 match：Known/Absent/Undecidable 各自成臂，没有合流也没有 _ 兜底
    let three_arms = "let v = match f {\n    Fact::Known(v) => Some(v),\n    Fact::Absent => None,\n    Fact::Undecidable(_) => None,\n};\n";
    let census = fact_fold_census(&[], &[("sample.rs".to_string(), three_arms.to_string())]);
    assert!(census.unmarked.is_empty());
    assert!(census.found.is_empty());
}

// ============================================================================================
// 类别二：`Result/IO 折叠`
// ============================================================================================

/// 折叠大类。两套清单的登记锚、计数口径分开，互不搅乱
#[derive(Clone, Copy, PartialEq, Eq)]
enum FoldCategory {
    /// `Fact` 三态折进 `Option::None` / `bool` / 单臂合流
    FactThreeState,
    /// `Result` / `io::Result` 折进 `Option`、默认值或跳过臂
    ResultIo,
}

impl FoldCategory {
    const fn label(self) -> &'static str {
        match self {
            Self::FactThreeState => "Fact 三态折叠",
            Self::ResultIo => "Result/IO 折叠",
        }
    }
}

/// 方向栏：读失败折落后，落点对判定的影响
const DIRECTION_FAIL_CLOSED: &str = "fail-closed";
const DIRECTION_DIRECTION_SAFE: &str = "direction-safe";
const DIRECTION_DISPLAY_ONLY: &str = "display-only";

/// 方向栏合法取值；登记表与清单条目都必须落在其中，不许留空
const DIRECTIONS: &[&str] = &[
    DIRECTION_FAIL_CLOSED,
    DIRECTION_DIRECTION_SAFE,
    DIRECTION_DISPLAY_ONLY,
];

fn valid_direction(d: &str) -> bool {
    !d.is_empty() && DIRECTIONS.contains(&d)
}

/// Result/IO 折叠的识别形态（生产行；测试段由 `production_lines` 与文件名切分排除）。
/// `map_err(...).ok()` 当前 0 命中：形态保留在清单里，将来出现即红。
const RESULT_FORMS: &[(&str, &str)] = &[
    ("ok_flatten", ".ok().flatten()"),
    ("ok", ".ok()"),
    ("if_let_ok", "if let Ok("),
    ("let_ok", "let Ok("),
    ("unwrap_or_default", ".unwrap_or_default()"),
    ("unwrap_or", ".unwrap_or("),
    ("map_err_then_ok", ".map_err(...).ok()"),
];

/// 一行（已 trim）命中哪个 Result/IO 形态；未命中返回 `None`
fn result_fold_form(line: &str) -> Option<&'static str> {
    if line.contains(".ok().flatten()") {
        Some("ok_flatten")
    } else if line.contains(".map_err(") && line.contains(".ok()") {
        Some("map_err_then_ok")
    } else if line.contains(".ok()") {
        Some("ok")
    } else if line.contains("if let Ok(") {
        Some("if_let_ok")
    } else if line.contains("let Ok(") {
        Some("let_ok")
    } else if line.contains(".unwrap_or_default()") {
        Some("unwrap_or_default")
    } else if line.contains(".unwrap_or(") {
        Some("unwrap_or")
    } else {
        None
    }
}

/// 逐条登记的 Result/IO 折叠：(文件, 站点 id, 内容锚四元, 方向, 理由)。
/// 内容锚四元 = (整行内容, 出现次序 k, 本文件同内容总条数 m)；标记写在折叠点上一行：
/// `// audit-allow: R-<id>`。当前生产代码的 Result/IO 折叠全部落入下方按类归并的
/// 清单；本登记表留给需要逐条上游保证、不能被类理由覆盖的站点。
const RESULT_ALLOWED: &[(&str, &str, &str, usize, usize, &str, &str)] = &[];

/// 一个按类归并的放行清单：`reason` 是该类成立的机械理由，`sites` 逐条列内容锚四元与方向。
///
/// 锚是内容四元 `(文件, 去注释去空白后的整行内容, 该内容在本文件折叠行中的出现次序 k,
/// 该内容在本文件折叠行中的总条数 m)`；行号只进失败报告，不作判据——上方插行不改内容
/// 就不应致病，内容被改/删才判 dangling，k/m 与实际次序不符判 count_mismatch。
struct ResultClass {
    name: &'static str,
    reason: &'static str,
    sites: &'static [(&'static str, &'static str, usize, usize, &'static str)],
}

/// 测试夹具文件：与 `architecture_escape_sites` 同一口径
fn is_test_result_file(rel: &str) -> bool {
    rel == "tests.rs" || rel.ends_with("/tests.rs") || rel.contains("/tests/")
}

fn is_production_code_line(line: &str) -> bool {
    !line.trim_start().starts_with("//")
}

/// 切掉行尾 `//` 注释（跳过字符串/字符字面量内部的 `//`），返回注释前的片段
fn strip_line_comment(line: &str) -> &str {
    let bytes = line.as_bytes();
    let mut in_str = false;
    let mut in_chr = false;
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        if in_str {
            if c == b'\\' {
                i += 2;
                continue;
            }
            if c == b'"' {
                in_str = false;
            }
            i += 1;
            continue;
        }
        if in_chr {
            if c == b'\\' {
                i += 2;
                continue;
            }
            if c == b'\'' {
                in_chr = false;
            }
            i += 1;
            continue;
        }
        if c == b'"' {
            in_str = true;
            i += 1;
            continue;
        }
        if c == b'\'' {
            in_chr = true;
            i += 1;
            continue;
        }
        if c == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
            return &line[..i];
        }
        i += 1;
    }
    line
}

/// 内容锚归一化：去行尾注释、去首尾空白
fn normalize_line(line: &str) -> String {
    strip_line_comment(line).trim().to_string()
}

/// 按类归并的 Result/IO 折叠清单。清单类一句话定义：
///
/// - `ok-question-propagates`：`.ok()?` 把 `Err/None` 经 `?` 送上游错误出口；
/// - `ok-decode-or-refuse`：裸 `.ok()` 折成的 `None` 由解码/解析助手返回，上游据此整条拒绝；
/// - `ok-conservative-none`：裸 `.ok()` 折成的 `None` 被当作证据不足/不匹配，方向保守；
/// - `ok-display-or-best-effort`：折叠值只进提示文案、只读预览或收尾资源回收；
/// - `skip-on-read-failure`：`if let Ok` / `let Ok else` 读失败走跳过/回落臂（对应 S2 登记）；
/// - `option-default-constant`：接收者是 `Option`，默认值是常量或恒等回落，不折叠 IO 错误；
/// - `result-default-nondecision`：接收者是 `Result`，默认值不参与放行判定。
const RESULT_CLASSES: &[ResultClass] = &[
    ResultClass {
        name: "ok-question-propagates",
        reason: "`.ok()?`（或 `.ok()` 处于 `?` 传播链上）：`Err/None` 经 `?` 转成上游 `Result`/`Option` 的错误出口，函数在此退出，不产生放行分支或宽松默认值。",
        sites: &[
            ("cli.rs", "let int: u64 = if int_part.is_empty() { 0 } else { int_part.parse().ok()? };", 1, 1, DIRECTION_FAIL_CLOSED),
            ("cli.rs", "let frac: u64 = if frac_part.is_empty() { 0 } else { frac_part.parse().ok()? };", 1, 1, DIRECTION_FAIL_CLOSED),
            ("support.rs", "text[i] = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).ok()?;", 1, 1, DIRECTION_FAIL_CLOSED),
            ("cmd/resize/msdos.rs", ".ok()?;", 1, 1, DIRECTION_FAIL_CLOSED),
            ("dev/target.rs", ".and_then(|(a, b)| Some((a.parse::<u32>().ok()?, b.parse::<u32>().ok()?)))", 1, 1, DIRECTION_FAIL_CLOSED),
            ("exec/protocol.rs", "getrandom::fill(&mut bytes).ok()?;", 1, 1, DIRECTION_FAIL_CLOSED),
            ("exec/protocol.rs", "fstype: crate::fsops::parse_fstype(field(value, \"fstype\")?).ok()?,", 1, 1, DIRECTION_FAIL_CLOSED),
            ("exec/protocol.rs", "let kind = crate::table::TableWriteKind::from_tag(u8::try_from(u64_of(value, \"table_kind\")?).ok()?)?;", 1, 1, DIRECTION_FAIL_CLOSED),
            ("exec/protocol.rs", "let class = FailureClass::from_code(u8::try_from(u64_of(value, \"class\")?).ok()?)?;", 1, 1, DIRECTION_FAIL_CLOSED),
            ("fsops/ext.rs", "let out = self.read(&[\"--version\"]).ok()?;", 1, 1, DIRECTION_FAIL_CLOSED),
            ("fsops/mount.rs", "dev_no: (maj.parse().ok()?, min.parse().ok()?),", 1, 1, DIRECTION_FAIL_CLOSED),
            ("fsops/shrink.rs", ".ok()?;", 1, 2, DIRECTION_FAIL_CLOSED),
            ("fsops/shrink.rs", ".ok()?,", 1, 1, DIRECTION_FAIL_CLOSED),
            ("fsops/shrink.rs", "let (off, len) = partition_byte_range(src, part).ok()?;", 1, 1, DIRECTION_FAIL_CLOSED),
            ("fsops/shrink.rs", ".ok()?;", 2, 2, DIRECTION_FAIL_CLOSED),
            ("movepart/faults.rs", "let given = std::env::var(\"DISKEDIT_FAULT\").ok()?;", 1, 1, DIRECTION_FAIL_CLOSED),
            ("online/guard.rs", "Some((maj.parse().ok()?, min.parse().ok()?))", 1, 1, DIRECTION_FAIL_CLOSED),
            ("recovery/journal.rs", ".get(crate::support::host_usize(reloc.entry_index).ok()?)?;", 1, 1, DIRECTION_FAIL_CLOSED),
            ("recovery/record.rs", "let disk_guid = DiskGuidState::decode(c.take(crate::dev::CREDENTIAL_LEN)?).ok()?;", 1, 1, DIRECTION_FAIL_CLOSED),
            ("recovery/record.rs", "TargetId::from_material(IdentityMaterial::image(volume_serial, Some(file_id)).ok()?)", 1, 1, DIRECTION_FAIL_CLOSED),
            ("recovery/rollback.rs", "let cursor = crate::support::host_usize(reloc.entry_index).ok()?;", 1, 1, DIRECTION_FAIL_CLOSED),
        ],
    },
    ResultClass {
        name: "ok-decode-or-refuse",
        reason: "裸 `.ok()` 把 `Err` 折成 `Option::None`；该 `None` 由解码/解析助手返回，上游把 None 当作记录/帧/行不可信而拒绝或整条丢弃，不会当成「成立/相同」。",
        sites: &[
            ("gpt_policy.rs", ".ok()", 1, 1, DIRECTION_FAIL_CLOSED),
            ("lvm.rs", ".ok()", 1, 1, DIRECTION_FAIL_CLOSED),
            ("lvm.rs", "int.parse().ok()", 1, 1, DIRECTION_FAIL_CLOSED),
            ("exec/protocol.rs", "u32::try_from(u64_of(value, name)?).ok()", 1, 1, DIRECTION_FAIL_CLOSED),
            ("exec/protocol.rs", "bytes.try_into().ok()", 1, 1, DIRECTION_FAIL_CLOSED),
            ("exec/remote.rs", ".and_then(|index| crate::support::host_usize(index).ok())", 1, 1, DIRECTION_FAIL_CLOSED),
            ("exec/remote.rs", "u32::try_from(state.get(name)?.as_u64()?).ok()", 1, 1, DIRECTION_FAIL_CLOSED),
            ("recovery/record.rs", "String::from_utf8(bytes.to_vec()).ok()", 1, 1, DIRECTION_FAIL_CLOSED),
            ("recovery/record.rs", "String::from_utf8(c.take(len)?.to_vec()).ok()", 1, 1, DIRECTION_FAIL_CLOSED),
            ("table/mbr.rs", ".ok()", 1, 1, DIRECTION_FAIL_CLOSED),
        ],
    },
    ResultClass {
        name: "ok-conservative-none",
        reason: "裸 `.ok()` 把读失败折成 `None`；所在判定把 None 当作「证据不足/不匹配/跳过该候选」，下游据此不匹配或保守跳过，读失败不会被当成「相同/已成立」。",
        sites: &[
            ("support.rs", "u8::from_str_radix(hex, 16).ok()", 1, 1, DIRECTION_DIRECTION_SAFE),
            ("dev/identity.rs", "let regular = |p: &Path| std::fs::symlink_metadata(p).ok().filter(|m| m.file_type().is_file());", 1, 1, DIRECTION_DIRECTION_SAFE),
            ("dev/identity.rs", ".ok()", 1, 1, DIRECTION_DIRECTION_SAFE),
            ("dev/identity.rs", ".and_then(|_| std::fs::File::open(p).ok())", 1, 1, DIRECTION_DIRECTION_SAFE),
            ("dev/identity.rs", ".and_then(|f| TargetFingerprint::from_file(&f).ok())", 1, 1, DIRECTION_DIRECTION_SAFE),
            ("dev/identity.rs", "Ok(true) => return Self::resolve_block(path).ok(),", 1, 1, DIRECTION_DIRECTION_SAFE),
            ("exec/remote.rs", "child.kill().ok();", 1, 1, DIRECTION_DIRECTION_SAFE),
            ("exec/remote.rs", "child.wait().ok();", 1, 1, DIRECTION_DIRECTION_SAFE),
            ("fsops/grow.rs", "std::fs::metadata(path).ok().map(|m| (m.dev(), m.ino()))", 1, 1, DIRECTION_DIRECTION_SAFE),
            ("fsops/mount.rs", ".ok()", 1, 1, DIRECTION_DIRECTION_SAFE),
            ("fsops/shrink.rs", "Ok::<Option<bool>, FsError>(crate::lvm::pv_size_report(dev).ok().map(|r| r.pv_size <= requested))", 1, 1, DIRECTION_DIRECTION_SAFE),
            ("fsops/shrink.rs", "Ok::<Option<bool>, FsError>(read_fs_bytes(kind, dev).ok().map(|actual| actual <= requested))", 1, 1, DIRECTION_DIRECTION_SAFE),
            ("fsops/shrink.rs", "verdict = read_fs_bytes(FsKind::Btrfs, dev).ok().map(|actual| actual <= requested);", 1, 1, DIRECTION_DIRECTION_SAFE),
            ("movepart/faults.rs", ".and_then(|rest| rest.parse::<$ty>().ok())", 1, 1, DIRECTION_DIRECTION_SAFE),
            ("movepart/plan.rs", ".ok()", 1, 1, DIRECTION_DIRECTION_SAFE),
            ("movepart/plan.rs", "PlanKind::Shift => grow_end_for(plan, target_start).ok(),", 1, 1, DIRECTION_DIRECTION_SAFE),
            ("online/guard.rs", "s.trim().parse().ok()", 1, 1, DIRECTION_DIRECTION_SAFE),
            ("online/guard.rs", "std::fs::read_to_string(dir.join(\"dev\")).ok().and_then(|s| parse_dev_attr(&s)) == Some(dev)", 1, 1, DIRECTION_DIRECTION_SAFE),
            ("online/pv.rs", "let this_dev = std::fs::read_to_string(sysdir.join(\"dev\")).ok().and_then(|s| parse_dev_attr(&s));", 1, 1, DIRECTION_DIRECTION_SAFE),
            ("recovery/engine.rs", "ownership_scene(src).ok()", 1, 1, DIRECTION_DIRECTION_SAFE),
        ],
    },
    ResultClass {
        name: "ok-display-or-best-effort",
        reason: "折叠后的值只进入提示文案、只读预览或收尾资源回收，不参与任何写或许可判定分支。",
        sites: &[
            ("support.rs", "let file = std::fs::OpenOptions::new().create(true).append(true).open(path).ok();", 1, 1, DIRECTION_DISPLAY_ONLY),
            ("cmd/info_grow.rs", ".ok()", 1, 1, DIRECTION_DISPLAY_ONLY),
        ],
    },
    ResultClass {
        name: "skip-on-read-failure",
        reason: "`if let Ok(..)` / `let Ok(..) else`：读失败走 else 臂跳过当前候选或退回更弱证据；与 architecture_escape_sites 的 S2 登记一一对应，方向按各 S2 上游保证标注。",
        sites: &[
            ("dev/identity.rs", "if let Ok(meta) = std::fs::metadata(canonical) {", 1, 1, DIRECTION_DIRECTION_SAFE),
            ("dev/identity.rs", "if let Ok(file) = std::fs::File::open(canonical)", 1, 1, DIRECTION_DIRECTION_SAFE),
            ("fsops/grow.rs", "if let Ok(mut f) = std::fs::File::open(\"/dev/urandom\") {", 1, 1, DIRECTION_DIRECTION_SAFE),
            ("fsops/mount.rs", "if let Ok(st) = udev_settle()", 1, 1, DIRECTION_DIRECTION_SAFE),
            ("platform/probe.rs", "if let Ok(meta) = std::fs::metadata(spec.path())", 1, 1, DIRECTION_FAIL_CLOSED),
            ("recovery/engine.rs", "if let Ok(meta) = std::fs::metadata(a.target())", 1, 1, DIRECTION_FAIL_CLOSED),
            ("cli.rs", "&& let Ok(byte) = u8::from_str_radix(hex, 16)", 1, 1, DIRECTION_FAIL_CLOSED),
            ("ioctl.rs", "let Ok(start) = i64::try_from(start_bytes) else {", 1, 1, DIRECTION_FAIL_CLOSED),
            ("ioctl.rs", "let Ok(length) = i64::try_from(new_len_bytes) else {", 1, 1, DIRECTION_FAIL_CLOSED),
            ("ioctl.rs", "let Ok(pno) = i32::try_from(pno) else {", 1, 1, DIRECTION_FAIL_CLOSED),
            ("cmd/inspect.rs", "let Ok(src) = open_target(path, None) else {", 1, 1, DIRECTION_DIRECTION_SAFE),
            ("dev/identity.rs", "&& let Ok((volume, index)) = win::identity(file)", 1, 1, DIRECTION_DIRECTION_SAFE),
            ("dev/identity.rs", "&& let Ok((volume, index)) = win::identity(&file)", 1, 1, DIRECTION_DIRECTION_SAFE),
            ("dev/identity.rs", "let Ok(node) = sysfs_node(path) else { return true };", 1, 1, DIRECTION_FAIL_CLOSED),
            ("dev/target.rs", "&& let Ok(d) = std::fs::File::open(dir)", 1, 1, DIRECTION_DIRECTION_SAFE),
            ("fsops/grow.rs", "let Ok(own) = self.dir.metadata().map(|m| {", 1, 1, DIRECTION_DIRECTION_SAFE),
            ("fsops/mount.rs", "let Ok(pid) = name.parse::<u32>() else { continue };", 1, 1, DIRECTION_DIRECTION_SAFE),
            ("fsops/scope.rs", "let Ok(txt) = std::fs::read_to_string(&start_file) else { continue };", 1, 1, DIRECTION_DIRECTION_SAFE),
            ("fsops/scope.rs", "let Ok(start_sectors) = txt.trim().parse::<u64>() else { continue };", 1, 1, DIRECTION_DIRECTION_SAFE),
            ("movepart/faults.rs", "let Ok(v) = std::env::var(\"DISKEDIT_FAULT\") else { return };", 1, 1, DIRECTION_DIRECTION_SAFE),
            ("platform/evidence_store.rs", "let Ok(cname) = c_name(name) else {", 1, 1, DIRECTION_FAIL_CLOSED),
            ("recovery/engine.rs", "let Ok(scene) = ownership_scene(src) else {", 1, 1, DIRECTION_DIRECTION_SAFE),
            ("recovery/journal.rs", "let Ok((dir, name)) = companion_parts(record_path) else { return };", 1, 1, DIRECTION_DIRECTION_SAFE),
            ("recovery/journal.rs", "let Ok(dir_c) = std::ffi::CString::new(dir.as_os_str().as_bytes()) else { return };", 1, 1, DIRECTION_DIRECTION_SAFE),
            ("recovery/journal.rs", "let Ok(entry) = std::ffi::CString::new(file_name.as_bytes()) else { continue };", 1, 1, DIRECTION_DIRECTION_SAFE),
        ],
    },
    ResultClass {
        name: "option-default-constant",
        reason: "`.unwrap_or(..)` / `.unwrap_or_default()` 的接收者是 `Option`（链上只有 map/filter/max/parent/as_deref/strip_prefix 等 Option 组合子，没有返回 Result 的调用）；默认值是常量或恒等回落，不折叠任何 IO/解析错误。值只进输出串的标 display-only，其余 direction-safe。",
        sites: &[
            ("cli.rs", "flag_spec(flag).map(|s| s.domain.placeholder()).unwrap_or(\"VALUE\");", 1, 1, DIRECTION_DISPLAY_ONLY),
            ("docs.rs", "let column = rows.iter().map(|(s, _)| s.chars().count()).max().unwrap_or(0) + 2;", 1, 1, DIRECTION_DISPLAY_ONLY),
            ("docs.rs", "let column = synopses.iter().map(|s| s.chars().count()).max().unwrap_or(0) + 2;", 1, 1, DIRECTION_DISPLAY_ONLY),
            ("geometry.rs", ".unwrap_or(0);", 1, 1, DIRECTION_DIRECTION_SAFE),
            ("geometry.rs", "let last_usable = post_repair_last_usable.unwrap_or(g.header.last_usable_lba);", 1, 1, DIRECTION_DIRECTION_SAFE),
            ("gpt_policy.rs", "g.entries.iter().filter(|e| !crate::table::is_unused(e)).map(|e| e.ending_lba).max().unwrap_or(0)", 1, 1, DIRECTION_DIRECTION_SAFE),
            ("lvm.rs", "let int_part = tok.trim().split('.').next().unwrap_or(\"\");", 1, 1, DIRECTION_DIRECTION_SAFE),
            ("support.rs", "let hex = s.strip_prefix(\"0x\").or_else(|| s.strip_prefix(\"0X\")).unwrap_or(s);", 1, 1, DIRECTION_DIRECTION_SAFE),
            ("cmd/fs.rs", "let name = fsid::content_name(fstype.unwrap_or(ContentKind::Unknown));", 1, 1, DIRECTION_DISPLAY_ONLY),
            ("cmd/fs.rs", "&[fstype_name.as_bytes(), a.label.as_deref().unwrap_or(\"\").as_bytes()],", 1, 1, DIRECTION_DIRECTION_SAFE),
            ("cmd/fs.rs", "fsid::content_name(f.unwrap_or(ContentKind::Unknown))", 1, 1, DIRECTION_DISPLAY_ONLY),
            ("cmd/layout.rs", "let kind = a.table.unwrap_or(table::TableKind::Gpt);", 1, 1, DIRECTION_DIRECTION_SAFE),
            ("cmd/layout.rs", "a.name.as_deref().unwrap_or(\"\"),", 1, 2, DIRECTION_DISPLAY_ONLY),
            ("cmd/layout.rs", "align.unit_sectors(sector_size).unwrap_or(1)", 1, 1, DIRECTION_DIRECTION_SAFE),
            ("cmd/layout.rs", "let (fb_start, fb_end) = committed_old.unwrap_or((old_start, old_end));", 1, 1, DIRECTION_DIRECTION_SAFE),
            ("cmd/layout.rs", "a.name.as_deref().unwrap_or(\"\"),", 2, 2, DIRECTION_DISPLAY_ONLY),
            ("cmd/records.rs", "let dir = record.parent().unwrap_or(Path::new(\".\"));", 1, 1, DIRECTION_DIRECTION_SAFE),
            ("cmd/records.rs", "let dir = record.parent().unwrap_or(Path::new(\".\")).to_path_buf();", 1, 1, DIRECTION_DIRECTION_SAFE),
            ("cmd/release.rs", "let dir = path.parent().unwrap_or(path);", 1, 2, DIRECTION_DISPLAY_ONLY),
            ("cmd/release.rs", "let dir = path.parent().unwrap_or(path);", 2, 2, DIRECTION_DISPLAY_ONLY),
            ("cmd/resize/decide.rs", "fsid::content_name(fstype.unwrap_or(fsid::ContentKind::Unknown))", 1, 1, DIRECTION_DISPLAY_ONLY),
            ("cmd/resize/msdos.rs", ".unwrap_or(movepart::FsShrink::NotShrunk);", 1, 1, DIRECTION_DIRECTION_SAFE),
            ("dev/source.rs", "let sector_size = sector_size_override.unwrap_or(512);", 1, 1, DIRECTION_DIRECTION_SAFE),
            ("exec/remote.rs", ".unwrap_or(\"the worker reported an error\");", 1, 1, DIRECTION_DISPLAY_ONLY),
            ("execution/fs.rs", "crate::fsid::content_name(kind.unwrap_or(ContentKind::Unknown))", 1, 1, DIRECTION_DISPLAY_ONLY),
            ("execution/mod.rs", "(chunk.unwrap_or(0), 0, 0)", 1, 1, DIRECTION_DIRECTION_SAFE),
            ("fsops/attrs.rs", "content_name(fstype.unwrap_or(ContentKind::Unknown))", 1, 2, DIRECTION_DISPLAY_ONLY),
            ("fsops/attrs.rs", "content_name(fstype.unwrap_or(ContentKind::Unknown))", 2, 2, DIRECTION_DISPLAY_ONLY),
            ("fsops/attrs.rs", "content_name(fstype.unwrap_or(ContentKind::Unknown)),", 1, 1, DIRECTION_DISPLAY_ONLY),
            ("fsops/capabilities.rs", "content_name(f.unwrap_or(ContentKind::Unknown)),", 1, 1, DIRECTION_DISPLAY_ONLY),
            ("fsops/capabilities.rs", "content_name(fstype.unwrap_or(ContentKind::Unknown))", 1, 1, DIRECTION_DISPLAY_ONLY),
            ("fsops/capabilities.rs", "content_name(fstype.unwrap_or(ContentKind::Unknown)),", 1, 1, DIRECTION_DISPLAY_ONLY),
            ("fsops/capabilities.rs", "check_support(\"shrink\", content_name(fstype.unwrap_or(ContentKind::Unknown)), shrink_support(fstype))?;", 1, 1, DIRECTION_DISPLAY_ONLY),
            ("fsops/grow.rs", "content_name(fstype.unwrap_or(ContentKind::Unknown))", 1, 1, DIRECTION_DISPLAY_ONLY),
            ("fsops/grow.rs", "content_name(other.unwrap_or(ContentKind::Unknown))", 1, 1, DIRECTION_DISPLAY_ONLY),
            ("fsops/mount.rs", "let path = path.strip_suffix(\" (deleted)\").unwrap_or(path);", 1, 1, DIRECTION_DIRECTION_SAFE),
            ("fsops/shrink.rs", "content_name(other.unwrap_or(ContentKind::Unknown))", 1, 1, DIRECTION_DISPLAY_ONLY),
            ("movepart/resize.rs", "FsShrink::from_code(code).unwrap_or(FsShrink::NotShrunk)", 1, 1, DIRECTION_DIRECTION_SAFE),
            ("movepart/shift.rs", ".unwrap_or(m.first_lba)", 1, 1, DIRECTION_DIRECTION_SAFE),
            ("online/flow.rs", "crate::fsid::content_name(fstype.unwrap_or(ContentKind::Unknown))", 1, 1, DIRECTION_DISPLAY_ONLY),
            ("online/fs_grow.rs", "crate::fsid::content_name(fstype.unwrap_or(ContentKind::Unknown))", 1, 1, DIRECTION_DISPLAY_ONLY),
            ("online/part.rs", "let partx_ok = partx.as_ref().map(|o| o.status.success()).unwrap_or(false);", 1, 1, DIRECTION_DIRECTION_SAFE),
            ("platform/mod.rs", "let attr_tail = attr.rsplit('/').next().unwrap_or(attr);", 1, 1, DIRECTION_DIRECTION_SAFE),
            ("recovery/journal.rs", "prefix_digest.unwrap_or(current.prefix_digest)", 1, 1, DIRECTION_DIRECTION_SAFE),
            ("assembly.rs", "let preloaded = g.decided.as_ref().map(|d| d.regions(g.ss)).unwrap_or_default();", 1, 1, DIRECTION_DIRECTION_SAFE),
            ("cli.rs", "let value = text_of(p, Slot::Value).unwrap_or_default();", 1, 1, DIRECTION_DISPLAY_ONLY),
            ("cli.rs", "state: text_of(p, Slot::State).unwrap_or_default(),", 1, 1, DIRECTION_DISPLAY_ONLY),
            ("docs.rs", "None => cli::flag_spec(name).map(|f| f.desc.to_string()).unwrap_or_default(),", 1, 1, DIRECTION_DISPLAY_ONLY),
            ("geometry.rs", "self.decided.as_ref().map(|d| d.regions(self.ss)).unwrap_or_default()", 1, 1, DIRECTION_DIRECTION_SAFE),
            ("cmd/records.rs", ".unwrap_or_default()", 1, 3, DIRECTION_DISPLAY_ONLY),
            ("cmd/records.rs", ".unwrap_or_default()", 2, 3, DIRECTION_DISPLAY_ONLY),
            ("cmd/records.rs", ".unwrap_or_default()", 3, 3, DIRECTION_DISPLAY_ONLY),
            ("fsops/mount.rs", "let name = node.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();", 1, 1, DIRECTION_DIRECTION_SAFE),
            ("online/fs_grow.rs", "let mnt = t.mnt.as_deref().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();", 1, 2, DIRECTION_DISPLAY_ONLY),
            ("online/fs_grow.rs", "let mnt = t.mnt.as_deref().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();", 2, 2, DIRECTION_DISPLAY_ONLY),
        ],
    },
    ResultClass {
        name: "result-default-nondecision",
        reason: "接收者是 `Result`：默认值不参与放行判定——时间回退取 0 只作熵/时间戳种子；整数溢出回落 MAX 随后由相等比对拒绝。",
        sites: &[
            ("support.rs", ".unwrap_or(0)", 1, 1, DIRECTION_DIRECTION_SAFE),
            ("cmd/layout.rs", ".unwrap_or(0);", 1, 1, DIRECTION_FAIL_CLOSED),
            ("table/create.rs", "let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);", 1, 1, DIRECTION_DIRECTION_SAFE),
            ("table/step.rs", "let given = u32::try_from(entries.len()).unwrap_or(u32::MAX);", 1, 1, DIRECTION_FAIL_CLOSED),
        ],
    },
];

/// 一条已列出的放行站点：次序 k/m + 所属登记表/清单类（文件与内容锚由索引键携带）
struct ListedSite<'a> {
    k: usize,
    m: usize,
    class: &'a str,
}

/// Result/IO 折叠普查结果：
/// - `unmarked`：源码命中形态但四元找不到对应登记/清单；
/// - `dangling`：列出的四元在源码里找不到（内容被改/删）；
/// - `count_mismatch`：列出的 k/m 与源码实际次序/总条数不符；
/// - `found` / `expected`：逐条登记表的标记集合；
/// - `bad_direction` / `duplicate`：方向栏非法、同一四元重复收录。
#[derive(Debug, Clone, PartialEq)]
struct ResultCensus {
    unmarked: Vec<FactViolation>,
    dangling: Vec<FactViolation>,
    count_mismatch: Vec<FactViolation>,
    found: Vec<(String, String)>,
    expected: Vec<(String, String)>,
    bad_direction: Vec<FactViolation>,
    duplicate: Vec<FactViolation>,
}

fn result_fold_census(
    allowed: &[(&str, &str, &str, usize, usize, &str, &str)],
    classes: &[ResultClass],
    sources: &[(String, String)],
) -> ResultCensus {
    let mut unmarked: Vec<FactViolation> = Vec::new();
    let mut dangling: Vec<FactViolation> = Vec::new();
    let mut count_mismatch: Vec<FactViolation> = Vec::new();
    let mut found: Vec<(String, String)> = Vec::new();
    let mut bad_direction: Vec<FactViolation> = Vec::new();
    let mut duplicate: Vec<FactViolation> = Vec::new();

    // 生产折叠行：file -> [(行号, 归一化整行内容)]；行号只进报告
    let mut folds: std::collections::HashMap<&str, Vec<(usize, String)>> =
        std::collections::HashMap::new();
    for (rel, text) in sources {
        if is_test_result_file(rel) {
            continue;
        }
        let prod = production(text);
        let mut rows: Vec<(usize, String)> = Vec::new();
        for (no, line) in prod {
            if !is_production_code_line(line) {
                continue;
            }
            if result_fold_form(line.trim()).is_none() {
                continue;
            }
            rows.push((no, normalize_line(line)));
        }
        folds.insert(rel.as_str(), rows);
    }

    // 列出条目索引：(文件, 归一化内容) -> 该内容的全部列出站点
    let mut listed: std::collections::HashMap<(String, String), Vec<ListedSite>> =
        std::collections::HashMap::new();

    for (file, id, content, k, m, dir, why) in allowed {
        if !valid_direction(dir) {
            bad_direction.push(FactViolation {
                file: file.to_string(),
                line_no: 0,
                content: format!("{id}: 方向栏缺失或非法（允许 fail-closed / direction-safe / display-only）— {why}"),
            });
        }
        listed
            .entry((file.to_string(), content.to_string()))
            .or_default()
            .push(ListedSite { k: *k, m: *m, class: id });
    }
    for class in classes {
        for (file, content, k, m, dir) in class.sites {
            if !valid_direction(dir) {
                bad_direction.push(FactViolation {
                    file: file.to_string(),
                    line_no: 0,
                    content: format!("类 {} 站点方向栏缺失或非法", class.name),
                });
            }
            let key = (file.to_string(), content.to_string());
            if listed
                .get(&key)
                .map(|v| v.iter().any(|s| s.k == *k))
                .unwrap_or(false)
            {
                duplicate.push(FactViolation {
                    file: file.to_string(),
                    line_no: 0,
                    content: format!("内容锚 {content:?} 第 {k} 次出现被多个来源重复收录"),
                });
            }
            listed
                .entry(key)
                .or_default()
                .push(ListedSite { k: *k, m: *m, class: class.name });
        }
    }

    // 锚点核对：四元在源码找不到 → dangling；k/m 与实际不符 → count_mismatch
    for ((file, content), sites) in &listed {
        let occurrences: Vec<&String> = folds
            .get(file.as_str())
            .map(|rows| {
                rows.iter()
                    .filter(|(_, c)| c == content)
                    .map(|(_, c)| c)
                    .collect()
            })
            .unwrap_or_default();
        if occurrences.is_empty() {
            for site in sites {
                dangling.push(FactViolation {
                    file: file.clone(),
                    line_no: 0,
                    content: format!("[{}] 内容锚在源码中已找不到：{content:?}", site.class),
                });
            }
            continue;
        }
        let actual_m = occurrences.len();
        for site in sites {
            if site.m != actual_m || site.k == 0 || site.k > actual_m {
                count_mismatch.push(FactViolation {
                    file: file.clone(),
                    line_no: 0,
                    content: format!(
                        "[{}] 内容锚次序不符：登记 k={} m={}，实际 m={} — {content:?}",
                        site.class, site.k, site.m, actual_m
                    ),
                });
            }
        }
    }

    // 源码覆盖核对：按本文件内同内容出现次序 k 匹配列出四元
    for (rel, rows) in &folds {
        let mut seen: std::collections::HashMap<&String, usize> =
            std::collections::HashMap::new();
        for (no, content) in rows {
            let k = seen.entry(content).and_modify(|v| *v += 1).or_insert(1);
            let m = rows.iter().filter(|(_, c)| c == content).count();
            let covered = listed
                .get(&(rel.to_string(), content.clone()))
                .map(|sites| sites.iter().any(|s| s.k == *k && s.m == m))
                .unwrap_or(false);
            if !covered {
                unmarked.push(FactViolation {
                    file: rel.to_string(),
                    line_no: *no,
                    content: content.clone(),
                });
            }
        }
    }

    // 逐条登记表：标记仍须出现在源码里
    for (file, id, _, _, _, _, why) in allowed {
        let Some(text) = sources
            .iter()
            .find(|(rel, _)| rel == file)
            .map(|(_, t)| t.as_str())
        else {
            dangling.push(FactViolation {
                file: file.to_string(),
                line_no: 0,
                content: format!("{id}: 文件缺失 — {why}"),
            });
            continue;
        };
        let prod = production(text);
        if !prod.iter().any(|(_, l)| guards::marker_ids(l).contains(id)) {
            dangling.push(FactViolation {
                file: file.to_string(),
                line_no: 0,
                content: format!("{id}: 无标记 — {why}"),
            });
        }
        found.push((file.to_string(), id.to_string()));
    }
    found.sort();
    let mut expected: Vec<(String, String)> = allowed
        .iter()
        .map(|(file, id, _, _, _, _, _)| (file.to_string(), id.to_string()))
        .collect();
    expected.sort();

    ResultCensus {
        unmarked,
        dangling,
        count_mismatch,
        found,
        expected,
        bad_direction,
        duplicate,
    }
}

#[test]
fn result_io_folds_are_all_covered() {
    let census = result_fold_census(RESULT_ALLOWED, RESULT_CLASSES, &rust_sources());

    assert!(
        census.unmarked.is_empty(),
        "这些 Result/IO 折叠既未逐条登记，也不在任何归并清单里——先归类登记再放行：\n{}",
        census.unmarked.iter().map(FactViolation::render).collect::<Vec<_>>().join("\n")
    );
    assert_eq!(
        census.found, census.expected,
        "Result/IO 登记表与源码标记不一致：每个 R- 标记需要恰好一条登记，每条登记需要恰好一个标记"
    );
    assert!(
        census.dangling.is_empty(),
        "这些内容锚四元在源码中已找不到（内容被改/删，或标记不在）：\n{}",
        census.dangling.iter().map(FactViolation::render).collect::<Vec<_>>().join("\n")
    );
    assert!(
        census.count_mismatch.is_empty(),
        "这些内容锚的 k/m 次序与源码实际不符：\n{}",
        census.count_mismatch.iter().map(FactViolation::render).collect::<Vec<_>>().join("\n")
    );
    assert!(
        census.bad_direction.is_empty(),
        "这些条目方向栏缺失或非法：\n{}",
        census.bad_direction.iter().map(FactViolation::render).collect::<Vec<_>>().join("\n")
    );
    assert!(
        census.duplicate.is_empty(),
        "这些坐标被多个清单类重复收录：\n{}",
        census.duplicate.iter().map(FactViolation::render).collect::<Vec<_>>().join("\n")
    );
}

/// 自检：[`RESULT_FORMS`] 列的每个形态都能被识别判据命中；每个归并清单类都有唯一名
/// 与机械理由；两个大类的显示名固定。
#[test]
fn result_forms_and_classes_are_documentable() {
    let samples: &[(&str, &str)] = &[
        ("ok_flatten", "let k = v.ok().flatten();"),
        ("ok", "let k = v.ok();"),
        ("if_let_ok", "if let Ok(meta) = std::fs::metadata(p) {"),
        ("let_ok", "let Ok(meta) = std::fs::metadata(p) else { return None };"),
        ("unwrap_or_default", "let v = opt.unwrap_or_default();"),
        ("unwrap_or", "let v = opt.unwrap_or(0);"),
        ("map_err_then_ok", "let v = r.map_err(|e| e).ok();"),
    ];
    for (id, sample) in samples {
        assert_eq!(result_fold_form(sample), Some(*id), "形态 {id} 未被识别: {sample}");
    }
    for (id, _) in RESULT_FORMS {
        assert!(samples.iter().any(|(s_id, _)| s_id == id), "形态 {id} 缺自检样本");
    }

    let mut names = std::collections::HashSet::new();
    for class in RESULT_CLASSES {
        assert!(!class.reason.is_empty(), "类 {} 缺机械理由", class.name);
        assert!(names.insert(class.name), "类名重复: {}", class.name);
        for (_, content, k, m, _) in class.sites {
            assert!(!content.is_empty(), "类 {} 有空内容锚", class.name);
            assert!(*k >= 1 && *k <= *m, "类 {} 内容锚 k/m 非法: {k}/{m}", class.name);
        }
    }

    assert_eq!(FoldCategory::FactThreeState.label(), "Fact 三态折叠");
    assert_eq!(FoldCategory::ResultIo.label(), "Result/IO 折叠");
}

/// 合成样本自检：一处未登记的 `.ok()` 折叠必须判红并定位到行。
#[test]
fn an_unregistered_result_fold_is_flagged() {
    let sample = "fn probe(r: std::io::Result<u64>) -> Option<u64> {\n    r.ok()\n}\n";
    let census = result_fold_census(&[], RESULT_CLASSES, &[("sample.rs".to_string(), sample.to_string())]);

    assert_eq!(census.unmarked.len(), 1);
    let v = &census.unmarked[0];
    assert_eq!(v.file, "sample.rs");
    assert!(v.content.contains(".ok()"));
    assert_eq!(v.line_no, 2);
    assert!(census.found.is_empty());
}

/// 合成样本自检：同一处已登记折叠改写成跨行 `.ok()`、`if let Ok(`、`let Ok(` 三种
/// 等价形态都仍匹配登记，零误红。
#[test]
fn a_registered_result_fold_and_equivalent_writings_are_not_flagged() {
    let samples: [(&str, &str); 3] = [
        // 跨行方法链：折叠点 `.ok()` 与登记标记同在首条语句上
        (
            "let v = std::fs::metadata(p).ok()",
            "// audit-allow: R-1\nlet v = std::fs::metadata(p).ok()\n    .map(|m| m.dev());\n",
        ),
        // `if let Ok(` 守卫形态
        (
            "if let Ok(meta) = std::fs::metadata(p) { Some(meta.dev()) } else { None }",
            "// audit-allow: R-1\nif let Ok(meta) = std::fs::metadata(p) { Some(meta.dev()) } else { None }\n",
        ),
        // `let Ok(..) else` 形态
        (
            "let Ok(meta) = std::fs::metadata(p) else { return None };",
            "// audit-allow: R-1\nlet Ok(meta) = std::fs::metadata(p) else { return None };\n",
        ),
    ];
    for (content, sample) in samples {
        let registry = [(
            "sample.rs",
            "R-1",
            content,
            1,
            1,
            DIRECTION_FAIL_CLOSED,
            "自检样本：读失败走上游拒绝侧",
        )];
        let census = result_fold_census(
            &registry,
            &[],
            &[("sample.rs".to_string(), sample.to_string())],
        );
        assert!(
            census.unmarked.is_empty(),
            "等价写法被误判为未登记折叠:\n{sample}\n{}",
            census.unmarked.iter().map(FactViolation::render).collect::<Vec<_>>().join("\n")
        );
        assert!(census.dangling.is_empty(), "等价写法被误判为 dangling:\n{sample}");
        assert!(census.count_mismatch.is_empty(), "等价写法次序被误判:\n{sample}");
        assert_eq!(census.found, census.expected);
    }
}

/// 自检用清单站点：内容锚 `r.ok()`，k=1 m=1
const SELF_TEST_SITES: &[(&str, &str, usize, usize, &str)] =
    &[("sample.rs", "r.ok()", 1, 1, DIRECTION_FAIL_CLOSED)];

/// 锚点稳定性自检：在已登记折叠行**上方插一行注释**（折叠行内容不变），普查仍应为绿。
#[test]
fn shifting_a_registered_line_does_not_break_the_census() {
    let classes = [ResultClass {
        name: "self-test-shift",
        reason: "自检：上方插行不改折叠行内容，内容锚不应致病",
        sites: SELF_TEST_SITES,
    }];
    let original = "fn probe(r: std::io::Result<u64>) -> Option<u64> {\n    r.ok()\n}\n";
    let shifted = "// 一行新增的注释\nfn probe(r: std::io::Result<u64>) -> Option<u64> {\n    r.ok()\n}\n";
    for text in [original, shifted] {
        let census = result_fold_census(
            &[],
            &classes,
            &[("sample.rs".to_string(), text.to_string())],
        );
        assert!(census.unmarked.is_empty(), "插行后折叠被误判未登记:\n{text}");
        assert!(census.dangling.is_empty(), "插行后内容锚被误判 dangling:\n{text}");
        assert!(census.count_mismatch.is_empty(), "插行后次序被误判:\n{text}");
    }
}

/// 锚点稳定性自检：把已登记行的折叠形态改掉（`.ok()` → `.expect(..)`，不再是识别
/// 形态），列出的内容锚在源码中找不到，必须判 dangling。
#[test]
fn changing_a_registered_fold_shape_is_reported_as_dangling() {
    let classes = [ResultClass {
        name: "self-test-dangling",
        reason: "自检：折叠形态被改掉后内容锚应判 dangling",
        sites: SELF_TEST_SITES,
    }];
    let changed = "fn probe(r: std::io::Result<u64>) -> u64 {\n    r.expect(\"missing\")\n}\n";
    let census = result_fold_census(&[], &classes, &[("sample.rs".to_string(), changed.to_string())]);

    assert!(census.unmarked.is_empty(), ".expect 不在识别形态内，不应另报未登记");
    assert_eq!(census.dangling.len(), 1);
    assert_eq!(census.dangling[0].file, "sample.rs");
    assert!(census.dangling[0].content.contains("r.ok()"));
    assert!(census.count_mismatch.is_empty());
}
