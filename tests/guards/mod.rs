//! 守卫 target 共用的只读工具：源码读取、标记顺序、进程级命令入口。
//!
//! 这些 target 把「计划步骤只有一个执行器」「码值只有一份映射」「记录自证且无版本号」
//! 三条结构约束固化成对源码的可机械复核断言；其中 `cmd_info`、`cmd_info_grow` 另以进程级
//! 只读命令核对输出字段与退出码。目标字节的零写检查由既有 `no_write_probe`（Linux，
//! 递归 sha256 快照）与 `cli/platform_gate`（非 Linux）承担。结构与行为两类断言不互相替代。
//!
//! 每个 integration test 是独立 crate，各自只用到本模块的一部分函数；未使用的 `pub`
//! 项在该 crate 内必然触发 `dead_code`，故模块级放行（全仓唯一的模块级 allow）。

#![allow(dead_code)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

pub fn src_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src")
}

/// 读一份源码；缺文件直接失败，不静默跳过（否则守卫会随文件改名失效）
pub fn source(rel: &str) -> String {
    let path = src_root().join(rel);
    std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()))
}

/// `src/` 下全部 `.rs` 的 (相对路径, 内容)。跳过 `tests/` 子目录——测试夹具里的直写
/// 不属于生产写点，不该被「不得新增写路径」的断言误伤
pub fn rust_sources() -> Vec<(String, String)> {
    fn walk(dir: &Path, out: &mut Vec<(String, String)>) {
        let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)
            .unwrap_or_else(|e| panic!("cannot list {}: {e}", dir.display()))
            .map(|e| e.expect("read a directory entry").path())
            .collect();
        entries.sort();
        for path in entries {
            if path.is_dir() {
                if path.file_name().is_some_and(|n| n == "tests") {
                    continue;
                }
                walk(&path, out);
            } else if path.extension().is_some_and(|e| e == "rs") {
                let rel = path
                    .strip_prefix(src_root())
                    .expect("walk started at src/")
                    .to_string_lossy()
                    .replace('\\', "/");
                let text = std::fs::read_to_string(&path)
                    .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
                out.push((rel, text));
            }
        }
    }
    let mut out = Vec::new();
    walk(&src_root(), &mut out);
    out
}

/// 去掉尾部测试模块之后的行。`#[cfg(test)]` 访问器不算测试模块，仍留在生产行里；
/// 返回 (1-based 行号, 行文本)。
///
/// 测试模块的判据是"`#[cfg(...)` 里含 `test`，随后（跳过属性与注释）声明的是模块"：
/// `#[cfg(test)]` 与 `#[cfg(all(test, target_os = "linux"))]` 都成立。只看字面量
/// `#[cfg(test)]` 会把带平台的测试模块当生产代码，让只针对生产写的断言在测试夹具上误报
pub fn production_lines(text: &str) -> Vec<(usize, &str)> {
    let lines: Vec<&str> = text.lines().collect();
    let mut out = Vec::new();
    // 已见到 `#[cfg(...test...)]`，等它修饰的那条声明（属性与注释行不算）
    let mut pending = false;
    // 正在跳过的测试模块体：该模块声明行的缩进。模块体在更深一级，故只有回到这一级
    // 的 `}` 才收尾——按花括号计数会被测试里的格式串（`"{{{}}}"` 之类）带偏
    let mut skipping: Option<usize> = None;
    for (i, line) in lines.iter().enumerate() {
        if let Some(indent) = skipping {
            if indent_of(line) == indent && line.trim_start().starts_with('}') {
                skipping = None;
            }
            continue;
        }
        if pending {
            let t = line.trim_start();
            if t.starts_with("#[") || t.starts_with("//") {
                continue;
            }
            pending = false;
            if declares_module(t) {
                if opens_body(line) {
                    skipping = Some(indent_of(line));
                }
                continue;
            }
            // 这条 `cfg(test)` 修饰的不是模块：该行按生产行计
            out.push((i + 1, *line));
            continue;
        }
        if is_test_module_attr(line) {
            if declares_module(line) {
                if opens_body(line) {
                    skipping = Some(indent_of(line));
                }
            } else {
                pending = true;
            }
            continue;
        }
        out.push((i + 1, *line));
    }
    out
}

