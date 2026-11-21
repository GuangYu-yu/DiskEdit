//! 「外部工具只有一个调用面」的结构守卫。
//!
//! 工具名、固定开关与读写类别收敛在 `fsops/ext.rs`；其余生产代码按操作调用那里的
//! 语义函数，不直接拼工具名。底层执行仍只有 `fsops/env.rs` 一处 spawn。

mod guards;

use guards::{production_lines, rust_sources};

/// 执行外部工具的入口形态。只允许出现在 ext（唯一调用面）与 env（唯一执行点）。
/// `Command::new` 一并算入口：绕过注册表直接 spawn 也长这个样子
const TOOL_ENTRY: &[&str] = &[
    "run_read(",
    "run_write(",
    "run_with(",
    "run_write_input(",
    "tool_command(",
    "find_tool(",
    "require_tool(",
    "Command::new(",
];

const TOOL_LAYER: &[&str] = &["fsops/ext.rs", "fsops/env.rs"];

/// 载体的进程不是文件系统工具：它由 `exec` 自己拉起，工具注册表在这里不适用。
/// 清单按文件列入——载体里跑的 FS 工具仍要经 `fsops::ext`，那条路径在 guest 进程内
const CARRIER_SPAWN: &[&str] = &["exec/qemu.rs", "exec/remote.rs"];

#[test]
fn external_tool_entries_live_only_in_the_registry_and_the_executor() {
    let mut found = Vec::new();
    for (rel, text) in rust_sources() {
        if TOOL_LAYER.contains(&rel.as_str())
            || CARRIER_SPAWN.contains(&rel.as_str())
            || rel.ends_with("/tests.rs")
        {
            continue;
        }
        for (n, line) in production_lines(&text) {
            if TOOL_ENTRY.iter().any(|entry| line.contains(entry)) {
                found.push(format!("{rel}:{n}: {}", line.trim()));
            }
        }
    }
    assert!(
        found.is_empty(),
        "external tools must be started through `fsops::ext` (tool name, fixed flags and \
         read/write class live there); these lines bypass it:\n{}",
        found.join("\n")
    );
}

#[test]
fn the_registry_passes_no_literal_tool_name() {
    // ext 的包装方法把注册表变体本身（`self`）交给 env，env 再取 `name()`：
    // 出现字符串字面量就等于表与调用各写一份，改名时会漏掉一处
    let text = guards::source("fsops/ext.rs");
    for (n, line) in production_lines(&text) {
        for entry in [
            "run_read(",
            "run_write(",
            "run_write_input(",
            "run_with(",
            "tool_command(",
            "find_tool(",
            "require_tool(",
        ] {
            let Some(at) = line.find(entry) else { continue };
            let rest = line[at + entry.len()..].trim_start();
            assert!(
                rest.starts_with("self)") || rest.starts_with("self,"),
                "fsops/ext.rs:{n}: something other than the registered variant reaches the executor: {line}"
            );
        }
    }
}
