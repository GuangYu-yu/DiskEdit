//! 命令面声明：16 个入口与 `records` 三个子命令的用法、目标后缀约束与选项白名单。
//!
//! 帮助文本与用法校验都读同一张表；解析只按表里的白名单收选项，未列出的选项一律
//! 落用法错误。

/// 命令分组，仅用于帮助排版。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Group {
    Everyday,
    LowLevel,
}

/// 目标后缀约束。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TargetMode {
    /// 只接受不带后缀的目标。
    Whole,
    /// 必须带 `:N` 或 `:last`。
    Partition,
    /// 带不带后缀都可以。
    Any,
    /// 没有目标参数（`records list`）。
    None,
}

/// 一个选项：是否需要取值。
#[derive(Clone, Copy, Debug)]
pub struct Opt {
    pub name: &'static str,
    pub value: bool,
}

/// 命令声明。
#[derive(Clone, Copy, Debug)]
pub struct Spec {
    pub name: &'static str,
    pub group: Group,
    pub summary: &'static str,
    pub usage: &'static [&'static str],
    pub target: TargetMode,
    pub confirmation: bool,
    pub options: &'static [Opt],
}

const SECTOR: Opt = Opt {
    name: "--sector-size",
    value: true,
};
const YES: Opt = Opt {
    name: "--yes",
    value: false,
};
const IDENTITY: Opt = Opt {
    name: "--identity",
    value: true,
};

const INFO: &[Opt] = &[
    SECTOR,
    Opt {
        name: "--grow",
        value: false,
    },
];
const NONE: &[Opt] = &[];
const RESIZE: &[Opt] = &[
    SECTOR,
    YES,
    IDENTITY,
    Opt {
        name: "--allow-move",
        value: false,
    },
    Opt {
        name: "--chunk",
        value: true,
    },
    Opt {
        name: "--grow-lv",
        value: false,
    },
    Opt {
        name: "--lv",
        value: true,
    },
    Opt {
        name: "--no-fs",
        value: false,
    },
    Opt {
        name: "--dry-run",
        value: false,
    },
    Opt {
        name: "--end",
        value: true,
    },
    Opt {
        name: "--grow-to-end",
        value: false,
    },
    Opt {
        name: "--align",
        value: true,
    },
];
const MOVE: &[Opt] = &[
    SECTOR,
    YES,
    IDENTITY,
    Opt {
        name: "--start",
        value: true,
    },
    Opt {
        name: "--align",
        value: true,
    },
    Opt {
        name: "--chunk",
        value: true,
    },
];
const COPY: &[Opt] = &[
    SECTOR,
    YES,
    IDENTITY,
    Opt {
        name: "--start",
        value: true,
    },
    Opt {
        name: "--align",
        value: true,
    },
    Opt {
        name: "--chunk",
        value: true,
    },
    Opt {
        name: "--name",
        value: true,
    },
    Opt {
        name: "--keep-partuuid",
        value: false,
    },
];
const DELETE: &[Opt] = &[SECTOR, YES, IDENTITY];
const SET: &[Opt] = &[
    SECTOR,
    YES,
    IDENTITY,
    Opt {
        name: "--random",
        value: false,
    },
];
const CHECK: &[Opt] = &[SECTOR];
const MKFS: &[Opt] = &[
    SECTOR,
    YES,
    IDENTITY,
    Opt {
        name: "--label",
        value: true,
    },
];
const RESIZEFS: &[Opt] = &[SECTOR, YES, IDENTITY];
const RECOVER: &[Opt] = &[
    SECTOR,
    YES,
    IDENTITY,
    Opt {
        name: "--replay-table-only",
        value: false,
    },
    Opt {
        name: "--release",
        value: false,
    },
    Opt {
        name: "--discard-replay",
        value: false,
    },
];
const NEW: &[Opt] = &[
    SECTOR,
    YES,
    IDENTITY,
    Opt {
        name: "--table",
        value: true,
    },
    Opt {
        name: "--overwrite-data",
        value: false,
    },
];
const ADD: &[Opt] = &[
    SECTOR,
    YES,
    IDENTITY,
    Opt {
        name: "--start",
        value: true,
    },
    Opt {
        name: "--align",
        value: true,
    },
    Opt {
        name: "--part-number",
        value: true,
    },
    Opt {
        name: "--name",
        value: true,
    },
    Opt {
        name: "--type-guid",
        value: true,
    },
];

