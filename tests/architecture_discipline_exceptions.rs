//! 纪律例外（`#[allow(...)]` 与有意丢弃的 `let _ =`）的登记表守卫。
//!
//! 两类写法都会让编译器闭嘴：放行 lint，或丢弃一个 `#[must_use]` 的返回值。它们可以
//! 是对的，但不能是无声的——本文件把 `src/` 下每一处登记成具名条目（文件 + 站点 id +
//! 为什么），id 由源码里紧邻该语句上一行的 `// audit-allow: <id>` 标记给出。改写措辞、
//! 换行或重排都不会让登记失配；新增一处没标记即失败，标记在而语句已不在也失败。
//!
//! 扫描范围是 `src/` 下全部 `.rs`，包含 `tests/` 子目录里的测试辅助：那里的丢弃也是
//! 代码，同样要有理由。

mod guards;

use std::path::{Path, PathBuf};

struct Rule {
    file: &'static str,
    id: &'static str,
    why: &'static str,
}

const ALLOW_RULES: &[Rule] = &[
    Rule {
        file: "cmd/layout.rs",
        id: "A-2",
        why: "布局写入口与执行器共用同一组参数，拆参数对象只是搬运",
    },
    Rule {
        file: "cmd/layout.rs",
        id: "A-3",
        why: "布局写入口与执行器共用同一组参数，拆参数对象只是搬运",
    },
    Rule {
        file: "cmd/resize.rs",
        id: "A-4",
        why: "同上：扩容入口的参数直接来自请求与计划",
    },
    Rule {
        file: "cmd/resize/finish.rs",
        id: "A-5",
        why: "同上：收尾步骤沿用同一组参数",
    },
    Rule {
        file: "cmd/resize/finish.rs",
        id: "A-6",
        why: "同上：收尾步骤沿用同一组参数",
    },
    Rule {
        file: "movepart/shift.rs",
        id: "A-7",
        why: "搬移计划的构造与 apply 共用入参",
    },
    Rule {
        file: "movepart/tests/support.rs",
        id: "A-8",
        why: "测试辅助镜像生产入口的签名",
    },
    Rule {
        file: "fsops/env.rs",
        id: "A-9",
        why: "外部工具失败的收尸动作，主错误已经返回",
    },
    Rule {
        file: "support.rs",
        id: "A-10",
        why: "诊断行写不出去不影响命令结论",
    },
    Rule {
        file: "testdir.rs",
        id: "A-11",
        why: "析构函数没有返回通道",
    },
    Rule {
        file: "dev/target.rs",
        id: "A-12",
        why: "清理目录失败不改变元数据结论",
    },
    Rule {
        file: "dev/target.rs",
        id: "A-13",
        why: "同步失败只让元数据回退到未落盘",
    },
    Rule {
        file: "recovery/evidence.rs",
        id: "A-14",
        why: "证据枚举按值传递，装箱会改变调用点的所有权形状",
    },
    Rule {
        file: "recovery/journal.rs",
        id: "A-15",
        why: "同上：记录枚举的变体大小不影响热路径",
    },
    Rule {
        file: "fsops/scope.rs",
        id: "A-16",
        why: "重读失败由下一轮重试体现，不在这一处上报",
    },
    Rule {
        file: "testdir.rs",
        id: "A-17",
        why: "夹具清理旧镜像的记录目录，失败没有后续动作",
    },
    Rule {
        file: "movepart/shift.rs",
        id: "A-18",
        why: "扩容收尾的参数是一次作业的独立事实，拆参数对象只是搬运",
    },
];

