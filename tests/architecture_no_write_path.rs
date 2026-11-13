//! 「计划步骤只有一个执行器」的结构守卫。
//!
//! 计划之外有三类写入：`undo` 的回退方向回放（原语在 `recovery/rollback.rs`）、
//! 已提交态续跑时的 NTFS HiddenSectors 回填、mkfs 前的 `wipe_zero`；另有证据文件
//! 自身的原子替换与外部工具的 stdin。本文件把「出现字节写原语的源码文件集合」
//! 钉成一张封闭清单。

mod guards;

use guards::{rust_sources, source};

/// 允许出现目标/证据写原语的文件。清单与 `rust_sources` 的扫描范围一致：
/// `tests/` 目录被跳过，其中的夹具直写只存在于 `#[cfg(test)]`，不在生产调用链上；
/// `fsops/tests.rs` 是文件而非目录，仍在扫描范围内。`write_guarded`/`write_converged`
/// 是派发到写入原语的守卫入口，`write_recorded`（回放写）与 `write_fixture`（测试夹具写）
/// 同样是写入入口，调用它们的文件一并计入清单。
const WRITE_SITES: &[&str] = &[
    "dev/source.rs",
    "execution/copy.rs",
    "execution/mod.rs",
    "execution/relocate.rs",
    "execution/table.rs",
    "fsops/env.rs",
    "fsops/swap.rs",
    "fsops/tests.rs",
    "gpt_policy.rs",
    "platform/evidence_store.rs",
    "recovery/journal.rs",
    "recovery/rollback.rs",
    "table/create.rs",
    "table/gpt_write.rs",
    "table/mbr.rs",
];

#[test]
fn no_production_module_opens_a_new_write_path() {
    let mut found = Vec::new();
    for (rel, text) in rust_sources() {
        let writes = text.contains(".write_at(")
            || text.contains(".write_all(")
            || text.contains(".write_guarded(")
            || text.contains(".write_converged(")
            || text.contains(".write_recorded(")
            || text.contains(".write_fixture(")
            || text.contains("pwrite(")
            || text.contains("renameat(")
            || text.contains("unlinkat(");
        if writes {
            found.push(rel);
        }
    }
    found.sort();
    found.dedup();
    let mut expected = WRITE_SITES.to_vec();
    expected.sort();
    assert_eq!(
        found, expected,
        "the set of files that can write bytes changed — either a second write path appeared, or this guard and the documented write points must be updated together"
    );

    let executor = source("execution/mod.rs");
    assert!(
        !executor.contains("全仓唯一的写盘路径"),
        "the executor must not claim to be the only writer in the whole repository"
    );
    assert!(
        executor.contains("计划步骤"),
        "the executor's scope is plan steps; the off-plan writes are registered separately"
    );
}

#[test]
fn no_write_claims_live_only_in_the_result_presenter() {
    // 「本次运行没有写」由结果呈现层按最终变体说一次，构造点的文案不得自带该断言。
    // 判据是结论的几种写法，不是一个字面量：只钉一种写法时，换个说法就能绕过去。
    // 例外按「文件 + 判别子串 + 为什么它不是运行级结论」逐行登记，不做整文件豁免；
    // 步骤级/组件级/对象级的句子必须自带范围词（this step / the filesystem / LV 等）。
    // 呈现层那句原句由本用例末尾单独断言：措辞一改就报错，不会静默放行。
    // 大小写不构成另一种写法，比较前统一折成小写；同义措辞一并列出
    const CLAIMS: &[&str] = &[
        "nothing was written",
        "nothing has been written",
        "wrote nothing",
        "nothing was modified",
        "nothing has been modified",
        "no bytes were written",
        "no data was written",
    ];
    const EXCEPTIONS: &[(&str, &str, &str)] = &[
        (
            "cmd/undo.rs",
            "nothing was written back",
            "回放结论：说的是没有回写记录里的字节，不是本次运行没写",
        ),
        (
            "recovery/rollback.rs",
            "nothing was written back",
            "回放结论：同上，范围是记录里的区间",
        ),
        (
            "execution/table.rs",
            "this step wrote nothing",
            "步骤级，且同句声明 earlier steps may already have written the target",
        ),
    ];
    let mut found = Vec::new();
    for (rel, text) in rust_sources() {
        for (n, line) in text.lines().enumerate() {
            let lower = line.to_lowercase();
            if !CLAIMS.iter().any(|claim| lower.contains(claim)) {
                continue;
            }
            let presenter = rel == "outcome.rs" && lower.contains("nothing was written by this run");
            let excepted = EXCEPTIONS
                .iter()
                .any(|(file, needle, _)| rel == *file && lower.contains(needle));
            if !presenter && !excepted {
                found.push(format!("{rel}:{}: {}", n + 1, line.trim()));
            }
        }
    }
    assert!(
        found.is_empty(),
        "a no-write claim outside the result presenter is not backed by anything:\n{}",
        found.join("\n")
    );
    // 豁免锚在呈现层的原句上：原句改了措辞，上面那条 `presenter` 就永远不命中，
    // 断言会在别处失败而不是静默放行
    assert!(
        source("outcome.rs").contains("nothing was written by this run"),
        "the presenter sentence this guard exempts is gone — the exemption covers nothing"
    );
}

