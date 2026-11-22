//! 完成判定与"已落盘提交点"两个答案来源的封口普查（R10 结构式收口）。
//!
//! "作业做完了吗"只有一个答案来源：[`Record::completion_verdict`] 三态
//! （`Complete` / `NeedsReplay` / `MustRefuse`），收尾放行只看 `allows_finish()`。
//! "盘上有没有已落盘的提交点"是另一个问题，只由 [`Record::has_recorded_work`] 回答：
//! 它只说有没有已落盘的提交点，不回答能否收尾；反过来 `NeedsReplay` / `MustRefuse`
//! 都不许被读成"没有记录"。
//!
//! `Record::committed` 是 `record` 模块私有布尔，只供 `completion_verdict` 与
//! `derive_stage` 内部穷尽匹配；recovery 之外直接读它在编译期就过不了。本守卫把这道
//! 封口再钉一层文本普查：
//!
//! - 生产行上每一处 `completion_verdict(` / `has_recorded_work(` 调用都必须落在登记
//!   表里。登记身份是（文件、规范化整行内容、该内容出现次序、总条数、问题类别）；
//!   匹配按从整行提取的调用形状（入口 + 接收者占位 + 调用后缀）进行，接收者拼写、别名
//!   与跨行换行不影响命中。`completed` 问"作业做完没"，`work-recorded` 问"是否存在
//!   已提交记录"。未登记站点、悬空登记、同形状条数不符都变红。
//! - `src/` 下若重新出现 `pub(crate) fn committed` / `pub fn committed`（`Record`
//!   上下文）即红。`cleanup.rs` 里返回 `&CommittedRecord` 的同名访问器是另一个类型，
//!   按返回类型排除，不与 `Record::committed` 混谈。
//!
//! 剥注释/剥字符串与切 `#[cfg(test)]` 测试模块复用 [`guards`] 既有原语：测试模块体由
//! [`guards::production_lines`] 切掉，注释与字面量内部由 [`guards::code_only`] 置空。

mod guards;

use guards::{Violation, code_only, is_fixture_file, production_lines, rust_sources};
use std::collections::BTreeMap;

/// 站点问题类别：这处调用在问哪一个问题。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Category {
    /// 问"作业做完没"：走 `completion_verdict()`，必要时 `.allows_finish()` 或三态穷尽 match
    Completed,
    /// 问"是否存在已提交记录"：走 `has_recorded_work()`，只说有没有落盘提交点
    WorkRecorded,
}

impl Category {
    fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::WorkRecorded => "work-recorded",
        }
    }

    /// 该类别对应的入口方法名
    fn entry(self) -> &'static str {
        match self {
            Self::Completed => "completion_verdict",
            Self::WorkRecorded => "has_recorded_work",
        }
    }

    fn from_entry(entry: &str) -> Option<Self> {
        match entry {
            "completion_verdict" => Some(Self::Completed),
            "has_recorded_work" => Some(Self::WorkRecorded),
            _ => None,
        }
    }
}

/// 登记条目：文件 + 规范化整行内容（匹配按其调用形状）+ 出现次序 + 该形状登记总条数
/// + 类别 + 理由。行号不参与匹配，只在失败报告里由普查现场带出。
struct GateSite {
    file: &'static str,
    line: &'static str,
    occurrence: usize,
    count: usize,
    category: Category,
    reason: &'static str,
}

