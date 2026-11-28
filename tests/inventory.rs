//! 入口矩阵与 lint 登记：每个声明过的入口恰好有一行，并归属于一个已登记的类别。

use std::collections::BTreeSet;

use diskedit::app::cli::declared_entries;

/// 一个入口的类别归属；新增入口必须在这里补行。
struct Entry {
    command: &'static str,
    category: &'static str,
}

const ENTRIES: &[Entry] = &[
    Entry {
        command: "layout",
        category: "inspect",
    },
    Entry {
        command: "records list",
        category: "inspect",
    },
    Entry {
        command: "records verify",
        category: "inspect",
    },
    Entry {
        command: "records show",
        category: "inspect",
    },
    Entry {
        command: "check",
        category: "inspect",
    },
    Entry {
        command: "new",
        category: "table-create",
    },
    Entry {
        command: "add",
        category: "table-edit",
    },
    Entry {
        command: "delete",
        category: "table-edit",
    },
    Entry {
        command: "set",
        category: "table-edit",
    },
    Entry {
        command: "move",
        category: "relocate",
    },
    Entry {
        command: "copy",
        category: "relocate",
    },
    Entry {
        command: "mkfs",
        category: "filesystem-create",
    },
    Entry {
        command: "undo",
        category: "record-lifecycle",
    },
    Entry {
        command: "replay",
        category: "record-lifecycle",
    },
    Entry {
        command: "release",
        category: "record-lifecycle",
    },
    Entry {
        command: "resize",
        category: "partition-resize",
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
fn every_entry_has_one_row_and_a_known_category() {
    const CATEGORIES: &[&str] = &[
        "inspect",
        "table-create",
        "table-edit",
        "relocate",
        "filesystem-create",
        "partition-resize",
        "record-lifecycle",
    ];
    let mut seen = BTreeSet::new();
    for entry in ENTRIES {
        assert!(
            seen.insert(entry.command),
            "入口 {} 在矩阵中有多行",
            entry.command
        );
        assert!(
            CATEGORIES.contains(&entry.category),
            "{} 的类别 {} 不在已登记的类别内",
            entry.command,
            entry.category
        );
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
