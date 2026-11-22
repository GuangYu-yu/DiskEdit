//! 子进程命令构造点的类型层封口普查（R13）。
//!
//! 生产代码拿到 `std::process::Command` 只有三种形态：`Command::new(..)`、
//! `Command::from(..)`、结构体字面量 `Command { .. }`。封口之后这些形态只允许出现在
//! 闸口模块 `fsops/env.rs`：媒介类型 `GateCommand` 在那里构造，字段私有，模块外
//! 既拿不到 `Command`，也没有 `Deref` / `as_command` / `From<Command>` 一类逃逸口。
//!
//! `#[cfg(test)]` 与 `#[cfg(all(test, ..))]` 段里的直写是测试夹具，不计入生产行；
//! 剥除方式复用 [`guards::production_lines`]，不另造一套解析。

mod guards;

use guards::{production_lines, rust_sources};

/// 闸口模块：唯一允许出现 Command 构造形态的生产文件
const GATE_FILE: &str = "fsops/env.rs";

/// 判断一行源码是否在获取一个 `std::process::Command`：构造函数、`From` 转换、
/// 结构体字面量三种形态都算
fn command_construction(line: &str) -> bool {
    if line.contains("Command::new(") || line.contains("Command::from(") {
        return true;
    }
    // 结构体字面量排除返回类型位置：`fn f() -> std::process::Command {` 里的 `{`
    // 开的是函数体，不是结构体字段表；返回类型在 `->` 之后，字面量只看它之前的部分
    let body = match line.split_once("->") {
        Some((before, _)) => before,
        None => line,
    };
    body.contains("Command {")
}

/// 普查一份源码：返回闸口模块之外、落在生产行上的构造形态命中
/// （1-based 行号, 行文本）。闸口模块内部的构造是封口本身，不计违规
fn violations<'a>(rel: &str, text: &'a str) -> Vec<(usize, &'a str)> {
    if rel == GATE_FILE {
        return Vec::new();
    }
    production_lines(text)
        .into_iter()
        .filter(|(_, line)| command_construction(line.trim()))
        .collect()
}

/// 整文件即测试夹具（`src/**/tests.rs`）：与 `guards` 里其余普查同一口径跳过
fn is_test_file(rel: &str) -> bool {
    rel.ends_with("tests.rs") || rel.contains("/tests/")
}

#[test]
fn production_command_construction_lives_only_in_the_gate() {
    let mut found = Vec::new();
    for (rel, text) in rust_sources() {
        if is_test_file(&rel) {
            continue;
        }
        for (n, line) in violations(&rel, &text) {
            found.push(format!("{rel}:{n}: {}", line.trim()));
        }
    }
    assert!(
        found.is_empty(),
        "a `std::process::Command` is constructed outside the fsops gate:\n{}",
        found.join("\n")
    );
}

/// 合成样本：闸口模块之外拿到 Command 的三种形态都必须被判红
const OUTSIDE_SAMPLE: &str = r#"
fn start() -> std::process::Command {
    std::process::Command::new("qemu-system-x86_64")
}

fn from_os() -> std::process::Command {
    std::process::Command::from("losetup")
}

fn literal() -> std::process::Command {
    std::process::Command { program: "sh".into(), ..Default::default() }
}
"#;

#[test]
fn synthetic_command_outside_the_gate_is_flagged() {
    let hits = violations("exec/qemu.rs", OUTSIDE_SAMPLE);
    assert_eq!(
        hits.len(),
        3,
        "every Command construction form outside the gate must be flagged: {hits:?}"
    );
}

/// 合成样本：闸口模块内的构造不计违规；模块外路径下，生产行那一处被抓住，
/// 两种测试段（`#[cfg(test)]` 与 `#[cfg(all(test, ..))]`）里的直写不误红
const GATE_AND_TEST_SAMPLE: &str = r#"
pub(crate) fn gate() -> std::process::Command {
    std::process::Command::new("losetup")
}

#[cfg(test)]
mod tests {
    #[test]
    fn fixture() {
        let _ = std::process::Command::new("sh").status();
    }
}

#[cfg(all(test, target_os = "linux"))]
mod linux_tests {
    #[test]
    fn fixture_linux() {
        let _ = std::process::Command::new("dmsetup").status();
    }
}
"#;

#[test]
fn synthetic_gate_construction_and_test_blocks_are_not_flagged() {
    let gate_hits = violations(GATE_FILE, GATE_AND_TEST_SAMPLE);
    assert!(
        gate_hits.is_empty(),
        "construction inside the gate module must not be flagged: {gate_hits:?}"
    );

    let outside = violations("fsops/scope.rs", GATE_AND_TEST_SAMPLE);
    assert_eq!(
        outside.len(),
        1,
        "only the production-line construction may be flagged; test-block fixtures are stripped: {outside:?}"
    );
    assert!(outside[0].1.contains("losetup"));
}
