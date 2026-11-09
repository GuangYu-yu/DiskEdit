//! 领域层与规划层的边界守卫。
//!
//! 领域模块（domain/table/fsid/geometry/gpt_policy/movepart/online）只依赖更底层
//! 的读盘端口（`crate::dev` 的 `FileSource`/`WriteSource`），不感知平台能力与命令
//! 解析——前者属于 `crate::platform`，后者属于 `crate::cli`。计划值的构造只发生在
//! 装配层，其余模块拿到的都是已经成型的 `ConfirmedPlan`。
//!
//! 这些边界靠类型可见性只能约束到 crate 外部；crate 内部由本守卫对源码文本断言。

mod guards;

use guards::rust_sources;

/// 领域模块的生产文件不得引用平台能力或命令解析。
/// `crate::dev` 是有意放行的：`FileSource`/`WriteSource` 就是领域层读盘的端口形态。
/// `movepart/tests` 是 crate 内单测夹具，不受生产边界约束。
#[test]
fn domain_modules_do_not_reach_platform_or_cli() {
    let domain_prefixes = ["domain.rs", "table.rs", "fsid.rs", "geometry.rs", "gpt_policy.rs", "movepart/", "online/"];
    let forbidden = ["crate::platform::", "crate::cli::"];
    for (rel, text) in rust_sources() {
        let in_domain = domain_prefixes.iter().any(|p| rel.starts_with(p));
        if !in_domain || rel.starts_with("movepart/tests") {
            continue;
        }
        for token in forbidden {
            assert!(
                !text.contains(token),
                "`{rel}` references `{token}`; domain modules must stay behind the disk-port boundary"
            );
        }
    }
}

/// `ConfirmedPlan` 的组装函数只在装配层调用。命令层与执行器拿到的都是成品，
/// 不在半途拼装计划。
#[test]
fn confirmed_plan_is_assembled_only_in_assembly() {
    for (rel, text) in rust_sources() {
        if rel == "assembly.rs" {
            continue;
        }
        assert!(
            !text.contains("ConfirmedPlan::assembled_"),
            "`{rel}` assembles a `ConfirmedPlan`; construction must stay in `assembly.rs`"
        );
    }
}
