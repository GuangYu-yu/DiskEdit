//! 逃逸站点普查的可执行台账（`.trae/documents/逃逸站点表.md` 第 17 节）。
//!
//! 普查命令原本是十一条一次性 ripgrep。这里把其中能离散枚举的两类钉成封闭清单：
//!
//! - 折叠：`unwrap_or(false|true)` 把缺失折成布尔量；
//! - 跳过：`let Ok(..) else` 与 `if let Ok(..)` 在读失败时跳过当前判定。
//!
//! 每条按「文件 + 站点 id」登记，id 由源码里紧邻该语句上一行的 `// audit-allow: <id>`
//! 标记给出；语句换行、重排或改写措辞都不会让登记失配。新增未登记站点、或标记在而
//! 语句已不在，都直接失败。
//! 同类扫描无法离散枚举的形态（布尔传参、`_ =>` catch-all、文档承诺）留在文档，
//! 由第 17 节逐条给出上游保证与复现命令，不在本文件假装有判据。
//!
//! **项目约定，不属于方法论要求**：「普查必须固化成可执行清单」是本仓库对既有一次性
//! ripgrep 普查的收口方式；方法论只要求每条待复核站点给出上游保证与复现命令。
//!
//! `.trae/` 不在 crate 内，本文件不依赖台账存在。

mod guards;

use std::collections::BTreeMap;

use guards::{production_lines, rust_sources};

/// S1：缺失折成布尔量的站点
struct FoldSite {
    id: &'static str,
    file: &'static str,
}

/// S2：读失败后跳过判定的站点
struct SkipSite {
    id: &'static str,
    file: &'static str,
    /// 上游保证：为什么这一跳不会把缺失折成放行
    why: &'static str,
}

const FOLD_SITES: &[FoldSite] = &[
    FoldSite { id: "S1-10", file: "online/part.rs" },
];

const SKIP_SITES: &[SkipSite] = &[
    SkipSite {
        id: "S2-19",
        file: "cli.rs",
        why: "解析过滤，失败落到 `Err` 而不是跳过校验",
    },
    SkipSite {
        id: "S2-1",
        file: "ioctl.rs",
        why: "溢出按拒绝返回，不折成 0 或省略",
    },
    SkipSite {
        id: "S2-2",
        file: "ioctl.rs",
        why: "溢出按拒绝返回，不折成 0 或省略",
    },
    SkipSite {
        id: "S2-3",
        file: "ioctl.rs",
        why: "溢出按拒绝返回，不折成 0 或省略",
    },
    SkipSite {
        id: "S2-4",
        file: "dev/identity.rs",
        why: "答案是「是否整盘设备」，读不出反而按是处理，进入更严的分支",
    },
    SkipSite {
        id: "S2-20",
        file: "dev/target.rs",
        why: "目录 fsync 只影响元数据新鲜度，失败时数据写入已完成且无前置状态",
    },
    SkipSite {
        id: "S2-14",
        file: "fsops/grow.rs",
        why: "随机源缺失退回进程熵源，不放弃擦除",
    },
    SkipSite {
        id: "S2-21",
        file: "fsops/grow.rs",
        why: "取不到目录项身份时放弃清理自己的记录，不签发许可",
    },
    SkipSite {
        id: "S2-12",
        file: "fsops/mount.rs",
        why: "只跳过不是进程号的目录名，挂载归属由后续系统表核对",
    },
    SkipSite {
        id: "S2-13",
        file: "fsops/mount.rs",
        why: "只决定是否等待 udev 落定，不参与任何判定",
    },
    SkipSite {
        id: "S2-8",
        file: "fsops/scope.rs",
        why: "候选读不出时跳过该候选，全部跳过则整体报错",
    },
    SkipSite {
        id: "S2-9",
        file: "fsops/scope.rs",
        why: "候选内容不可解析时跳过该候选，全部跳过则整体报错",
    },
    SkipSite {
        id: "S2-18",
        file: "movepart/faults.rs",
        why: "仅 test-faults 特性下存在；未设置注入点时不做任何事",
    },
    SkipSite {
        id: "S2-16",
        file: "platform/evidence_store.rs",
        why: "名字含 NUL 时直接出错，不折成另一个记录名",
    },
    SkipSite {
        id: "S2-15",
        file: "platform/probe.rs",
        why: "取不到元数据时按镜像打开，随后打开失败即拒绝",
    },
    SkipSite {
        id: "S2-22",
        file: "recovery/engine.rs",
        why: "锁前预检读不出所有者时跳过，锁下枚举仍按记录形态判定",
    },
    SkipSite {
        id: "S2-23",
        file: "dev/identity.rs",
        why: "读不出文件元数据时退回路径键：键只定位落点，记录归属仍按载荷身份核对",
    },
    SkipSite {
        id: "S2-24",
        file: "dev/identity.rs",
        why: "打不开镜像时退回路径键，用于定位落点，不作身份证据",
    },
    SkipSite {
        id: "S2-25",
        file: "dev/identity.rs",
        why: "取不到文件对象身份时退回路径键，用于定位落点，不作身份证据",
    },
    SkipSite {
        id: "S2-17",
        file: "recovery/engine.rs",
        why: "取不到元数据时按镜像打开，随后打开失败即拒绝",
    },
    SkipSite {
        id: "S2-5",
        file: "recovery/journal.rs",
        why: "只清理目录里的临时项，读不出就放弃清理，不签发许可",
    },
    SkipSite {
        id: "S2-6",
        file: "recovery/journal.rs",
        why: "只清理目录里的临时项，读不出就放弃清理，不签发许可",
    },
    SkipSite {
        id: "S2-7",
        file: "recovery/journal.rs",
        why: "只跳过无法命名的目录项，不影响记录状态判定",
    },
];

