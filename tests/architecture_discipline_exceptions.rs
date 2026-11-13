//! 纪律例外（`#[allow(...)]` 与有意丢弃的 `let _ =`）的登记表守卫。
//!
//! 两类写法都会让编译器闭嘴：放行 lint，或丢弃一个 `#[must_use]` 的返回值。它们可以
//! 是对的，但不能是无声的——本文件把 `src/` 下每一处登记成具名条目（文件 + 整行原文 +
//! 出现次数 + 为什么），多一处、少一处、改一个字都会让断言失败。
//!
//! 扫描范围是 `src/` 下全部 `.rs`，包含 `tests/` 子目录里的测试辅助：那里的丢弃也是
//! 代码，同样要有理由。行原文参与比对，因此改写一行等于改登记表。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

struct Rule {
    file: &'static str,
    line: &'static str,
    count: usize,
    why: &'static str,
}

const ALLOW_RULES: &[Rule] = &[
    Rule {
        file: "cmd/layout.rs",
        line: "#[allow(clippy::too_many_arguments)]",
        count: 3,
        why: "布局写入口与执行器共用同一组参数，拆参数对象只是搬运",
    },
    Rule {
        file: "cmd/resize.rs",
        line: "#[allow(clippy::too_many_arguments)]",
        count: 1,
        why: "同上：扩容入口的参数直接来自请求与计划",
    },
    Rule {
        file: "cmd/resize/finish.rs",
        line: "#[allow(clippy::too_many_arguments)]",
        count: 2,
        why: "同上：收尾步骤沿用同一组参数",
    },
    Rule {
        file: "movepart/shift.rs",
        line: "#[allow(clippy::too_many_arguments)] // 与 apply 同一组入参",
        count: 1,
        why: "搬移计划的构造与 apply 共用入参",
    },
    Rule {
        file: "movepart/tests/support.rs",
        line: "#[allow(clippy::too_many_arguments)] // 与生产入口同一组入参，测试辅助不另造参数对象",
        count: 1,
        why: "测试辅助镜像生产入口的签名",
    },
    Rule {
        file: "fsops/env.rs",
        line: "#[allow(clippy::let_underscore_must_use)] // 有意忽略：主错误已上报",
        count: 1,
        why: "外部工具失败的收尸动作，主错误已经返回",
    },
    Rule {
        file: "support.rs",
        line: "#[allow(clippy::let_underscore_must_use)] // 有意忽略：诊断设施失败不改变业务结论",
        count: 1,
        why: "诊断行写不出去不影响命令结论",
    },
    Rule {
        file: "testdir.rs",
        line: "#[allow(clippy::let_underscore_must_use)] // 有意忽略：Drop 没有返回通道",
        count: 1,
        why: "析构函数没有返回通道",
    },
    Rule {
        file: "dev/target.rs",
        line: "#[allow(clippy::let_underscore_must_use)]",
        count: 1,
        why: "清理目录失败不改变元数据结论",
    },
    Rule {
        file: "dev/target.rs",
        line: "#[allow(clippy::let_underscore_must_use)] // 有意忽略：失败只使元数据回退",
        count: 1,
        why: "同步失败只让元数据回退到未落盘",
    },
    Rule {
        file: "recovery/evidence.rs",
        line: "#[allow(clippy::large_enum_variant)]",
        count: 1,
        why: "证据枚举按值传递，装箱会改变调用点的所有权形状",
    },
    Rule {
        file: "recovery/journal.rs",
        line: "#[allow(clippy::large_enum_variant)]",
        count: 1,
        why: "同上：记录枚举的变体大小不影响热路径",
    },
];