/// 全部命令声明。`records` 的三个子命令各占一项，名字带空格。
pub const SPECS: &[Spec] = &[
    Spec {
        name: "info",
        group: Group::Everyday,
        summary: "show the target and partition overview",
        usage: &[
            "diskedit info <TARGET> [--sector-size N]",
            "diskedit info <TARGET>:N --grow [--sector-size N]",
        ],
        target: TargetMode::Any,
        confirmation: false,
        options: INFO,
    },
    Spec {
        name: "inspect",
        group: Group::Everyday,
        summary: "show per-partition extents and content",
        usage: &["diskedit inspect <TARGET>"],
        target: TargetMode::Whole,
        confirmation: false,
        options: &[IDENTITY],
    },
    Spec {
        name: "records list",
        group: Group::Everyday,
        summary: "list recovery record directory entries",
        usage: &["diskedit records list [TARGET]"],
        target: TargetMode::None,
        confirmation: false,
        options: &[IDENTITY],
    },
    Spec {
        name: "records verify",
        group: Group::Everyday,
        summary: "verify one recovery record",
        usage: &["diskedit records verify <TARGET> <ID>"],
        target: TargetMode::Whole,
        confirmation: false,
        options: &[IDENTITY],
    },
    Spec {
        name: "records scan",
        group: Group::Everyday,
        summary: "scan a recovery record container",
        usage: &["diskedit records scan <ROOT>"],
        target: TargetMode::Whole,
        confirmation: false,
        options: NONE,
    },
    Spec {
        name: "resize",
        group: Group::Everyday,
        summary: "resize a partition",
        usage: &[
            "diskedit resize <TARGET>[:N] <SIZE> [--allow-move] [--chunk N] [--grow-lv] [--lv NAME] [--no-fs] --yes",
            "diskedit resize <TARGET>:N --end LBA [--align A] [--no-fs] --yes",
            "diskedit resize <TARGET>:N --grow-to-end [--align A] [--no-fs] --yes",
        ],
        target: TargetMode::Any,
        confirmation: true,
        options: RESIZE,
    },
    Spec {
        name: "move",
        group: Group::Everyday,
        summary: "relocate a partition",
        usage: &["diskedit move <TARGET>:N --start <LBA|end|align> [--chunk N] --yes [--align A]"],
        target: TargetMode::Partition,
        confirmation: true,
        options: MOVE,
    },
    Spec {
        name: "copy",
        group: Group::Everyday,
        summary: "copy a partition to a new location and add its entry",
        usage: &[
            "diskedit copy <TARGET>:N --start <LBA|end> [--chunk N] --yes [--align A] [--name S] [--keep-partuuid]",
        ],
        target: TargetMode::Partition,
        confirmation: true,
        options: COPY,
    },
    Spec {
        name: "delete",
        group: Group::Everyday,
        summary: "delete a partition entry",
        usage: &["diskedit delete <TARGET>:N --yes"],
        target: TargetMode::Partition,
        confirmation: true,
        options: DELETE,
    },
    Spec {
        name: "set",
        group: Group::Everyday,
        summary: "set a partition or filesystem attribute",
        usage: &[
            "diskedit set <TARGET>:N name S --yes",
            "diskedit set <TARGET>:N label S --yes",
            "diskedit set <TARGET>:N uuid [U] --yes [--random]",
            "diskedit set <TARGET>:N flag F on|off --yes",
            "diskedit set <TARGET>:N type GUID|0xXX --yes",
        ],
        target: TargetMode::Partition,
        confirmation: true,
        options: SET,
    },
    Spec {
        name: "check",
        group: Group::Everyday,
        summary: "check the filesystem in a partition",
        usage: &["diskedit check <TARGET>:N"],
        target: TargetMode::Partition,
        confirmation: false,
        options: CHECK,
    },
    Spec {
        name: "mkfs",
        group: Group::Everyday,
        summary: "create a filesystem in a partition",
        usage: &["diskedit mkfs <TARGET>:N <FS> --yes [--label S]"],
        target: TargetMode::Partition,
        confirmation: true,
        options: MKFS,
    },
    Spec {
        name: "resizefs",
        group: Group::Everyday,
        summary: "resize the filesystem inside a partition",
        usage: &[
            "diskedit resizefs <TARGET>:N [BYTES] --yes",
            "diskedit resizefs <MOUNTPOINT> [BYTES] --yes",
        ],
        target: TargetMode::Any,
        confirmation: true,
        options: RESIZEFS,
    },
    Spec {
        name: "recover",
        group: Group::Everyday,
        summary: "roll back, replay or release a recovery record",
        usage: &[
            "diskedit recover <TARGET> --yes",
            "diskedit recover <TARGET> --replay-table-only --yes",
            "diskedit recover <TARGET> --release [--discard-replay] --yes",
        ],
        target: TargetMode::Whole,
        confirmation: true,
        options: RECOVER,
    },
    Spec {
        name: "new",
        group: Group::LowLevel,
        summary: "create a fresh partition table",
        usage: &["diskedit new <TARGET> [--table gpt|mbr] --yes [--overwrite-data]"],
        target: TargetMode::Whole,
        confirmation: true,
        options: NEW,
    },
    Spec {
        name: "add",
        group: Group::LowLevel,
        summary: "add a partition",
        usage: &[
            "diskedit add <TARGET> SIZE [--start LBA] [--align A] [--part-number N] [--name S] [--type-guid G] --yes",
        ],
        target: TargetMode::Whole,
        confirmation: true,
        options: ADD,
    },
];