#[test]
fn table_writes_go_through_the_guarded_entry() {
    // 生产表写入只经 write_converged（写前比对 + 写后回读都在这一个入口内）；
    // `#[cfg(test)]` 之后的直写是测试夹具，不在命令调用链上
    let mut found = Vec::new();
    for (rel, text) in rust_sources() {
        if !(rel == "execution/table.rs" || rel.starts_with("table/")) {
            continue;
        }
        let production = match text.find("#[cfg(test)]") {
            Some(at) => &text[..at],
            None => &text,
        };
        for (n, line) in production.lines().enumerate() {
            if line.contains(".write_at(") || line.contains(".write_data_at(") || line.contains(".write_raw(") {
                found.push(format!("{rel}:{}: {}", n + 1, line.trim()));
            }
        }
    }
    assert!(
        found.is_empty(),
        "a table write bypasses the guarded entry:\n{}",
        found.join("\n")
    );
    // 夹具直写免掉的只是「登记写事实」这一步：生产表写仍须经守卫入口，写原语仍须
    // 调登记函数，否则这条豁免覆盖的就不是一条真实路径
    assert!(
        source("execution/table.rs").contains("src.write_converged("),
        "the table step no longer writes through the guarded entry"
    );
    assert!(
        source("dev/source.rs").contains("crate::outcome::note_target_write("),
        "the write primitive no longer records the run write fact"
    );
}

#[test]
fn off_plan_writes_are_registered_in_the_executor_header() {
    let header = source("execution/mod.rs");
    for marker in [
        "fix_ntfs_hidden_sectors",
        "cmd/undo",
        "wipe_zero",
        "计划步骤",
    ] {
        assert!(
            header.contains(marker),
            "an off-plan write is no longer registered beside the executor: {marker}"
        );
    }
}

#[test]
fn every_unsupported_reason_has_a_build_site() {
    // `UnsupportedReason` 的变体只在 `fsops/error.rs` 声明，构造走同一文件的
    // `FsError::unsupported*`（`reason: UnsupportedReason::<变体>`）。新增变体而不给它
    // 构造点时，`unsupported_reason()` 会多出一条永远走不到的分支；本断言让这种改动失败。
    let text = source("fsops/error.rs");
    const ENUM_START: &str = "enum UnsupportedReason {";
    let body_start = text
        .find(ENUM_START)
        .expect("UnsupportedReason must be declared in fsops/error.rs")
        + ENUM_START.len();
    let body_end = body_start
        + text[body_start..]
            .find('}')
            .expect("UnsupportedReason enum body must be closed");
    let variants: Vec<&str> = text[body_start..body_end]
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with("//"))
        .map(|line| line.trim_end_matches(','))
        .collect();
    assert!(!variants.is_empty(), "UnsupportedReason must declare variants");

    // 只算生产代码：`fsops/tests.rs` 是文件而非目录，`rust_sources` 不会跳过它；
    // 让测试夹具里的构造充数，等于这条分支只在测试里可达
    let sources: Vec<String> = rust_sources()
        .into_iter()
        .filter(|(rel, _)| rel != "fsops/tests.rs")
        .map(|(_, text)| text)
        .collect();
    for variant in variants {
        let build = format!("reason: UnsupportedReason::{variant}");
        let sites: usize = sources.iter().map(|text| text.matches(&build).count()).sum();
        assert!(
            sites > 0,
            "UnsupportedReason::{variant} has no build site — every reason must be \
             constructible as `reason: UnsupportedReason::{variant}`"
        );
    }
}

