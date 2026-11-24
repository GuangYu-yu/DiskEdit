//! 用户可见文案的 ASCII 守卫。
//!
//! 判据只有一条：凡能进入 stdout/stderr/帮助输出的字符串字面量必须是 ASCII。
//! 注释与测试内部字符串不跨进程边界，不在守卫范围；`docsgen` 产出的是文档，
//! 中英双语各自成立，同样不在范围。
//!
//! 范围取“输出构造路径”：CLI、报告、判定、观测、执行、载体与平台端口。
//! 运行时再对帮助、用法错误与一条真实拒侧输出的字节做同一断言。

use std::path::{Path, PathBuf};
use std::process::Command;

/// 输出构造路径的源文件根目录。
const ROOTS: &[&str] = &[
    "src/app/cli",
    "src/app/report",
    "src/app/decide",
    "src/app/observe",
    "src/app/execute",
    "src/app/carrier",
    "src/platform",
];

#[test]
fn user_visible_literals_in_output_paths_are_ascii() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let mut checked = 0usize;
    for root_dir in ROOTS {
        let mut files = Vec::new();
        collect(&root.join(root_dir), &mut files);
        assert!(!files.is_empty(), "{root_dir} 下没有源文件");
        for file in files {
            let source = std::fs::read_to_string(&file).expect("读取源文件");
            for literal in literals(&source) {
                checked += 1;
                assert!(
                    literal.text.is_ascii(),
                    "{}:{} 的字符串不是 ASCII（会被用户看到）：{:?}",
                    file.display(),
                    literal.line,
                    literal.text
                );
            }
        }
    }
    assert!(checked > 100, "扫描到的字面量过少，守卫范围可能写错");
}

#[test]
fn help_version_and_refusals_stay_ascii() {
    let binary = env!("CARGO_BIN_EXE_diskedit");
    for args in [
        vec!["--help"],
        vec!["--version"],
        vec!["unknown-command"],
        vec!["info"],
        vec!["help", "set"],
        vec!["help", "nosuch"],
        vec!["add", "/dev/null", "16M", "--yes"],
    ] {
        let output = Command::new(binary)
            .args(&args)
            .output()
            .expect("启动被测二进制");
        for (stream, bytes) in [("stdout", &output.stdout), ("stderr", &output.stderr)] {
            assert!(
                bytes.is_ascii(),
                "{:?} 的 {stream} 含非 ASCII 字节：{}",
                args,
                String::from_utf8_lossy(bytes)
            );
        }
    }
}

fn collect(dir: &Path, out: &mut Vec<PathBuf>) {
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .unwrap_or_else(|error| panic!("列出 {} 失败: {error}", dir.display()))
        .map(|entry| entry.expect("目录项").path())
        .collect();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            collect(&path, out);
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            out.push(path);
        }
    }
}

struct Literal {
    line: usize,
    text: String,
}

/// 提取普通字符串与原始字符串字面量；跳过注释与 `#[cfg(test)]` 之后的测试模块。
fn literals(source: &str) -> Vec<Literal> {
    let source = match source.find("#[cfg(test)]") {
        Some(at) => &source[..at],
        None => source,
    };
    let bytes = source.as_bytes();
    let mut out = Vec::new();
    let mut index = 0usize;
    let mut line = 1usize;
    while index < bytes.len() {
        match bytes[index] {
            b'\n' => {
                line += 1;
                index += 1;
            }
            b'/' if bytes.get(index + 1) == Some(&b'/') => {
                while index < bytes.len() && bytes[index] != b'\n' {
                    index += 1;
                }
            }
            b'/' if bytes.get(index + 1) == Some(&b'*') => {
                index += 2;
                while index < bytes.len()
                    && !(bytes[index] == b'*' && bytes.get(index + 1) == Some(&b'/'))
                {
                    if bytes[index] == b'\n' {
                        line += 1;
                    }
                    index += 1;
                }
                index += 2;
            }
            b'"' => {
                let (text, next) = take_string(source, index, line);
                out.push(Literal { line, text });
                line += source[index..next].matches('\n').count();
                index = next;
            }
            b'r' if is_raw_start(bytes, index) => {
                let (text, next) = take_raw_string(source, index, line);
                out.push(Literal { line, text });
                line += source[index..next].matches('\n').count();
                index = next;
            }
            _ => index += 1,
        }
    }
    out
}

/// `r"…"` 或 `r#…"…"#` 的起点；前一字节不能是标识符字符。
fn is_raw_start(bytes: &[u8], index: usize) -> bool {
    if let Some(previous) = index.checked_sub(1).map(|at| bytes[at]) {
        if previous.is_ascii_alphanumeric() || previous == b'_' {
            return false;
        }
    }
    let mut at = index + 1;
    while bytes.get(at) == Some(&b'#') {
        at += 1;
    }
    bytes.get(at) == Some(&b'"')
}

fn take_string(source: &str, start: usize, _line: usize) -> (String, usize) {
    let bytes = source.as_bytes();
    let mut index = start + 1;
    let mut text = String::new();
    while index < bytes.len() {
        match bytes[index] {
            b'\\' => {
                text.push_str(&source[index..(index + 2).min(source.len())]);
                index += 2;
            }
            b'"' => {
                index += 1;
                break;
            }
            _ => {
                let ch = source[index..].chars().next().expect("字符");
                text.push(ch);
                index += ch.len_utf8();
            }
        }
    }
    (text, index)
}

fn take_raw_string(source: &str, start: usize, _line: usize) -> (String, usize) {
    let bytes = source.as_bytes();
    let mut at = start + 1;
    let mut hashes = 0usize;
    while bytes.get(at) == Some(&b'#') {
        hashes += 1;
        at += 1;
    }
    at += 1;
    let body_start = at;
    let closing = format!("\"{}", "#".repeat(hashes));
    let end = source[body_start..]
        .find(&closing)
        .map(|offset| body_start + offset)
        .unwrap_or(source.len());
    (
        source[body_start..end].to_owned(),
        (end + closing.len()).min(source.len()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn comments_and_test_modules_are_not_scanned() {
        let source = "// 中文注释\nfn f() { let a = \"ok\"; }\n#[cfg(test)]\nmod tests { const S: &str = \"中文\"; }";
        let found = literals(source);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].text, "ok");
    }

    #[test]
    fn raw_strings_are_scanned() {
        let found = literals("let a = r\"raw\"; let b = r#\"x\"#;");
        assert_eq!(found.len(), 2);
        assert_eq!(found[1].text, "x");
    }
}