fn indent_of(line: &str) -> usize {
    line.len() - line.trim_start().len()
}

/// 声明行是否带一个跨行的模块体：`mod x;` 与整行写完的模块都不进入跳过状态
fn opens_body(line: &str) -> bool {
    line.matches('{').count() > line.matches('}').count()
}

/// `#[cfg(...)]` 且其中出现 `test`：`#[cfg(test)]`、`#[cfg(all(test, target_os = "linux"))]`
fn is_test_module_attr(line: &str) -> bool {
    let attr = line.trim();
    attr.starts_with("#[cfg(") && attr.contains("test")
}

/// 这一行声明的是模块吗：先剥掉属性与可见性修饰，再看首词是否为 `mod`。
/// 按子串找 `mod ` 认不出 `pub(crate) mod fixtures`，属性里的空格也会干扰判断
fn declares_module(line: &str) -> bool {
    let mut rest = line.trim_start();
    loop {
        if let Some(after) = rest.strip_prefix("#[") {
            let Some(at) = after.find(']') else { return false };
            rest = after[at + 1..].trim_start();
        } else if let Some(after) = rest.strip_prefix("pub(") {
            let Some(at) = after.find(')') else { return false };
            rest = after[at + 1..].trim_start();
        } else if let Some(after) = rest.strip_prefix("pub") {
            // `public_thing` 这类标识符不是可见性修饰
            if !after.chars().next().is_some_and(char::is_whitespace) {
                return false;
            }
            rest = after.trim_start();
        } else {
            break;
        }
    }
    rest.split_whitespace().next() == Some("mod")
}

/// 豁免标记的行首。标记写在被豁免语句的上一行，形如 `// audit-allow: W-2 S1-6`：
/// 一个标记可列多个 id，各守卫只取自己命名空间下的那些
pub const MARKER: &str = "// audit-allow:";

/// 一条标记行里的 id 列表；不是标记行时为空
pub fn marker_ids(line: &str) -> Vec<&str> {
    line.trim()
        .strip_prefix(MARKER)
        .map(|rest| rest.split_whitespace().collect())
        .unwrap_or_default()
}

