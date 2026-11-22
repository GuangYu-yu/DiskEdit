//! `checked_*` 的失败不能折叠成值：`checked_add`/`checked_sub`/`checked_mul`/
//! `checked_div` 返回 `Option`，`None` 表达「这一步算不出来」。把 `None` 换成一个
//! 具体数值继续算，等于把「算不出来」伪装成「算出来是 0」——后续长度、下标、区间
//! 判断会拿着这个假值往下走。
//!
//! 允许的消费形态：`?`、`.ok_or`、`.ok_or_else`、`.map_err`、`let ... else`、
//! `match`/`if let`、`.and_then`/`.map`/`.filter`/`.is_*` 等保持 `Option` 的变换，
//! 以及 `.unwrap()`/`.expect()` 这类失败即中止的用法。`unwrap_or` 族里闭包走
//! `bail_fail`/`panic!`/`return` 的分支是显式拒绝，同样允许。
//!
//! 本 target 只揪一种形态：`checked_*` 的紧邻消费是 `.unwrap_or(`、`.map_or(`、
//! `.unwrap_or_default()`，或闭包不发散（不拒绝、不 panic、不 return）的
//! `.unwrap_or_else(`。豁免必须登记在 `KNOWN` 并写明真实理由；登记条目若不再命中，
//! 反向校验报悬空登记，避免修好后留下失效的白名单。

mod guards;

use guards::{anchor_of, code_only, is_word_byte, production_lines, rust_sources};

/// 允许的折叠例外：(相对路径, 整行锚点, 理由)。理由为空即视为无效登记
struct Known {
    file: &'static str,
    anchor: &'static str,
    reason: &'static str,
}

/// 当前没有例外：两处历史折叠已根修为普通除法/显式 `let-else`
const KNOWN: &[Known] = &[];

/// 把源码里被 `production_lines` 排除的测试模块行置空，再剥注释与字面量；
/// 行号与物理行一一对应，报告的行号即源码行号
fn production_code(text: &str) -> String {
    let total = text.lines().count();
    let mut by_line: Vec<&str> = vec![""; total];
    for (no, line) in production_lines(text) {
        by_line[no - 1] = line;
    }
    code_only(&by_line.join("\n"))
}

fn matching_paren(bytes: &[u8], open: usize) -> Option<usize> {
    let mut depth = 0usize;
    for (offset, &b) in bytes[open..].iter().enumerate() {
        match b {
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(open + offset);
                }
            }
            _ => {}
        }
    }
    None
}

/// 闭包体是否走了拒绝/中止路径
fn diverges(body: &str) -> bool {
    ["bail_fail(", "panic!", "unreachable!", "todo!", "unimplemented!", "return "]
        .iter()
        .any(|needle| body.contains(needle))
}

/// 紧跟在 `checked_*(...)` 之后的消费形态是否把失败折叠成值
fn folds_to_value(code: &str, after_close: usize) -> bool {
    let bytes = code.as_bytes();
    let rest = &code[after_close..];
    let trimmed = rest.trim_start();
    let start = after_close + (rest.len() - trimmed.len());
    if trimmed.starts_with(".unwrap_or(")
        || trimmed.starts_with(".map_or(")
        || trimmed.starts_with(".unwrap_or_default()")
    {
        return true;
    }
    if trimmed.starts_with(".unwrap_or_else(") {
        let open = start + ".unwrap_or_else".len();
        let Some(close) = matching_paren(bytes, open) else {
            return true;
        };
        let body = &code[open + 1..close];
        return !diverges(body);
    }
    false
}

#[test]
fn checked_results_are_not_collapsed_into_values() {
    let mut offenders: Vec<String> = Vec::new();
    for (file, text) in rust_sources() {
        let code = production_code(&text);
        let bytes = code.as_bytes();
        let raw_lines: Vec<&str> = text.lines().collect();
        let mut i = 0usize;
        while i < bytes.len() {
            if !code[i..].starts_with("checked_") {
                i += 1;
                continue;
            }
            let prev_word = i > 0 && is_word_byte(bytes[i - 1]);
            let mut j = i + "checked_".len();
            while j < bytes.len() && is_word_byte(bytes[j]) {
                j += 1;
            }
            if prev_word || j == i + "checked_".len() {
                i = j.max(i + 1);
                continue;
            }
            // 跳过空白找到调用的左括号
            let mut open = j;
            while open < bytes.len() && bytes[open].is_ascii_whitespace() {
                open += 1;
            }
            if open >= bytes.len() || bytes[open] != b'(' {
                i = j;
                continue;
            }
            let Some(close) = matching_paren(bytes, open) else {
                i = open + 1;
                continue;
            };
            if folds_to_value(&code, close + 1) {
                let line_no = bytes[..i].iter().filter(|&&b| b == b'\n').count() + 1;
                let anchor = raw_lines.get(line_no - 1).map(|l| anchor_of(l)).unwrap_or_default();
                offenders.push(format!("{file}:{line_no}: {anchor}"));
            }
            i = close + 1;
        }
    }

    let mut unregistered: Vec<String> = Vec::new();
    let mut matched = vec![false; KNOWN.len()];
    for offender in &offenders {
        let mut registered = false;
        for (idx, known) in KNOWN.iter().enumerate() {
            if offender.starts_with(&format!("{}:", known.file)) && offender.ends_with(known.anchor) {
                assert!(!known.reason.is_empty(), "豁免登记必须写明真实理由: {:?}", known.anchor);
                matched[idx] = true;
                registered = true;
            }
        }
        if !registered {
            unregistered.push(offender.clone());
        }
    }
    let dangling: Vec<String> = KNOWN
        .iter()
        .zip(&matched)
        .filter(|(_, hit)| !**hit)
        .map(|(known, _)| format!("{}: {}", known.file, known.anchor))
        .collect();

    assert!(
        unregistered.is_empty(),
        "checked_* 的失败被折叠成值（用 ?、ok_or、let-else、match 或显式拒绝消费，必要时登记理由）:\n{}",
        unregistered.join("\n")
    );
    assert!(dangling.is_empty(), "失效的豁免登记（对应折叠已不存在）:\n{}", dangling.join("\n"));
}