const REGISTRY: &[GateSite] = &[
    // completed：问"作业做完没"，完成与否只认 `completion_verdict` 的放行结论
    GateSite {
        file: "cmd/inspect.rs",
        line: "if record.completion_verdict().allows_finish() {",
        occurrence: 1,
        count: 1,
        category: Category::Completed,
        reason: "inspect 出口码：完成才给 Checked，其余形态走续跑/回放/释放文案",
    },
    GateSite {
        file: "cmd/records.rs",
        line: "} else if record.completion_verdict().allows_finish() {",
        occurrence: 1,
        count: 1,
        category: Category::Completed,
        reason: "records list 状态展示：完成记 Valid，其余记 Unfinished",
    },
    GateSite {
        file: "cmd/recover.rs",
        line: "if record.completion_verdict().allows_finish() {",
        occurrence: 1,
        count: 1,
        category: Category::Completed,
        reason: "recover 提交后未清理分支：只释放记录、不回滚区间",
    },
    GateSite {
        file: "recovery/engine.rs",
        line: "recorded.is_some_and(|r| r.op.resumable() && !r.completion_verdict().allows_finish());",
        occurrence: 1,
        count: 1,
        category: Category::Completed,
        reason: "续跑准入闸：已完成记录不参与续跑，避免第二次写（engine 闸 1）",
    },
    GateSite {
        file: "recovery/engine.rs",
        line: "if record.completion_verdict().allows_finish() {",
        occurrence: 1,
        count: 1,
        category: Category::Completed,
        reason: "busy 出路文案闸：完成记录只给释放/回放出路（engine 闸 2）",
    },
    GateSite {
        file: "recovery/engine.rs",
        line: "one.slot.resumes_in_place() && record.op == op && !record.completion_verdict().allows_finish()",
        occurrence: 1,
        count: 2,
        category: Category::Completed,
        reason: "table 记录续跑准入闸（engine 闸 3）",
    },
    GateSite {
        file: "recovery/engine.rs",
        line: "if record.op == crate::recovery::OpKind::Resize && !record.completion_verdict().allows_finish()",
        occurrence: 2,
        count: 2,
        category: Category::Completed,
        reason: "resize 记录续跑准入闸（engine 闸 4）",
    },
    // work-recorded：问"有没有已落盘的提交点"，不回答能否收尾
    GateSite {
        file: "dev/source.rs",
        line: "crate::outcome::note_record_present(record.has_recorded_work());",
        occurrence: 1,
        count: 1,
        category: Category::WorkRecorded,
        reason: "接管盘上记录时标记是否已定型；只报提交点存在，不问能否收尾",
    },
    GateSite {
        file: "dev/source.rs",
        line: "if record.restore_blocked || record.has_recorded_work() {",
        occurrence: 1,
        count: 1,
        category: Category::WorkRecorded,
        reason: "越过不可逆点判定：提交点已落盘即不可回滚",
    },
];

/// 一条逻辑行：首行 1-based 行号（只用于报告定位）与规范化后的整行内容
struct LogicalLine {
    line_no: usize,
    text: String,
}

/// 关联函数形态 `R::completion_verdict(record)` 改写成方法形态 `record.completion_verdict()`：
/// 别名导入（`use ... as R`）后的调用与直写方法调用是同一站点，匹配不看接收者拼写。
fn rewrite_associated(s: &mut String) {
    for entry in ["completion_verdict", "has_recorded_work"] {
        let needle = format!("::{entry}(");
        while let Some(at) = s.find(&needle) {
            let prefix = &s[..at];
            let recv_start = prefix
                .rfind(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                .map(|p| p + 1)
                .unwrap_or(0);
            let open = at + needle.len() - 1;
            let Some(close) = s[open..].find(')') else { break };
            let close = open + close;
            let arg = s[open + 1..close].trim().trim_end_matches(',').trim();
            let plain_arg = arg
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_')
                && !arg.is_empty();
            if plain_arg {
                let repl = format!("{arg}.{entry}()");
                s.replace_range(recv_start..close + 1, &repl);
            } else {
                break;
            }
        }
    }
}

/// 规范化一条逻辑行：空白折成单空格；`.` / `::` 两侧空白去掉；关联调用改写成方法形态。
fn normalize(raw: &str) -> String {
    let mut collapsed = String::with_capacity(raw.len());
    let mut prev_space = true;
    for c in raw.chars() {
        if c.is_whitespace() {
            if !prev_space {
                collapsed.push(' ');
                prev_space = true;
            }
        } else {
            collapsed.push(c);
            prev_space = false;
        }
    }
    let mut s = collapsed.trim().to_string();
    loop {
        let before = s.clone();
        s = s
            .replace(" . ", ".")
            .replace(" .", ".")
            .replace(". ", ".")
            .replace(" :: ", "::")
            .replace(" ::", "::")
            .replace(":: ", "::");
        if s == before {
            break;
        }
    }
    rewrite_associated(&mut s);
    s
}

/// 生产行拼成逻辑行：行尾悬着 `.` / `&&` / `||`，或下一行以 `.` / `&&` / `||` 开头时
/// 接成一条；`use` 导入行不参与站点识别（别名导入不该被当成调用，也不锚定内容）。
fn logical_production_lines(text: &str) -> Vec<LogicalLine> {
    let code = code_only(text);
    let code_lines: Vec<&str> = code.lines().collect();
    let phys: Vec<(usize, String)> = production_lines(text)
        .iter()
        .map(|(n, _)| {
            let stripped = code_lines.get(n - 1).copied().unwrap_or("").trim().to_string();
            (*n, stripped)
        })
        .filter(|(_, t)| !t.is_empty() && !t.starts_with("use "))
        .collect();

    let mut out: Vec<LogicalLine> = Vec::new();
    let mut i = 0;
    while i < phys.len() {
        let (mut line_no, mut text) = phys[i].clone();
        let mut j = i + 1;
        loop {
            let trailing = text.ends_with('.') || text.ends_with("&&") || text.ends_with("||");
            let Some((next_no, next)) = phys.get(j) else { break };
            let leading = next.starts_with('.') || next.starts_with("&&") || next.starts_with("||");
            if !trailing && !leading {
                break;
            }
            text.push(' ');
            text.push_str(next);
            line_no = line_no.min(*next_no);
            j += 1;
        }
        out.push(LogicalLine { line_no, text: normalize(&text) });
        i = j.max(i + 1);
    }
    out
}

/// 一个调用站点：首行号、规范化整行原文、调用形状、入口方法名
#[derive(Clone)]
struct Site {
    line_no: usize,
    content: String,
    shape: String,
    entry: &'static str,
}

/// 调用形状：入口方法 + 接收者占位 + 入口调用闭括号之后的后缀。接收者拼写、`use ... as`
/// 别名、接收者跨行与 `&&` 链式换行都不参与匹配；整行原文留在 `content` 与登记 `line`
/// 里供报告与人工复核。
fn call_shape(content: &str, entry: &str) -> String {
    let needle = format!(".{entry}(");
    let Some(at) = content.find(&needle) else { return content.to_string() };
    let open = at + needle.len() - 1;
    let mut depth = 0i32;
    let mut close = open;
    for (i, c) in content[open..].char_indices() {
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    close = open + i;
                    break;
                }
            }
            _ => {}
        }
    }
    let suffix = content[close + 1..].trim();
    format!("<R>.{entry}({suffix}")
}