const DISCARD_RULES: &[Rule] = &[
    Rule {
        file: "fsops/env.rs",
        id: "D-1",
        why: "外部工具已失败，收尸动作的返回值无意义",
    },
    Rule {
        file: "fsops/env.rs",
        id: "D-2",
        why: "同上：等待已被杀死的子进程",
    },
    Rule {
        file: "support.rs",
        id: "D-3",
        why: "诊断输出失败不改变结论",
    },
    Rule {
        file: "support.rs",
        id: "D-4",
        why: "平台条件编译下未用到的入参",
    },
    Rule {
        file: "testdir.rs",
        id: "D-5",
        why: "Drop 中的清理失败没有返回通道",
    },
    Rule {
        file: "fsops/scope.rs",
        id: "D-6",
        why: "平台条件编译下未用到的入参",
    },
    Rule {
        file: "fsops/scope.rs",
        id: "D-7",
        why: "重读失败由下一轮重试体现",
    },
    Rule {
        file: "fsops/scope.rs",
        id: "D-8",
        why: "同上：整组入参在某平台分支不可用",
    },
    Rule {
        file: "fsops/scope.rs",
        id: "D-9",
        why: "拆除映射的失败路径没有后续动作",
    },
    Rule {
        file: "targetlock.rs",
        id: "D-10",
        why: "平台条件编译下未用到的期望值",
    },
    Rule {
        file: "targetlock.rs",
        id: "D-11",
        why: "释放锁文件失败不改变持锁结论",
    },
    Rule {
        file: "targetlock.rs",
        id: "D-12",
        why: "同上：释放锁文件失败不改变持锁结论",
    },
    Rule {
        file: "targetlock.rs",
        id: "D-13",
        why: "同上：清理别名锁文件",
    },
    Rule {
        file: "dev/tests.rs",
        id: "D-14",
        why: "测试夹具拆除 loop 设备，失败由回收站兜底",
    },
    Rule {
        file: "dev/tests.rs",
        id: "D-15",
        why: "同上：另一种持有者的拆除",
    },
    Rule {
        file: "dev/target.rs",
        id: "D-16",
        why: "清理失败不改变元数据结论",
    },
    Rule {
        file: "dev/target.rs",
        id: "D-17",
        why: "同步失败只让元数据回退",
    },
    Rule {
        file: "cmd/resize/finish.rs",
        id: "D-18",
        why: "平台条件编译下整组入参不可用",
    },
    Rule {
        file: "dev/source.rs",
        id: "D-19",
        why: "平台条件编译下未用到的句柄",
    },
    Rule {
        file: "dev/source.rs",
        id: "D-20",
        why: "同上：写入参数在某平台分支不可用",
    },
    Rule {
        file: "dev/occupancy.rs",
        id: "D-21",
        why: "非 Linux 分支没有占用探测",
    },
    Rule {
        file: "dev/identity.rs",
        id: "D-22",
        why: "平台条件编译下未用到的路径",
    },
    Rule {
        file: "dev/host_repo.rs",
        id: "D-23",
        why: "镜像侧不取目标文件的所有者",
    },
    Rule {
        file: "table/tests/fuzz.rs",
        id: "D-24",
        why: "模糊测试用例的临时文件清理",
    },
    Rule {
        file: "table/tests/fuzz.rs",
        id: "D-25",
        why: "同上：模糊测试用例的临时文件清理",
    },
    Rule {
        file: "movepart/faults.rs",
        id: "D-26",
        why: "故障注入宏里逐个吞掉未使用的入参",
    },
    Rule {
        file: "testdir.rs",
        id: "D-27",
        why: "同上：清理旧镜像的记录目录失败没有后续动作",
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

#[test]
fn every_allow_is_a_registered_exception() {
    guards::compare_sites(
        "`#[allow(...)]` 例外",
        &all_sources(),
        "A-",
        guards::all_lines,
        |_, line| line.starts_with("#[allow("),
        ALLOW_RULES.iter().map(|r| (r.file.to_string(), r.id)),
    );
}

#[test]
fn every_intentional_discard_is_a_registered_exception() {
    guards::compare_sites(
        "有意丢弃",
        &all_sources(),
        "D-",
        guards::all_lines,
        |_, line| line.starts_with("let _ =") || line == "$( let _ = $arg; )*",
        DISCARD_RULES.iter().map(|r| (r.file.to_string(), r.id)),
    );
}

#[test]
fn every_registered_exception_states_why() {
    for rule in ALLOW_RULES.iter().chain(DISCARD_RULES) {
        assert!(
            !rule.why.is_empty(),
            "{}: {} 没有写理由——登记项是给人复核的，不是占位",
            rule.file,
            rule.id
        );
    }
}

/// 条件放行只用于平台差异：`cfg_attr` 的谓词必须落在目标平台上才放行，这样警告在
/// 该代码真正被编译的平台上仍然生效。`cfg_attr(all(), allow(...))` 之类的无条件
/// 伪装会在这里被拦住
#[test]
fn a_conditional_allow_is_platform_scoped() {
    let mut bad = Vec::new();
    for (path, text) in all_sources() {
        for (n, line) in text.lines().enumerate() {
            let trimmed = line.trim_start();
            if (trimmed.starts_with("#[cfg_attr(") || trimmed.starts_with("#![cfg_attr("))
                && trimmed.contains("allow(")
                && !trimmed.contains("target_os")
            {
                bad.push(format!("{path}:{}: {}", n + 1, line.trim()));
            }
        }
    }
    assert!(
        bad.is_empty(),
        "`cfg_attr` 里的放行只用于平台差异，谓词必须含 `target_os`：\n{}",
        bad.join("\n")
    );
}

/// 模块级放行（`#![allow(...)]`）写在文件首部，标记没有位置可放，因此不进登记表；
/// 改用形态约束：只允许测试清理用的 `clippy::let_underscore_must_use`，平台条件放行
/// 走 `#![cfg_attr(...)]`（在用到该代码的平台上 lint 仍然生效）。无条件放行 rustc 的
/// `dead_code` / `unused_*` / `warnings` 会在这里被拦住
#[test]
fn a_module_level_allow_is_test_cleanup_only() {
    const ALLOWED: &str = "#![allow(clippy::let_underscore_must_use)]";
    let mut bad = Vec::new();
    for (path, text) in all_sources() {
        for (n, line) in text.lines().enumerate() {
            if line.trim_start().starts_with("#![allow(") && !line.trim_start().starts_with(ALLOWED) {
                bad.push(format!("{path}:{}: {}", n + 1, line.trim()));
            }
        }
    }
    assert!(
        bad.is_empty(),
        "模块级 `#![allow(...)]` 只用于测试清理的 `clippy::let_underscore_must_use`；\
         平台差异用 `#![cfg_attr(...)]`：\n{}",
        bad.join("\n")
    );
}
