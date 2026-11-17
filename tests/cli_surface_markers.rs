//! `cli-surface: refused` 标记与拒绝断言的可执行绑定（双向）。
//!
//! `tests/wsl/check_cli_surface.sh` 用二进制才知道的命令面做双向判定；本文件把它
//! 搬到任何平台都能跑的一半：
//!
//! - 正向：每一条标记的下文必须真的断言退出码 2。标记退化成「免检注释」时先失败。
//! - 反向：每一处落在命令面外的调用（未知命令、该命令未声明的旗标），如果所在测试
//!   断言退出 2，就必须带标记。新增用例时忘了标记，这里会红。
//!
//! 「命令面」直接读 `src/cli.rs` 的 `CommandDef` 表，不另维护一份：遗漏与漂移会先
//! 在这里暴露。全局旗标（`--help` / `-h` / `--version` / `-V` / `--`）对任何命令都算
//! 面内，与命令层解析一致。

use std::path::{Path, PathBuf};

fn tests_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests")
}

fn rs_files() -> Vec<PathBuf> {
    let root = tests_root();
    let mut out = Vec::new();
    for dir in [root.clone(), root.join("cli")] {
        let mut here: Vec<PathBuf> = std::fs::read_dir(&dir)
            .unwrap_or_else(|e| panic!("cannot list {}: {e}", dir.display()))
            .map(|e| e.expect("read a directory entry").path())
            .filter(|p| p.extension().is_some_and(|e| e == "rs"))
            .collect();
        here.sort();
        out.extend(here);
    }
    out
}

fn sh_files() -> Vec<PathBuf> {
    let dir = tests_root().join("wsl");
    let mut out: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("cannot list {}: {e}", dir.display()))
        .map(|e| e.expect("read a directory entry").path())
        .filter(|p| p.extension().is_some_and(|e| e == "sh"))
        .collect();
    out.sort();
    out
}

const MARKER: &str = "cli-surface: refused";

/// 标记必须在代码行的行尾注释里；文档、注释整行、以及字符串字面量里提到这个词都不算
fn is_marker(line: &str, sigil: &str) -> bool {
    let t = line.trim_start();
    if t.starts_with("//") || t.starts_with('#') {
        return false;
    }
    match line.find(MARKER) {
        Some(i) => line[..i].contains(sigil),
        None => false,
    }
}

#[test]
fn rust_markers_assert_exit_two() {
    let mut markers = 0;
    for file in rs_files() {
        if file.ends_with("cli_surface_markers.rs") {
            continue;
        }
        let text = std::fs::read_to_string(&file).expect("read a rust test");
        let lines: Vec<&str> = text.lines().collect();
        for (i, line) in lines.iter().enumerate() {
            if !is_marker(line, "//") {
                continue;
            }
            markers += 1;
            let window = &lines[i + 1..lines.len().min(i + 7)];
            let asserted = window
                .iter()
                .any(|l| l.contains("assert_eq!") && (l.contains(", 2,") || l.contains(", 2)")));
            assert!(
                asserted,
                "{}:{} 的 `{MARKER}` 之后 6 行内没有断言退出 2",
                file.display(),
                i + 1
            );
        }
    }
    assert!(markers > 0, "找不到任何 `{MARKER}` 标记，绑定失去对象");
}

#[test]
fn shell_markers_assert_exit_two() {
    let mut markers = 0;
    for file in sh_files() {
        let text = std::fs::read_to_string(&file).expect("read a shell test");
        let lines: Vec<&str> = text.lines().collect();
        for (i, line) in lines.iter().enumerate() {
            if !is_marker(line, "#") {
                continue;
            }
            markers += 1;
            let mut asserted = false;
            for next in &lines[i..lines.len().min(i + 5)] {
                if next.contains("$B") && !next.contains(MARKER) {
                    break;
                }
                if next.contains("exp $? 2")
                    || next.contains("chk \"$?\" \"2\"")
                    || next.contains("[ \"$E\" = \"2\" ]")
                {
                    asserted = true;
                    break;
                }
            }
            assert!(
                asserted,
                "{}:{} 的 `{MARKER}` 之后没有断言退出 2",
                file.display(),
                i + 1
            );
        }
    }
    assert!(markers > 0, "找不到任何 `{MARKER}` 标记，绑定失去对象");
}

/// 命令面：命令名 → 该命令声明的长选项。读 `src/cli.rs` 的 `CommandDef` 表，
/// 不另维护一份
fn command_surface() -> Vec<(String, Vec<String>)> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/cli.rs");
    let text = std::fs::read_to_string(&path).expect("read src/cli.rs");
    let mut out = Vec::new();
    let mut rest: &str = &text;
    while let Some(pos) = rest.find("CommandDef {") {
        rest = &rest[pos + "CommandDef {".len()..];
        let end = rest.find("\n    },").map(|i| i + 1).unwrap_or(rest.len());
        let block = &rest[..end];
        rest = &rest[end..];
        let Some(name) = between(block, "name: \"", "\"") else { continue };
        out.push((name.to_string(), declared_options(block)));
    }
    out
}

fn between<'a>(text: &'a str, open: &str, close: &str) -> Option<&'a str> {
    let start = text.find(open)? + open.len();
    let end = text[start..].find(close)?;
    Some(&text[start..start + end])
}