/// 按名字查声明；`records` 的子命令用空格拼接。
pub fn find(name: &str) -> Option<&'static Spec> {
    SPECS.iter().find(|spec| spec.name == name)
}

/// 打印一条命令的帮助；名字未知时返回 `false`。
pub fn print_command(name: &str) -> bool {
    let Some(spec) = find(name) else {
        return false;
    };
    println!("diskedit {}", spec.name);
    println!();
    for line in spec.usage {
        println!("  {line}");
    }
    println!();
    println!("{}", spec.summary);
    if !spec.options.is_empty() {
        println!();
        for option in spec.options {
            if option.value {
                println!("  {} VALUE", option.name);
            } else {
                println!("  {}", option.name);
            }
        }
    }
    println!();
    println!("target: image path or block device; :N = partition number (1-based)");
    true
}

/// 打印帮助；写失败不影响退出码。
pub fn print_help() {
    println!("DiskEdit: disk partition editor");
    for group in [Group::Everyday, Group::LowLevel] {
        println!();
        println!(
            "{}",
            match group {
                Group::Everyday => "Commands:",
                Group::LowLevel => "Low-level:",
            }
        );
        for spec in SPECS.iter().filter(|spec| spec.group == group) {
            println!("  {:14} {}", spec.name, spec.summary);
            for usage in spec.usage {
                println!("      {usage}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_surface_is_exactly_the_baseline() {
        let names: Vec<&str> = SPECS.iter().map(|spec| spec.name).collect();
        assert_eq!(
            names,
            [
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
            ]
        );
    }

    #[test]
    fn every_spec_declares_usage_and_unique_options() {
        for spec in SPECS {
            assert!(!spec.usage.is_empty(), "{} 缺少用法", spec.name);
            for usage in spec.usage {
                assert!(
                    usage.starts_with("diskedit "),
                    "{} 的用法必须以 diskedit 开头",
                    spec.name
                );
            }
            let mut seen = Vec::new();
            for option in spec.options {
                assert!(
                    !seen.contains(&option.name),
                    "{} 重复声明选项 {}",
                    spec.name,
                    option.name
                );
                seen.push(option.name);
            }
        }
    }

    #[test]
    fn confirmation_matches_read_and_write_commands() {
        let read_only = [
            "info",
            "inspect",
            "records list",
            "records verify",
            "records scan",
            "check",
        ];
        for spec in SPECS {
            let expected = !read_only.contains(&spec.name);
            assert_eq!(
                spec.confirmation, expected,
                "{} 的确认要求与读写类别不符",
                spec.name
            );
        }
    }
}
