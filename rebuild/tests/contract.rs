//! 命令面契约：声明的形状、进程级出口与退出码映射。

use std::collections::BTreeSet;
use std::ffi::OsString;

use diskedit::app::cli::{declare, declared_entries};
use diskedit::app::decide::Refusal;
use diskedit::app::execute::{FailureKind, Outcome};
use diskedit::app::report::exit;

fn run(arguments: &[&str]) -> u8 {
    let args: Vec<OsString> = std::iter::once("diskedit")
        .chain(arguments.iter().copied())
        .map(OsString::from)
        .collect();
    diskedit::app::run(args)
}

#[test]
fn command_surface_has_sixteen_named_entries() {
    let names: Vec<&str> = declared_entries().collect();
    assert_eq!(names.len(), 16);
    let unique: BTreeSet<&str> = names.iter().copied().collect();
    assert_eq!(unique.len(), names.len(), "命令名不得重复");
    for expected in [
        "info",
        "inspect",
        "records list",
        "records verify",
        "records scan",
        "resize",
        "move",
        "copy",
        "delete",
        "set",
        "check",
        "mkfs",
        "resizefs",
        "recover",
        "new",
        "add",
    ] {
        assert!(unique.contains(expected), "命令面缺少 {expected}");
    }
}

#[test]
fn exported_surface_has_the_documented_shape() {
    let document: serde_json::Value =
        serde_json::from_str(&declare::surface_json()).expect("命令面应是合法 JSON");
    let commands = document["commands"]
        .as_array()
        .expect("命令面应有 commands 数组");
    assert_eq!(commands.len(), 16);
    for command in commands {
        assert!(command["name"].is_string());
        assert!(matches!(
            command["confirmation"].as_str(),
            Some("Required" | "None")
        ));
        assert!(command["common_options"].is_array());
        let variants = command["variants"].as_array().expect("variants 应为数组");
        assert!(!variants.is_empty(), "{} 没有形态", command["name"]);
        for variant in variants {
            assert!(variant["syntax"].is_string());
            assert!(variant["options"].is_array());
            assert!(variant["selectors"].is_array());
            assert!(matches!(
                variant["target"].as_str(),
                Some("Any" | "Whole" | "Partition")
            ));
            assert!(variant["default"].is_boolean());
            assert!(variant["syntax_only_options"].is_array());
        }
    }
}

#[test]
fn help_and_usage_choose_the_documented_process_codes() {
    assert_eq!(run(&[]), exit::USAGE);
    assert_eq!(run(&["--help"]), exit::SUCCESS);
    assert_eq!(run(&["-h"]), exit::SUCCESS);
    assert_eq!(run(&["help"]), exit::SUCCESS);
    assert_eq!(run(&["--version"]), exit::SUCCESS);
    assert_eq!(run(&["-V"]), exit::SUCCESS);
    assert_eq!(run(&["bogus"]), exit::USAGE);
    assert_eq!(run(&["help", "info"]), exit::SUCCESS);
    assert_eq!(run(&["help", "records", "list"]), exit::SUCCESS);
    assert_eq!(run(&["info", "--help"]), exit::SUCCESS);
    assert_eq!(run(&["--help", "info"]), exit::SUCCESS);
    assert_eq!(run(&["help", "bogus"]), exit::USAGE);
    assert_eq!(run(&["help", "records"]), exit::USAGE);
}

// covers: R0782, R0783, R0784, R0785
#[test]
fn outcomes_and_refusals_map_to_the_seven_codes() {
    let codes = [
        exit::SUCCESS,
        exit::INTERNAL,
        exit::USAGE,
        exit::REFUSED,
        exit::ENVIRONMENT,
        exit::PARTIAL,
        exit::UNSUPPORTED,
    ];
    assert_eq!(codes, [0, 1, 2, 3, 4, 5, 6]);

    assert_eq!(exit::exit_of(&Outcome::completed()), exit::SUCCESS);
    assert_eq!(
        exit::exit_of(&Outcome::unwritten(FailureKind::Refused, "no")),
        exit::REFUSED
    );
    assert_eq!(
        exit::exit_of(&Outcome::unwritten(FailureKind::Unsupported, "no")),
        exit::UNSUPPORTED
    );
    assert_eq!(
        exit::exit_of(&Outcome::unwritten(FailureKind::FailedBeforeWrite, "no")),
        exit::ENVIRONMENT
    );
    assert_eq!(exit::exit_of(&Outcome::needs_recovery("no")), exit::PARTIAL);
    assert_eq!(
        exit::exit_of_refusal(&Refusal::ConfirmationRequired),
        exit::USAGE
    );
    assert_eq!(exit::exit_of_refusal(&Refusal::TableMissing), exit::REFUSED);
    assert_eq!(
        exit::exit_of_refusal(&Refusal::MountNotBlockDevice {
            source: "/dev/sda".to_owned()
        }),
        exit::UNSUPPORTED
    );
}