/// `prod[i]` 所述语句的标记里属于 `prefix` 命名空间的 id。标记写在语句上一行，只对它
/// 紧邻的下一行生效——中间隔了空行或其他注释就不再算数。属性行（`#[...]`）属于它下面
/// 那条语句，因此标记写在属性之上时仍算作对整条语句的标注
pub fn marked_ids<'a>(prod: &'a [(usize, &'a str)], i: usize, prefix: &str) -> Vec<&'a str> {
    let mut j = i;
    while j > 0 {
        j -= 1;
        if prod[j].1.trim_start().starts_with("#[") {
            continue;
        }
        return marker_ids(prod[j].1)
            .into_iter()
            .filter(|id| id.starts_with(prefix))
            .collect();
    }
    Vec::new()
}

/// 全部行，含 `#[cfg(test)] mod` 块；返回 (1-based 行号, 行文本)
pub fn all_lines(text: &str) -> Vec<(usize, &str)> {
    text.lines().enumerate().map(|(i, line)| (i + 1, line)).collect()
}

/// 按标记普查一类站点：返回已标记站点的 (文件, id) 与没有标记的命中行。
/// `lines_of` 决定扫哪个行集（生产行或全部行），`match_line` 收 (文件相对路径, trimmed 行)
pub fn census(
    sources: &[(String, String)],
    prefix: &str,
    lines_of: for<'a> fn(&'a str) -> Vec<(usize, &'a str)>,
    match_line: impl Fn(&str, &str) -> bool,
) -> (Vec<(String, String)>, Vec<String>) {
    let mut found = Vec::new();
    let mut unmarked = Vec::new();
    for (path, text) in sources {
        let lines = lines_of(text);
        for (i, (n, line)) in lines.iter().enumerate() {
            let trimmed = line.trim();
            if !match_line(path, trimmed) {
                continue;
            }
            let ids = marked_ids(&lines, i, prefix);
            if ids.is_empty() {
                unmarked.push(format!("{path}:{n}: {trimmed}"));
                continue;
            }
            for id in ids {
                found.push((path.clone(), id.to_string()));
            }
        }
    }
    (found, unmarked)
}

/// 普查并与登记表比对：每个标记 id 恰好一条登记，每条登记恰好一个标记。
/// 计数按 (文件, id) 累加，同一个 id 标在两处同样失败
pub fn compare_sites(
    label: &str,
    sources: &[(String, String)],
    prefix: &str,
    lines_of: for<'a> fn(&'a str) -> Vec<(usize, &'a str)>,
    match_line: impl Fn(&str, &str) -> bool,
    registered: impl Iterator<Item = (String, &'static str)>,
) {
    let (found, mut unmarked) = census(sources, prefix, lines_of, match_line);
    unmarked.sort();
    assert!(
        unmarked.is_empty(),
        "{label}：这些行没有 `{MARKER} <{prefix}id>` 标记（写在紧邻的上一行）——先登记再标记：\n{}",
        unmarked.join("\n")
    );
    let mut actual: BTreeMap<(String, String), usize> = BTreeMap::new();
    for site in found {
        *actual.entry(site).or_insert(0) += 1;
    }
    let mut want: BTreeMap<(String, String), usize> = BTreeMap::new();
    for (file, id) in registered {
        *want.entry((file, id.to_string())).or_insert(0) += 1;
    }
    assert_eq!(
        actual, want,
        "{label} 与登记表不一致，先复核再改登记：每个标记 id 需要恰好一条登记，每条登记需要恰好一个标记"
    );
}

pub fn pos(text: &str, needle: &str, what: &str) -> usize {
    text.find(needle)
        .unwrap_or_else(|| panic!("{what}: expected marker {needle:?} is gone"))
}

pub fn assert_order(text: &str, needles: &[&str], what: &str) {
    let mut at = 0usize;
    for needle in needles {
        let p = text[at..]
            .find(needle)
            .unwrap_or_else(|| panic!("{what}: {needle:?} is missing after byte {at}"));
        at += p + needle.len();
    }
}

pub fn run(args: &[&str]) -> (i32, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_DiskEdit"))
        .args(args)
        .output()
        .expect("spawn DiskEdit");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

pub fn tests_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests")
}

/// 把整份源码里的注释与字符串/字符/原始字符串字面量内部置成空白，分隔符保留；
/// 换行一并保留，行号不漂移。
///
/// 字符字面量与生命周期标记按「引号后是否有收盘单引号」区分：`'a'`/`'\n'` 整段抹掉，
/// `'a`（生命周期）原样保留——否则 `<'a>` 会把该行余下内容一并当成字符字面量抹掉。
/// 块注释按 Rust 规则可嵌套，`r#"... "#`、`br#"..."#` 的井号数按开盘值配对收盘。
pub fn code_only(text: &str) -> String {
    let b = text.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'r' | b'b' => {
                let Some((quote, hashes)) = raw_string_open(b, i) else {
                    out.push(b[i]);
                    i += 1;
                    continue;
                };
                let mut j = quote + 1;
                while j < b.len() {
                    if b[j] == b'"' {
                        let close = j + 1 + hashes;
                        if close <= b.len() && b[j + 1..close].iter().all(|&c| c == b'#') {
                            j = close;
                            break;
                        }
                    }
                    j += 1;
                }
                for &byte in &b[i..j] {
                    out.push(if byte == b'\n' { b'\n' } else { b' ' });
                }
                i = j;
            }
            b'/' if b.get(i + 1) == Some(&b'/') => {
                while i < b.len() && b[i] != b'\n' {
                    out.push(b' ');
                    i += 1;
                }
            }
            b'/' if b.get(i + 1) == Some(&b'*') => {
                let mut depth = 0usize;
                while i < b.len() {
                    if b[i] == b'/' && b.get(i + 1) == Some(&b'*') {
                        depth += 1;
                        out.extend_from_slice(b"  ");
                        i += 2;
                    } else if b[i] == b'*' && b.get(i + 1) == Some(&b'/') {
                        depth -= 1;
                        out.extend_from_slice(b"  ");
                        i += 2;
                        if depth == 0 {
                            break;
                        }
                    } else {
                        out.push(if b[i] == b'\n' { b'\n' } else { b' ' });
                        i += 1;
                    }
                }
            }
            b'"' => {
                out.push(b' ');
                i += 1;
                while i < b.len() {
                    if b[i] == b'\\' {
                        out.push(b' ');
                        i += 1;
                        if i < b.len() {
                            out.push(if b[i] == b'\n' { b'\n' } else { b' ' });
                            i += 1;
                        }
                    } else if b[i] == b'"' {
                        out.push(b' ');
                        i += 1;
                        break;
                    } else {
                        out.push(if b[i] == b'\n' { b'\n' } else { b' ' });
                        i += 1;
                    }
                }
            }
            b'\'' => {
                let after_quote = b.get(i + 1).copied();
                let width = match (after_quote, b.get(i + 2), b.get(i + 3)) {
                    (Some(c), Some(&b'\''), _) if c != b'\\' => Some(3),
                    (Some(b'\\'), _, Some(&b'\'')) => Some(4),
                    _ => None,
                };
                match width {
                    Some(n) => {
                        out.resize(out.len() + n, b' ');
                        i += n;
                    }
                    None if after_quote == Some(b'\\') => {
                        let mut j = i + 1;
                        while j < b.len() && b[j] != b'\'' {
                            j += 1;
                        }
                        let end = (j + 1).min(b.len());
                        out.resize(out.len() + (end - i), b' ');
                        i = end;
                    }
                    None => {
                        out.push(b[i]);
                        i += 1;
                    }
                }
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8(out).expect("blanking keeps UTF-8 byte boundaries")
}

/// 原始字符串的起点：`r"…"`、`r#"…"#`、`br#"…"#`。返回开引号下标与井号个数
fn raw_string_open(b: &[u8], i: usize) -> Option<(usize, usize)> {
    let mut j = i;
    if b[j] == b'b' {
        j += 1;
    }
    if b.get(j) != Some(&b'r') {
        return None;
    }
    j += 1;
    let hashes = b[j..].iter().take_while(|&&c| c == b'#').count();
    (b.get(j + hashes) == Some(&b'"')).then_some((j + hashes, hashes))
}

pub fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// 把代码行切成 token：标识符/字面量/多字符运算符/单字符符号
pub fn tokenize(code: &str) -> Vec<String> {
    let mut toks = Vec::new();
    let bytes = code.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        let b = bytes[i];
        if b.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        if is_word_byte(b) {
            let mut j = i;
            while j < bytes.len() && is_word_byte(bytes[j]) {
                j += 1;
            }
            toks.push(code[i..j].to_string());
            i = j;
            continue;
        }
        if b.is_ascii_digit() {
            let mut j = i;
            while j < bytes.len() && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_' || bytes[j] == b'.') {
                j += 1;
            }
            toks.push(code[i..j].to_string());
            i = j;
            continue;
        }
        let two = &code[i..i + 2.min(bytes.len() - i)];
        if let Some(op) = ["->", "=>", "==", "!=", "<=", ">=", "&&", "||", "+=", "-=", "*="]
            .iter()
            .find(|o| two == **o)
        {
            toks.push((*op).to_string());
            i += op.len();
            continue;
        }
        toks.push((b as char).to_string());
        i += 1;
    }
    toks
}

/// 测试夹具文件（`tests.rs` 模块文件与 `tests/` 目录）不参与生产普查
pub fn is_fixture_file(path: &str) -> bool {
    path.ends_with("tests.rs") || path.contains("/tests/")
}

/// 整行的锚点内容：去掉 `//` 起的注释后缀并去首尾空白（字符串字面量原样保留）
pub fn anchor_of(raw: &str) -> String {
    match raw.find("//") {
        Some(idx) => raw[..idx].trim(),
        None => raw.trim(),
    }
    .to_string()
}

/// 一条普查违规：文件、行号（0 表示挂在登记条目上、不对应现存源码行）、类别、内容
#[derive(Debug, Clone, PartialEq)]
pub struct Violation {
    pub file: String,
    pub line_no: usize,
    pub category: &'static str,
    pub content: String,
}

impl Violation {
    pub fn render(&self) -> String {
        if self.line_no == 0 {
            format!("{}: [{}] {}", self.file, self.category, self.content)
        } else {
            format!("{}:{}: [{}] {}", self.file, self.line_no, self.category, self.content)
        }
    }
}

/// 一个命中站点：现场行号只用于报告，匹配走内容与出现次序
#[derive(Debug, Clone)]
pub struct Site {
    pub line_no: usize,
    pub content: String,
    pub occurrence: usize,
}

/// 给一串 (行号, 内容) 按内容编号（同一内容第 k 次出现记 k）
pub fn number_occurrences(items: impl Iterator<Item = (usize, String)>) -> Vec<Site> {
    let mut seen: BTreeMap<String, usize> = BTreeMap::new();
    items
        .map(|(line_no, content)| {
            let k = seen.entry(content.clone()).or_insert(0);
            *k += 1;
            Site { line_no, content, occurrence: *k }
        })
        .collect()
}

/// `|` 折行时把 or-pattern 接成一条逻辑行（行尾悬着单个 `|`，或下一个非注释行以单个 `|`
/// 开头），夹在两者之间的整行注释是 trivia，不并入拼接文本。`||` 是逻辑或，不参与拼接。
/// 返回 (首行在生产行数组中的下标, 拼接文本)
pub fn join_or_patterns(text: &str) -> Vec<(usize, String)> {
    let prod = production_lines(text);
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

/// 源码的逻辑行与起始行号（1 起）：行尾 `\` 让字符串续到下一行，下一行的行首空白
/// 不进入字面量。注释行不参与拼接
pub fn join_string_continuations(text: &str) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    let mut buf = String::new();
    let mut start = 1;
    let mut continuing = false;
    for (no, raw) in text.lines().enumerate() {
        let line = if continuing { raw.trim_start() } else { raw };
        if !continuing {
            start = no + 1;
        }
        if let Some(head) = line.trim_end().strip_suffix('\\').filter(|_| is_string_continuation(line)) {
            buf.push_str(head);
            continuing = true;
        } else {
            buf.push_str(line);
            out.push((start, std::mem::take(&mut buf)));
            continuing = false;
        }
    }
    if continuing || !buf.is_empty() {
        out.push((start, buf));
    }
    out
}

/// 行尾 `\` 是不是字符串内的续行：该行在此之前有一个未闭合的字符串字面量。
/// 注释行的行尾 `\` 与字符/转义无关
fn is_string_continuation(line: &str) -> bool {
    let body = match line.trim_end().strip_suffix('\\') {
        Some(body) => body,
        None => return false,
    };
    if body.trim_start().starts_with("//") {
        return false;
    }
    let mut quotes = 0usize;
    let mut escaped = false;
    for c in body.chars() {
        if escaped {
            escaped = false;
            continue;
        }
        match c {
            '\\' => escaped = true,
            '"' => quotes += 1,
            _ => {}
        }
    }
    quotes % 2 == 1
}