fn is_test_file(path: &str) -> bool {
    path.ends_with("tests.rs") || path.contains("/tests/")
}

fn scan(match_line: impl Fn(&str) -> bool) -> BTreeMap<(String, String), usize> {
    let mut found = BTreeMap::new();
    for (path, text) in rust_sources() {
        if is_test_file(&path) {
            continue;
        }
        for (_, line) in production_lines(&text) {
            let trimmed = line.trim();
            if !match_line(trimmed) {
                continue;
            }
            *found
                .entry((path.clone(), trimmed.to_string()))
                .or_insert(0) += 1;
        }
    }
    found
}

/// 参与普查的源文件：`tests/` 与 `tests.rs` 是夹具，不在生产调用链上
fn production_sources() -> Vec<(String, String)> {
    rust_sources().into_iter().filter(|(path, _)| !is_test_file(path)).collect()
}

/// `settle_companion` 的第三参数是提交事实的开关，每个调用点都要有人复核过
const SETTLE_SITES: &[(&str, &str, &str)] = &[
    (
        "cmd/recover.rs",
        "C-1",
        "提交后未清理的释放路径：提交位已由记录自身给出，调用方不追加提交",
    ),
    (
        "cmd/recover.rs",
        "C-2",
        "盘面已是原文的零写入收尾：释放记录时落提交位，不重新判定区间",
    ),
    (
        "recovery/rollback.rs",
        "C-3",
        "回放完成后落提交：提交位由回滚本身确认，只差释放记录",
    ),
];

#[test]
fn fold_to_bool_sites_are_registered() {
    guards::compare_sites(
        "折叠站点",
        &production_sources(),
        "S1-",
        production_lines,
        |_, line| line.contains("unwrap_or(false)") || line.contains("unwrap_or(true)"),
        FOLD_SITES.iter().map(|s| (s.file.to_string(), s.id)),
    );
}

#[test]
fn skip_on_read_failure_sites_are_registered() {
    guards::compare_sites(
        "跳过站点",
        &production_sources(),
        "S2-",
        production_lines,
        |_, line| line.contains("let Ok(") || line.contains("if let Ok("),
        SKIP_SITES.iter().map(|s| (s.file.to_string(), s.id)),
    );
}

#[test]
fn registered_skip_sites_carry_an_upstream_guarantee() {
    for site in SKIP_SITES {
        assert!(
            site.why.len() >= 8,
            "{} 没有写上游保证；登记的理由是给人复核的，不是占位",
            site.id
        );
    }
}

#[test]
fn commit_is_never_fed_a_constant_gate() {
    let found = scan(|l| l.contains("RecoveryEngine::commit(") && l.contains("false"));
    assert!(
        found.is_empty(),
        "提交闸不得由调用方常量关死：{found:?}"
    );
}

#[test]
fn settle_companion_call_sites_are_registered() {
    guards::compare_sites(
        "settle_companion 调用点",
        &production_sources(),
        "C-",
        production_lines,
        |_, line| line.contains("settle_companion(") && !line.contains("fn settle_companion"),
        SETTLE_SITES.iter().map(|(file, id, _)| (file.to_string(), *id)),
    );
}
