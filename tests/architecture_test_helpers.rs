//! 守卫 target 的原语只能来自 `tests/guards`：剥离注释/字面量、分词、夹具判定、锚点
//! 提取与逻辑行拼接若在各 `architecture_*.rs` 里就地重写，判定口径会各自漂移——
//! 同一段源码在两个普查里得出不同的命中集合，登记表也就失去可比性。
//!
//! 本 target 只做一件事：扫描 `tests/architecture_*.rs`，禁止再声明这些共享原语的
//! 本地副本。测试函数名允许以原语名作前缀（如 `code_only_blanks_raw_strings` 是
//! 对共享实现的断言），因此匹配按标识符边界判定，不按子串。

mod guards;

use guards::tests_root;

/// 共享原语的名字。新增共享原语时同步登记在这里
const SHARED: &[&str] = &[
    "code_only",
    "is_word_byte",
    "tokenize",
    "is_fixture_file",
    "anchor_of",
    "logical_lines",
];

fn declares_fn(line: &str, name: &str) -> bool {
    let t = line.trim_start();
    let Some(rest) = t.strip_prefix("fn ") else {
        return false;
    };
    let Some(after) = rest.strip_prefix(name) else {
        return false;
    };
    !after.starts_with(|c: char| c.is_alphanumeric() || c == '_')
}

#[test]
fn scanner_primitives_are_not_redeclared_in_architecture_targets() {
    let dir = tests_root();
    let mut offenders: Vec<String> = Vec::new();
    let mut entries: Vec<_> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("cannot list {}: {e}", dir.display()))
        .map(|e| e.expect("read a directory entry").path())
        .collect();
    entries.sort();
    for path in entries {
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if !name.starts_with("architecture_")
            || !name.ends_with(".rs")
            || name == "architecture_test_helpers.rs"
        {
            continue;
        }
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
        for (no, line) in text.lines().enumerate() {
            for shared in SHARED {
                if declares_fn(line, shared) {
                    offenders.push(format!("{name}:{}: {}", no + 1, line.trim()));
                }
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "剥离/分词/夹具判定/锚点等原语只在 tests/guards/mod.rs 定义，禁止就地重写:\n{}",
        offenders.join("\n")
    );
}
