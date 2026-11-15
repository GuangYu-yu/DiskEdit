//! 「缺失不得折叠为放行」的结构守卫。
//!
//! `Fact` 的三态（`Known` / `Absent` / `Undecidable`）由类型本身堵住 `==`：`Fact`
//! 与内嵌它的 `Record` / `Evidence` / `Prefix` 只在 `cfg(test)` 下实现
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
//! 每条按「文件 + 该行原文」登记，原文改了定位就失效；新增一处就失败，必须同时
//! 给出豁免理由与台账行号。台账缺失时跳过台账核对（`.trae/` 是工作区资料，不随
//! crate 分发）；台账在时，每条豁免的台账行号必须能查到。
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

/// 白名单：(文件, 该行 trim 后的原文, 台账行号, 为什么它不是「缺失折成放行」)
const ALLOWED: &[(&str, &str, &str, &str)] = &[
    (
        "cmd/records.rs",
        "if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {",
        "W-2",
        "主机仓库目录枚举跳过类型读不出的项；不签发写许可（站点 S1-6，待修）",
    ),
    (
        "cmd/records.rs",
        "if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {",
        "W-3",
        "sidecar 目录枚举同上（站点 S1-7，待修）",
    ),
    (
        "online/part.rs",
        "let partx_ok = partx.as_ref().map(|o| o.status.success()).unwrap_or(false);",
        "W-4",
        "partx 状态读不出时走 BLKPG 回退，回退失败即报错（站点 S1-10，待复核）",
    ),
    (
        "recovery/cleanup.rs",
        "let fence_settled = matches!(",
        "W-5",
        "表步围栏只有 Pending/Landed 算已定；Absent/Undecidable 落扣留许可一侧",
    ),
    (
        "recovery/cleanup.rs",
        "let no_in_flight = matches!(record.entry_table_in_flight, Fact::Absent);",
        "W-6",
        "条目在飞位只有「确实没有」才放行；Known/Undecidable 落扣留许可一侧",
    ),
    (
        "recovery/record.rs",
        "matches!(self.entry_table_in_flight, Fact::Known(in_flight) if in_flight == entry)",
        "W-7",
        "受控访问器 `table_step_in_flight`：只有 Known 且等于该条目才答「在飞」，Absent/Undecidable \
         都答否。调用点据此返回 Stage::Undecidable，不签发写许可",
    ),
    (
        "recovery/journal.rs",
        "Fact::Absent | Fact::Undecidable(_) => None,",
        "W-8",
        "`source()` 把「没有源区间」与「源区间读不出」都答成 None；唯一调用点在首写前拒绝 \
         None，不构成放行",
    ),
    (
        "recovery/journal.rs",
        "Fact::Absent | Fact::Undecidable(_) => {",
        "W-9",
        "`mark_table_step_started`：围栏读不出时不猜「表步未开始」，按拒绝返回",
    ),
    (
        "recovery/journal.rs",
        "Fact::Absent | Fact::Undecidable(_) => None,",
        "W-10",
        "`repair()` 的 `None` 有两条来源：`Absent` 是确实没有修复动作，`apply_repair` 据此\
         继续是正常路径；`Undecidable` 只由前缀解码产生，而写入路径的日志只来自新建记录\
         或完整解码的 `Recognized`（`open_image` 走 `Evidence::classify`），该臂当前不可达。\
         若将来前缀记录也能进入写路径，`Undecidable` 会与 `Absent` 合流成「无需修复」，\
         `apply_repair` 随即写修复——那时必须拆臂",
    ),
    (
        "recovery/record.rs",
        "Fact::Absent | Fact::Undecidable(_) => return Ok(Stage::Undecidable),",
        "W-11",
        "`derive_stage` 把围栏读不出与「没有围栏」都折成 Undecidable 阶段——两者都不签发\
         写许可",
    ),
    (
        "recovery/record.rs",
        "Fact::Absent | Fact::Undecidable(_) => return Err(tail_not_decoded()),",
        "W-12",
        "载荷编码：尾段读不出的记录不得重写，编码直接失败，不把 Undecidable 写成 Absent",
    ),
    (
        "recovery/rollback.rs",
        "crate::domain::Fact::Absent | crate::domain::Fact::Undecidable(_) => {",
        "W-13",
        "`copy` 回滚前的源复核：拿不到源区间就拒绝写回，两条 `None` 都走同一条拒绝分支，\
         不构成放行",
    ),
    (
        "recovery/record.rs",
        "} else if matches!(self.table_fence, Fact::Known(TableFence::InFlight)) {",
        "W-14",
        "`settle_table_step_after_rollback`：只在回滚把记录登记的表区原文写回并核对之后调用，\
         围栏明确记着 `InFlight` 才退回 `Pending`。`Absent` / `Undecidable` / `Pending` / \
         `Landed` 都不动，清理许可仍按记录原值判——读不出的围栏不会被当成「已落定」",
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

/// 这段代码是否用 `if let` 只接住 `Known`，把其余两态推进 `else`
fn if_let_known(line: &str) -> bool {
    line.contains("if let Fact::Known") || line.contains("if let crate::domain::Fact::Known")
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

#[test]
fn facts_are_not_folded_back_into_booleans() {
    let mut found: Vec<(String, String)> = Vec::new();
    for (rel, text) in rust_sources() {
        if rel == "tests.rs" || rel.ends_with("/tests.rs") {
            continue;
        }
        let prod = production(&text);
        for (idx, line) in logical_lines(&prod) {
            if line.contains("unwrap_or(false)") || line.contains("unwrap_or(true)") {
                found.push((rel.clone(), line.clone()));
            }
            if merges_absent_with_undecidable(&line) || if_let_known(&line) {
                found.push((rel.clone(), line.clone()));
            }
            if line.contains("matches!(") {
                let window: String = prod.iter().skip(idx).take(6).map(|(_, l)| *l).collect();
                if window.contains("Fact::") {
                    found.push((rel.clone(), line));
                }
            }
        }
    }
    found.sort();

    let mut expected: Vec<(String, String)> = ALLOWED
        .iter()
        .map(|(file, line, _, _)| (file.to_string(), line.to_string()))
        .collect();
    expected.sort();
    assert_eq!(
        found, expected,
        "the set of sites that fold a three-state fact into a bool changed — every new site \
         must be registered in ALLOWED together with a ledger row, and every removed site must \
         lose its row"
    );
}

/// `==` 由类型堵住：这四类只在 `cfg(test)` 下实现 `PartialEq` / `Eq`。生产代码里写
/// 相等比较编译不过；本守卫的判据因此落在源码里的 `cfg_attr(test, derive(...))`
#[test]
fn fact_types_derive_equality_only_under_test() {
    const TYPES: &[(&str, &str)] = &[
        ("domain.rs", "pub(crate) enum Fact<T> {"),
        ("recovery/record.rs", "pub(crate) struct Record {"),
        ("recovery/record.rs", "pub(crate) struct Prefix {"),
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
            if t.is_empty() || t.starts_with("///") {
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
    for (file, line, ledger, why) in ALLOWED {
        let text = sources
            .iter()
            .find(|(rel, _)| rel == file)
            .map(|(_, text)| text.as_str())
            .unwrap_or_else(|| panic!("{file} is gone; rewrite the allowance for {ledger}: {why}"));
        assert!(
            logical_lines(&production(text)).iter().any(|(_, l)| l == line),
            "{file}: the line this allowance pins is gone — update the allowance together with \
             the code ({ledger}: {why})"
        );
    }

    let ledger_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join(".trae/documents/读路径台账.md");
    let Ok(ledger_text) = std::fs::read_to_string(&ledger_path) else {
        // `.trae/` 是工作区资料，不随 crate 分发；缺档时只保留源码侧锚点
        return;
    };
    for (_, _, id, _) in ALLOWED {
        assert!(
            ledger_text.contains(id),
            "the ledger has no row {id} for an allowed fact fold — add the row before allowing it"
        );
    }
}