/// 一段 `CommandDef` 里声明的长选项：`opt("--a")` 与 `opt_desc("--b", "…")`。
/// 选项写在形态（`variant(...)`）与 `common_options` 里，两处都是同一张表的一部分；
/// 多行数组同样适用
fn declared_options(block: &str) -> Vec<String> {
    let mut out = Vec::new();
    for prefix in ["opt(\"", "opt_desc(\""] {
        let mut rest = block;
        while let Some(pos) = rest.find(prefix) {
            rest = &rest[pos + prefix.len()..];
            let Some(close) = rest.find('"') else { break };
            out.push(rest[..close].to_string());
            rest = &rest[close..];
        }
    }
    out
}

/// `run(&[...])` 里的字符串字面量、该调用的起止行号（0 基）
fn rust_calls(text: &str) -> Vec<(usize, usize, Vec<String>)> {
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(pos) = text[from..].find("run(&[") {
        let start = from + pos;
        from = start + 1;
        let args = start + "run(&[".len();
        let Some(end) = closing_bracket(text, args) else { continue };
        let lits = literals(&text[args..end]);
        if lits.is_empty() {
            continue;
        }
        let line_of = |offset: usize| text[..offset].bytes().filter(|b| *b == b'\n').count();
        out.push((line_of(start), line_of(end), lits));
    }
    out
}

/// 从 `[` 之后的位置起找配对的 `]`，字符串字面量里的括号不算
fn closing_bracket(text: &str, from: usize) -> Option<usize> {
    let mut depth = 1i32;
    let mut in_string = false;
    let mut escaped = false;
    for (offset, ch) in text[from..].char_indices() {
        if in_string {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }
        match ch {
            '"' => in_string = true,
            '[' => depth += 1,
            ']' => {
                depth -= 1;
                if depth == 0 {
                    return Some(from + offset);
                }
            }
            _ => {}
        }
    }
    None
}

fn literals(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(open) = rest.find('"') {
        rest = &rest[open + 1..];
        let mut chars = rest.char_indices();
        let mut end = None;
        while let Some((i, ch)) = chars.next() {
            if ch == '\\' {
                chars.next();
                continue;
            }
            if ch == '"' {
                end = Some(i);
                break;
            }
        }
        let Some(end) = end else { break };
        out.push(rest[..end].to_string());
        rest = &rest[end + 1..];
    }
    out
}

/// 调用里的命令名；两词命令（`records list`）合并。第一个字面量是旗标时（如
/// `--version`）不算命令调用
fn command_of(lits: &[String], known: &dyn Fn(&str) -> bool) -> Option<String> {
    let idx = lits.iter().position(|l| !l.starts_with('-'))?;
    let first = &lits[idx];
    if let Some(second) = lits.get(idx + 1) {
        let two = format!("{first} {second}");
        if known(&two) {
            return Some(two);
        }
    }
    Some(first.clone())
}

/// 这一次调用期待的结果是不是退出 2：取调用之后的第一条 `assert_eq!`，只看它。
/// 同一个测试函数里既有成功用例又有拒绝用例时，按调用逐个判，不按函数整体判
fn call_expects_two(lines: &[&str], start: usize, end: usize) -> bool {
    for line in &lines[start..lines.len().min(end + 5)] {
        let Some(pos) = line.find("assert_eq!") else { continue };
        let tail = &line[pos..];
        return tail.contains(", 2,") || tail.contains(", 2)");
    }
    false
}

/// 反向：落在命令面外、且所在测试断言退出 2 的调用必须带标记
#[test]
fn every_refusal_case_is_marked() {
    let surface = command_surface();
    // `help` / `?` 不在 CommandDef 表里：它们由命令层单独识别，属于命令面
    let known = |name: &str| matches!(name, "help" | "?") || surface.iter().any(|(n, _)| n == name);
    let options = |name: &str| -> Vec<String> {
        surface
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, o)| o.clone())
            .unwrap_or_default()
    };
    const GLOBAL: &[&str] = &["--help", "-h", "--version", "-V", "--"];
    let mut refusals = 0;
    for file in rs_files() {
        if file.ends_with("cli_surface_markers.rs") {
            continue;
        }
        let text = std::fs::read_to_string(&file).expect("read a rust test");
        let lines: Vec<&str> = text.lines().collect();
        for (line, end_line, lits) in rust_calls(&text) {
            let Some(cmd) = command_of(&lits, &known) else { continue };
            let declared = options(&cmd);
            let outside_command = !known(&cmd);
            let outside_flag = lits.iter().any(|l| {
                let flag = l.split('=').next().unwrap_or(l);
                flag.starts_with("--")
                    && !GLOBAL.contains(&flag)
                    && !declared.iter().any(|o| o == flag)
            });
            if !outside_command && !outside_flag {
                continue;
            }
            if !call_expects_two(&lines, line, end_line) {
                continue;
            }
            refusals += 1;
            let marked = lines[line..lines.len().min(line + 7)]
                .iter()
                .any(|l| is_marker(l, "//"));
            assert!(
                marked,
                "{}:{} 的调用落在命令面外（命令 `{cmd}`），所在测试断言退出 2，但没有 `{MARKER}` 标记",
                file.display(),
                line + 1
            );
        }
    }
    assert!(refusals > 0, "找不到任何落在命令面外的拒绝用例，反向校验失去对象");
}