fn sites_in(text: &str) -> Vec<Site> {
    let mut out = Vec::new();
    for line in logical_production_lines(text) {
        // 定义行不是调用点：`fn completion_verdict` / `fn has_recorded_work` 本身
        if line.text.contains("fn completion_verdict") || line.text.contains("fn has_recorded_work") {
            continue;
        }
        for entry in ["completion_verdict", "has_recorded_work"] {
            let needle = format!(".{entry}(");
            let mut from = 0;
            while let Some(at) = line.text[from..].find(&needle) {
                let at = from + at;
                let shape = call_shape(&line.text, entry);
                out.push(Site { line_no: line.line_no, content: line.text.clone(), shape, entry });
                from = at + needle.len();
            }
        }
    }
    out
}

struct Census {
    unregistered: Vec<Violation>,
    dangling: Vec<Violation>,
    count_mismatch: Vec<Violation>,
}

fn run_census(registry: &[GateSite], sources: &[(String, String)]) -> Census {
    let mut sites_by_key: BTreeMap<(String, String), Vec<Site>> = BTreeMap::new();
    for (path, text) in sources {
        if is_fixture_file(path) {
            continue;
        }
        for site in sites_in(text) {
            sites_by_key
                .entry((path.clone(), site.shape.clone()))
                .or_default()
                .push(site);
        }
    }

    // 同形状出现次序编号（1 起）
    let mut numbered: Vec<(String, Site, usize)> = Vec::new();
    for ((path, _), sites) in &sites_by_key {
        for (k, site) in sites.iter().enumerate() {
            numbered.push((path.clone(), site.clone(), k + 1));
        }
    }

    let mut unregistered: Vec<Violation> = Vec::new();
    let mut matched: Vec<(String, String, usize, Category)> = Vec::new();
    for (path, site, occurrence) in &numbered {
        let category = Category::from_entry(site.entry).unwrap();
        let row_matches = |row: &GateSite| {
            let row_shape = call_shape(row.line, row.category.entry());
            row.file == path
                && row_shape == site.shape
                && row.occurrence == *occurrence
                && row.category == category
        };
        match registry.iter().find(|row| row_matches(row)) {
            Some(row) => {
                assert!(
                    !row.reason.is_empty(),
                    "登记条目 {}:{} 必须带理由",
                    row.file, row.line
                );
                matched.push((path.clone(), site.shape.clone(), *occurrence, category));
            }
            None => unregistered.push(Violation {
                file: path.clone(),
                line_no: site.line_no,
                category: category.as_str(),
                content: site.content.clone(),
            }),
        }
    }

    // 反向锁定：每条登记必须仍能在现存代码里找到对应形状；调用被删/被改即报悬空登记
    let mut dangling: Vec<Violation> = Vec::new();
    for row in registry {
        let row_shape = call_shape(row.line, row.category.entry());
        if !matched.iter().any(|(p, shape, k, cat)| {
            p == row.file && shape == &row_shape && *k == row.occurrence && *cat == row.category
        }) {
            dangling.push(Violation {
                file: row.file.to_string(),
                line_no: 0,
                category: row.category.as_str(),
                content: format!("{} — {}", row.line, row.reason),
            });
        }
    }

    // 总条数锁定：同一形状在本文件的现存条数必须与登记总数相等
    let mut registered_total: BTreeMap<(String, String), usize> = BTreeMap::new();
    for row in registry {
        let row_shape = call_shape(row.line, row.category.entry());
        let key = (row.file.to_string(), row_shape);
        match registered_total.get(&key) {
            Some(&old) => assert_eq!(
                old, row.count,
                "登记内部不一致：{}:{} 同形状条目登记了两个不同总数",
                row.file, row.line
            ),
            None => {
                registered_total.insert(key, row.count);
            }
        }
    }
    let mut count_mismatch: Vec<Violation> = Vec::new();
    for ((file, shape), &registered) in &registered_total {
        let actual = sites_by_key
            .get(&((*file).to_string(), (*shape).to_string()))
            .map(|v| v.len())
            .unwrap_or(0);
        if registered != actual {
            count_mismatch.push(Violation {
                file: (*file).to_string(),
                line_no: 0,
                category: "count-mismatch",
                content: format!("登记总数 {registered}，现存 {actual} 条：{shape}"),
            });
        }
    }

    Census { unregistered, dangling, count_mismatch }
}