#[test]
fn partial_is_built_only_at_the_result_type_and_the_pending_merges() {
    // `RunOutcome::Partial` 在源码里有两种形态：字面构造，与模式匹配（match 分支、
    // `matches!`、`if let`）。本断言只钉构造。
    //
    // 扫的形态：逐行取 `RunOutcome::Partial` 最后一次出现之后的余文。余文以 `{` 开头，
    // 且不满足下列任一模式特征时，计为一处字面构造：
    // - 花括号内出现 `..`（`{ .. }`、`{ cause, .. }`、`{ cause, pending, .. }`）；
    // - 同一行闭合，`}` 之后是 `=>` 或 `if`（match 分支的条件位）。
    // 字段写在下一行的构造余文恰为 `{`，同样命中；同一行里「模式在前、构造在后」的
    // 合并点只看最后一处，因此前半段的模式不计数。
    //
    // 失效条件（改这些写法会让断言静默不再命中构造，必须同步改这里）：
    // - `RunOutcome::` 与 `Partial` 之间换行；
    // - 跨行书写的模式（`{` 在行尾、字段与 `} =>` 在后续行）会反过来被误计为构造，
    //   此时断言失败而不是漏报，按新增例外登记或调整判据；
    // - 经类型别名构造（`use ... RunOutcome as R` 后写 `R::Partial {`）不再命中。
    // 例外按「文件 + 判别子串 + 理由」登记，与上面的 CLAIMS 同形，不做整文件豁免。
    const MERGES: &[(&str, &str, &str)] = &[
        (
            "cmd/layout.rs",
            "Err(RunOutcome::Partial { cause, pending, origin }) => RunOutcome::Partial {",
            "待办合并点：把本进程攒下的 post 串进执行器已给出的 5，5 本身由执行器产生",
        ),
        (
            "cmd/resize.rs",
            "Err(RunOutcome::Partial { cause, pending, origin }) => RunOutcome::Partial {",
            "扩容路径的待办合并点：同上，只补 pending，不新造 5",
        ),
    ];
    let mut found = Vec::new();
    let mut constructs = 0usize;
    for (rel, text) in rust_sources() {
        if rel == "outcome.rs" {
            continue;
        }
        for (n, line) in text.lines().enumerate() {
            let Some(at) = line.rfind("RunOutcome::Partial") else {
                continue;
            };
            let rest = line[at + "RunOutcome::Partial".len()..].trim();
            let Some(block) = rest.strip_prefix('{') else {
                continue;
            };
            let is_pattern = match block.find('}') {
                Some(close) => {
                    let inner = &block[..close];
                    let after = block[close + 1..].trim_start();
                    inner.contains("..") || after.starts_with("=>") || after.starts_with("if ")
                }
                None => false,
            };
            if is_pattern {
                continue;
            }
            constructs += 1;
            let merged = MERGES
                .iter()
                .any(|(file, needle, _)| rel == *file && line.trim() == *needle);
            if !merged {
                found.push(format!("{rel}:{}: {}", n + 1, line.trim()));
            }
        }
    }
    assert_eq!(
        constructs,
        MERGES.len(),
        "the guard classified {constructs} RunOutcome::Partial constructions outside outcome.rs, \
         expected the {} registered merges — the scan no longer sees a construction it is anchored \
         to, so it would pass while a new build site went unnoticed",
        MERGES.len()
    );
    assert!(
        found.is_empty(),
        "a RunOutcome::Partial literal appeared outside the result type and the two pending \
         merges — the five entry points must stay inside outcome.rs:\n{}",
        found.join("\n")
    );
    for (file, needle, _) in MERGES {
        assert!(
            source(file).lines().any(|line| line.trim() == *needle),
            "{file}: the pending merge this guard is anchored to is gone — the guard must be \
             updated together with the code"
        );
    }
}
