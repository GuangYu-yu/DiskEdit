//! 入口矩阵与 lint 登记：每个声明过的入口恰好有一行，并且有唯一的纵切归属。

use std::collections::BTreeSet;

use diskedit::app::cli::declared_entries;

/// 一个入口的纵切归属；新增入口必须在这里补行。
struct Entry {
    command: &'static str,
    slice: &'static str,
}

const ENTRIES: &[Entry] = &[
    Entry {
        command: "layout",
        slice: "S1",
    },
    Entry {
        command: "records list",
        slice: "S1",
    },
    Entry {
        command: "records verify",
        slice: "S1",
    },
    Entry {
        command: "records show",
        slice: "S1",
    },
    Entry {
        command: "check",
        slice: "S1",
    },
    Entry {
        command: "new",
        slice: "S2",
    },
    Entry {
        command: "add",
        slice: "S3",
    },
    Entry {
        command: "delete",
        slice: "S3",
    },
    Entry {
        command: "set",
        slice: "S3",
    },
    Entry {
        command: "move",
        slice: "S4",
    },
    Entry {
        command: "copy",
        slice: "S4",
    },
    Entry {
        command: "mkfs",
        slice: "S5",
    },
    Entry {
        command: "undo",
        slice: "S7",
    },
    Entry {
        command: "replay",
        slice: "S7",
    },
    Entry {
        command: "release",
        slice: "S7",
    },
    Entry {
        command: "resize",
        slice: "S6",
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
fn every_entry_has_one_row_and_a_known_slice() {
    const SLICES: &[&str] = &["S1", "S2", "S3", "S4", "S5", "S6", "S7"];
    let mut seen = BTreeSet::new();
    for entry in ENTRIES {
        assert!(
            seen.insert(entry.command),
            "入口 {} 在矩阵中有多行",
            entry.command
        );
        assert!(
            SLICES.contains(&entry.slice),
            "{} 的纵切归属 {} 不在 S1–S7 内",
            entry.command,
            entry.slice
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