#[test]
fn completion_and_work_sites_are_registered() {
    let census = run_census(REGISTRY, &rust_sources());

    let mut unregistered = census.unregistered;
    unregistered.sort_by(|a, b| a.file.cmp(&b.file).then(a.line_no.cmp(&b.line_no)));
    assert!(
        unregistered.is_empty(),
        "这些生产行读完成判定/提交点但未登记入口类别——问'作业做完没'走 completion_verdict，\
         问'有没有已提交记录'走 has_recorded_work，登记理由后再放行：\n{}",
        unregistered.iter().map(Violation::render).collect::<Vec<_>>().join("\n")
    );

    let mut dangling = census.dangling;
    dangling.sort_by(|a, b| a.file.cmp(&b.file).then(a.content.cmp(&b.content)));
    assert!(
        dangling.is_empty(),
        "登记条目已无对应代码行，先复核再撤登记：\n{}",
        dangling.iter().map(Violation::render).collect::<Vec<_>>().join("\n")
    );

    let mut count_mismatch = census.count_mismatch;
    count_mismatch.sort_by(|a, b| a.file.cmp(&b.file).then(a.content.cmp(&b.content)));
    assert!(
        count_mismatch.is_empty(),
        "同内容调用条数与登记总数不符（多出的逐字相同行不能冒用旧登记）：\n{}",
        count_mismatch.iter().map(Violation::render).collect::<Vec<_>>().join("\n")
    );
}

/// `Record::committed` 不得被重新放回 crate 可见面。
///
/// 判定规则：剥注释后的生产行上出现 `pub(crate) fn committed(` 或 `pub fn committed(`
/// 即候选；`cleanup.rs` 的 `Evidence::committed(&self) -> &CommittedRecord` 是另一个
/// 类型的访问器（返回 `&CommittedRecord`），按返回类型排除。`Record` 上下文的
/// `committed` 返回 `bool`，一旦恢复 `pub(crate)` / `pub` 可见性即变红；模块私有的
/// `fn committed(`（无可见性修饰）是允许的内部穷尽匹配入口。
#[test]
fn record_committed_accessor_stays_private() {
    struct VisibleCommitted {
        returns_committed_record: bool,
    }

    fn visible_fn_committed(line: &str) -> Option<VisibleCommitted> {
        let trimmed = line.trim_start();
        let is_visible = trimmed.starts_with("pub(crate) fn committed(")
            || trimmed.starts_with("pub fn committed(");
        if !is_visible {
            return None;
        }
        let returns_committed_record = match line.split_once("->") {
            Some((_, ret)) => ret.contains("CommittedRecord"),
            None => false,
        };
        Some(VisibleCommitted { returns_committed_record })
    }

    let mut offenders: Vec<String> = Vec::new();
    for (path, text) in rust_sources() {
        if is_fixture_file(&path) {
            continue;
        }
        let code = code_only(&text);
        let code_lines: Vec<&str> = code.lines().collect();
        for (n, _) in production_lines(&text) {
            let Some(code_line) = code_lines.get(n - 1).copied() else { continue };
            let Some(site) = visible_fn_committed(code_line) else { continue };
            if site.returns_committed_record {
                continue;
            }
            offenders.push(format!("{path}:{n}: {}", code_line.trim()));
        }
    }

    assert!(
        offenders.is_empty(),
        "`Record::committed` 必须保持模块私有；recovery 之外读提交位布尔应经 \
         `completion_verdict` 或 `has_recorded_work`：\n{}",
        offenders.join("\n")
    );
}

