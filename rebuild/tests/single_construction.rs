//! 铸造点登记：每个不变量关键类型只在它声明的文件里定义一次。

use std::collections::BTreeSet;

/// 一个类型的定义位置与所在源码。类型名与 `pub struct`/`pub enum` 之后的标识符一致。
struct MintPoint {
    name: &'static str,
    path: &'static str,
    source: &'static str,
}

const MINT_POINTS: &[MintPoint] = &[
    MintPoint {
        name: "Snapshot",
        path: "src/app/observe/snapshot.rs",
        source: include_str!("../src/app/observe/snapshot.rs"),
    },
    MintPoint {
        name: "Request",
        path: "src/app/observe/mod.rs",
        source: include_str!("../src/app/observe/mod.rs"),
    },
    MintPoint {
        name: "Observation",
        path: "crates/core/src/evidence.rs",
        source: include_str!("../crates/core/src/evidence.rs"),
    },
    MintPoint {
        name: "Decision",
        path: "src/app/decide/mod.rs",
        source: include_str!("../src/app/decide/mod.rs"),
    },
    MintPoint {
        name: "Plan",
        path: "src/app/decide/plan.rs",
        source: include_str!("../src/app/decide/plan.rs"),
    },
    MintPoint {
        name: "Refusal",
        path: "src/app/decide/refusal.rs",
        source: include_str!("../src/app/decide/refusal.rs"),
    },
    MintPoint {
        name: "ConfirmedPlan",
        path: "src/app/decide/confirmed.rs",
        source: include_str!("../src/app/decide/confirmed.rs"),
    },
    MintPoint {
        name: "Recorded",
        path: "src/app/execute/token.rs",
        source: include_str!("../src/app/execute/token.rs"),
    },
    MintPoint {
        name: "ExecutionState",
        path: "src/app/execute/state.rs",
        source: include_str!("../src/app/execute/state.rs"),
    },
    MintPoint {
        name: "Lease",
        path: "src/platform/linux/source/lease.rs",
        source: include_str!("../src/platform/linux/source/lease.rs"),
    },
];

#[test]
fn every_mint_point_is_registered_once() {
    let mut seen = BTreeSet::new();
    for point in MINT_POINTS {
        assert!(
            seen.insert(point.name),
            "{} 在登记表中有多余的一行",
            point.name
        );
    }
}

#[test]
fn each_type_is_defined_in_its_registered_file() {
    for point in MINT_POINTS {
        let count =
            defines(point.source, "struct", point.name) + defines(point.source, "enum", point.name);
        assert_eq!(
            count, 1,
            "{} 应在 {} 恰好定义一次，实际 {} 次",
            point.name, point.path, count
        );
    }
}

/// 统计 `pub <keyword> <name>` 的出现次数，要求名字后面是边界字符（不是标识符）。
fn defines(source: &str, keyword: &str, name: &str) -> usize {
    let marker = format!("pub {keyword} {name}");
    source
        .match_indices(&marker)
        .filter(|(index, _)| {
            let rest = &source[index + marker.len()..];
            rest.chars()
                .next()
                .is_none_or(|next| !next.is_alphanumeric() && next != '_')
        })
        .count()
}
