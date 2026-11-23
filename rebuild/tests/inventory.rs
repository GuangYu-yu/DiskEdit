//! 入口矩阵与 lint 登记：每个声明过的入口恰好有一行，声明缺行即失败。

use std::collections::BTreeSet;

use diskedit::app::cli::declared_entries;

/// 一个入口在判定层与报告层的落点。新增入口必须在这里补行。
struct Entry {
    command: &'static str,
    decision: &'static str,
    report: &'static str,
}

const ENTRIES: &[Entry] = &[
    Entry {
        command: "info",
        decision: "decide::info::plan",
        report: "report::json::info",
    },
    Entry {
        command: "inspect",
        decision: "decide::inspect::plan",
        report: "report::json::inspect",
    },
    Entry {
        command: "records list",
        decision: "decide::records::list",
        report: "report::json::records",
    },
    Entry {
        command: "records verify",
        decision: "decide::records::verify",
        report: "report::json::records",
    },
    Entry {
        command: "records scan",
        decision: "decide::records::scan",
        report: "report::json::records",
    },
    Entry {
        command: "resize",
        decision: "decide::resize::plan",
        report: "report::text::outcome",
    },
    Entry {
        command: "move",
        decision: "decide::move_partition::plan",
        report: "report::text::outcome",
    },
    Entry {
        command: "copy",
        decision: "decide::copy::plan",
        report: "report::text::outcome",
    },
    Entry {
        command: "delete",
        decision: "decide::delete::plan",
        report: "report::text::outcome",
    },
    Entry {
        command: "set",
        decision: "decide::set::plan",
        report: "report::text::outcome",
    },
    Entry {
        command: "check",
        decision: "decide::check::plan",
        report: "report::json::check",
    },
    Entry {
        command: "mkfs",
        decision: "decide::mkfs::plan",
        report: "report::text::outcome",
    },
    Entry {
        command: "resizefs",
        decision: "decide::resizefs::plan",
        report: "report::text::outcome",
    },
    Entry {
        command: "recover",
        decision: "decide::recover::plan",
        report: "report::text::outcome",
    },
    Entry {
        command: "new",
        decision: "decide::new::plan",
        report: "report::text::outcome",
    },
    Entry {
        command: "add",
        decision: "decide::add::plan",
        report: "report::text::outcome",
    },
];

#[test]
fn entry_matrix_matches_the_declared_entries() {
    let declared: BTreeSet<&str> = declared_entries().collect();
    let registered: BTreeSet<&str> = ENTRIES.iter().map(|entry| entry.command).collect();
    assert_eq!(
        declared, registered,
        "入口矩阵与命令声明不一致：新增入口必须补一行"
    );
}

#[test]
fn every_entry_has_one_row_and_two_destinations() {
    let mut seen = BTreeSet::new();
    for entry in ENTRIES {
        assert!(
            seen.insert(entry.command),
            "入口 {} 在矩阵中有多行",
            entry.command
        );
        assert!(!entry.decision.is_empty(), "{} 缺少判定落点", entry.command);
        assert!(!entry.report.is_empty(), "{} 缺少报告落点", entry.command);
    }
}

#[test]
fn declared_lints_are_enabled() {
    let manifest: &str = include_str!("../Cargo.toml");
    for lint in ["wildcard_enum_match_arm", "let_underscore_must_use"] {
        assert!(
            manifest.contains(&format!("{lint} = \"deny\"")),
            "Cargo.toml 未启用 {lint}"
        );
    }
}