/// 匹配器自检：合成源码里多出第六个未登记的 `has_recorded_work()` 调用，必须判红并定位行。
#[test]
fn a_synthetic_sixth_reader_is_flagged() {
    let registered = "record.has_recorded_work();";
    let registry = [GateSite {
        file: "sample.rs",
        line: registered,
        occurrence: 1,
        count: 1,
        category: Category::WorkRecorded,
        reason: "自检样本",
    }];
    let source = format!(
        "fn first(record: &Record) {{\n    {registered}\n}}\n\n\
         fn sixth(record: &Record) {{\n    note(record.has_recorded_work());\n}}\n"
    );
    let census = run_census(&registry, &[("sample.rs".to_string(), source)]);

    assert_eq!(census.unregistered.len(), 1, "多出的第六个读者必须判红");
    let bad = &census.unregistered[0];
    assert_eq!(bad.file, "sample.rs");
    assert!(bad.line_no > 0, "违规必须定位到现存源码行");
    assert!(bad.content.contains("has_recorded_work"));
    assert!(census.dangling.is_empty());
    assert!(census.count_mismatch.is_empty());
}

/// 匹配器自检：一条已登记调用的三种等价改写都仍匹配原登记，零误红。
///
/// 形态一：接收者与 `.completion_verdict()` 跨行；
/// 形态二：`use crate::recovery::Record as R;` 别名后以关联函数形态 `R::completion_verdict(record)` 调用；
/// 形态三：方法链换行挂在上一行末尾 `&&` 之后。
#[test]
fn equivalent_writings_of_a_registered_site_are_not_flagged() {
    let registered = "record.completion_verdict().allows_finish();";
    let registry = [GateSite {
        file: "sample.rs",
        line: registered,
        occurrence: 1,
        count: 1,
        category: Category::Completed,
        reason: "自检样本",
    }];

    // 形态一：接收者与方法跨行
    let receiver_split = "fn f(record: &Record) {\n    record\n        .completion_verdict().allows_finish();\n}\n";
    // 形态二：别名导入 + 关联函数形态调用
    let aliased = "use crate::recovery::Record as R;\n\nfn f(record: &R) {\n    R::completion_verdict(record).allows_finish();\n}\n";
    // 形态三：`&&` 悬在上一行末尾，调用链换行续写
    let chained = "fn f(record: &Record) {\n    let ok = record.restore_blocked\n        && record.completion_verdict().allows_finish();\n    ok\n}\n";

    for (name, source) in [("receiver-split", receiver_split), ("aliased", aliased), ("chained", chained)] {
        let census = run_census(&registry, &[("sample.rs".to_string(), source.to_string())]);
        assert!(
            census.unregistered.is_empty(),
            "{name} 被误判为未登记站点：{}",
            census.unregistered.iter().map(Violation::render).collect::<Vec<_>>().join("\n")
        );
        assert!(census.dangling.is_empty(), "{name} 让原登记悬空了");
        assert!(census.count_mismatch.is_empty(), "{name} 条数不符");
    }
}

/// 匹配器自检：未登记的 `completion_verdict()` 读者同样判红，守卫不能只锁住提交点入口。
#[test]
fn unregistered_verdict_reader_is_flagged() {
    let registry: [GateSite; 0] = [];
    let source = "fn f(record: &Record) {\n    record.completion_verdict().allows_finish()\n}\n";
    let census = run_census(&registry, &[("sample.rs".to_string(), source.to_string())]);
    assert_eq!(census.unregistered.len(), 1);
    assert_eq!(census.unregistered[0].category, "completed");
    assert!(census.unregistered[0].line_no > 0);
}