const DISCARD_RULES: &[Rule] = &[
    Rule {
        file: "fsops/env.rs",
        line: "let _ = child.kill();",
        count: 1,
        why: "外部工具已失败，收尸动作的返回值无意义",
    },
    Rule {
        file: "fsops/env.rs",
        line: "let _ = child.wait();",
        count: 1,
        why: "同上：等待已被杀死的子进程",
    },
    Rule {
        file: "support.rs",
        line: "let _ = writeln!(f, \"{line}\");",
        count: 1,
        why: "诊断输出失败不改变结论",
    },
    Rule {
        file: "support.rs",
        line: "let _ = src;",
        count: 1,
        why: "平台条件编译下未用到的入参",
    },
    Rule {
        file: "testdir.rs",
        line: "let _ = remove_all(path);",
        count: 1,
        why: "Drop 中的清理失败没有返回通道",
    },
    Rule {
        file: "fsops/scope.rs",
        line: "let _ = policy;",
        count: 1,
        why: "平台条件编译下未用到的入参",
    },
    Rule {
        file: "fsops/scope.rs",
        line: "let _ = (src, part, want_start);",
        count: 1,
        why: "同上：整组入参在某平台分支不可用",
    },
    Rule {
        file: "fsops/scope.rs",
        line: "let _ = Command::new(\"dmsetup\").args([\"remove\", &dm]).status();",
        count: 1,
        why: "拆除映射的失败路径没有后续动作",
    },
    Rule {
        file: "targetlock.rs",
        line: "let _ = expected;",
        count: 1,
        why: "平台条件编译下未用到的期望值",
    },
    Rule {
        file: "targetlock.rs",
        line: "let _ = std::fs::remove_file(&lock_path);",
        count: 2,
        why: "释放锁文件失败不改变持锁结论",
    },
    Rule {
        file: "targetlock.rs",
        line: "let _ = std::fs::remove_file(&alias);",
        count: 1,
        why: "同上：清理别名锁文件",
    },
    Rule {
        file: "dev/tests.rs",
        line: "let _ = std::process::Command::new(\"losetup\").arg(\"-d\").arg(&self.dev).output();",
        count: 1,
        why: "测试夹具拆除 loop 设备，失败由回收站兜底",
    },
    Rule {
        file: "dev/tests.rs",
        line: "let _ = std::process::Command::new(\"losetup\").arg(\"-d\").arg(&self.0).output();",
        count: 1,
        why: "同上：另一种持有者的拆除",
    },
    Rule {
        file: "dev/target.rs",
        line: "let _ = std::fs::remove_dir(dir);",
        count: 1,
        why: "清理失败不改变元数据结论",
    },
    Rule {
        file: "dev/target.rs",
        line: "let _ = d.sync_all();",
        count: 1,
        why: "同步失败只让元数据回退",
    },
    Rule {
        file: "cmd/resize/finish.rs",
        line: "let _ = (is_block, lv_path, old_bytes, wsrc, online_lock, online_facts);",
        count: 1,
        why: "平台条件编译下整组入参不可用",
    },
    Rule {
        file: "dev/source.rs",
        line: "let _ = opened;",
        count: 1,
        why: "平台条件编译下未用到的句柄",
    },
    Rule {
        file: "dev/source.rs",
        line: "let _ = (off, buf);",
        count: 1,
        why: "同上：写入参数在某平台分支不可用",
    },
    Rule {
        file: "dev/occupancy.rs",
        line: "let _ = src;",
        count: 1,
        why: "非 Linux 分支没有占用探测",
    },
    Rule {
        file: "dev/identity.rs",
        line: "let _ = path;",
        count: 1,
        why: "平台条件编译下未用到的路径",
    },
    Rule {
        file: "dev/host_repo.rs",
        line: "let _ = owner;",
        count: 1,
        why: "镜像侧不取目标文件的所有者",
    },
    Rule {
        file: "table/tests/fuzz.rs",
        line: "let _ = std::fs::remove_file(&path);",
        count: 2,
        why: "模糊测试用例的临时文件清理",
    },
    Rule {
        file: "movepart/faults.rs",
        line: "$( let _ = $arg; )*",
        count: 1,
        why: "故障注入宏里逐个吞掉未使用的入参",
    },
];

/// `src/` 下全部 `.rs`，包含 `tests/` 子目录：丢弃与放行在这两类文件里同样要有理由
fn all_sources() -> Vec<(String, String)> {
    fn walk(dir: &Path, root: &Path, out: &mut Vec<(String, String)>) {
        let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)
            .unwrap_or_else(|e| panic!("cannot list {}: {e}", dir.display()))
            .map(|e| e.expect("read a directory entry").path())
            .collect();
        entries.sort();
        for path in entries {
            if path.is_dir() {
                walk(&path, root, out);
            } else if path.extension().is_some_and(|e| e == "rs") {
                let rel = path
                    .strip_prefix(root)
                    .expect("walk started at src/")
                    .to_string_lossy()
                    .replace('\\', "/");
                let text = std::fs::read_to_string(&path)
                    .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
                out.push((rel, text));
            }
        }
    }
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut out = Vec::new();
    walk(&root, &root, &mut out);
    out
}

fn actual(predicate: impl Fn(&str) -> bool) -> BTreeMap<(String, String), usize> {
    let mut found: BTreeMap<(String, String), usize> = BTreeMap::new();
    for (rel, text) in all_sources() {
        for line in text.lines() {
            let trimmed = line.trim();
            if predicate(trimmed) {
                *found.entry((rel.clone(), trimmed.to_string())).or_insert(0) += 1;
            }
        }
    }
    found
}

fn registered(rules: &[Rule]) -> BTreeMap<(String, String), usize> {
    let mut found: BTreeMap<(String, String), usize> = BTreeMap::new();
    for rule in rules {
        found.insert((rule.file.to_string(), rule.line.to_string()), rule.count);
    }
    found
}

fn report(left: &BTreeMap<(String, String), usize>, right: &BTreeMap<(String, String), usize>) -> String {
    let mut lines = Vec::new();
    for (key, count) in left {
        match right.get(key) {
            Some(other) if other == count => {}
            Some(other) => lines.push(format!("{}: {} found {}x, registered {}x", key.0, key.1, count, other)),
            None => lines.push(format!("{}: {} found {}x, not registered", key.0, key.1, count)),
        }
    }
    for key in right.keys() {
        if !left.contains_key(key) {
            lines.push(format!("{}: {} registered but not found", key.0, key.1));
        }
    }
    lines.join("\n")
}

fn describe(rules: &[Rule]) -> String {
    rules.iter().map(|r| format!("{}: {} — {}", r.file, r.line, r.why)).collect::<Vec<_>>().join("\n")
}

#[test]
fn every_allow_is_a_registered_exception() {
    let found = actual(|line| line.starts_with("#[allow("));
    let expected = registered(ALLOW_RULES);
    assert_eq!(
        found, expected,
        "an #[allow] appeared, moved or changed without being registered — add a named rule \
         with its reason, or remove the exception:\n{}\nregistered exceptions:\n{}",
        report(&found, &expected),
        describe(ALLOW_RULES)
    );
}

#[test]
fn every_intentional_discard_is_a_registered_exception() {
    let found = actual(|line| line.starts_with("let _ =") || line == "$( let _ = $arg; )*");
    let expected = registered(DISCARD_RULES);
    assert_eq!(
        found, expected,
        "an intentional `let _ =` discard appeared, moved or changed without being registered — \
         add a named rule with its reason, or handle the value:\n{}\nregistered exceptions:\n{}",
        report(&found, &expected),
        describe(DISCARD_RULES)
    );
}
