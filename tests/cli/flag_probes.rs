//! 黑盒旗标探针：每条注册旗标一行，写明它在参数层的形态、一个合法样例，
//! 以及确实接受它的命令。
//!
//! 本文件被两处 `#[path]` 引入——`src/cli.rs` 的单元测试（把探针与 `FLAGS`
//! 一一对账）与 `tests/cli/` 的集成测试（拿它驱动真实二进制）。集成测试是独立
//! crate，看不到 bin crate 的注册表，探针因此必须落在能被两侧引入的文件里。

/// 旗标的取值形态：布尔旗标不带值，值型旗标必须带一个
#[derive(Clone, Copy)]
pub(crate) enum Kind {
    Bool,
    Value,
}

pub(crate) struct FlagProbe {
    pub(crate) name: &'static str,
    pub(crate) kind: Kind,
    /// 值型旗标的合法取值（参数层必须接受它）；布尔旗标为 None
    pub(crate) sample: Option<&'static str>,
    /// 确实接受该旗标的命令。集成测试跑 `<CMD> <flag> --help`：退出 0 同时证明
    /// 命令名真实存在，且该旗标在这个命令的白名单里
    pub(crate) command: &'static str,
}

pub(crate) const PROBES: &[FlagProbe] = &[
    FlagProbe { name: "--align", kind: Kind::Value, sample: Some("mib"), command: "move" },
    FlagProbe { name: "--allow-move", kind: Kind::Bool, sample: None, command: "resize" },
    FlagProbe { name: "--chunk", kind: Kind::Value, sample: Some("4"), command: "move" },
    FlagProbe { name: "--discard-replay", kind: Kind::Bool, sample: None, command: "abandon" },
    FlagProbe { name: "--end", kind: Kind::Value, sample: Some("4096"), command: "resize-part" },
    FlagProbe { name: "--grow-lv", kind: Kind::Bool, sample: None, command: "resize" },
    FlagProbe { name: "--grow", kind: Kind::Bool, sample: None, command: "plan" },
    FlagProbe { name: "--grow-to-end", kind: Kind::Bool, sample: None, command: "resize-part" },
    FlagProbe { name: "--keep-partuuid", kind: Kind::Bool, sample: None, command: "copy" },
    FlagProbe { name: "--label", kind: Kind::Value, sample: Some("data"), command: "mkfs" },
    FlagProbe { name: "--lv", kind: Kind::Value, sample: Some("vg/lv"), command: "resize" },
    FlagProbe { name: "--name", kind: Kind::Value, sample: Some("n"), command: "add" },
    FlagProbe { name: "--no-fs", kind: Kind::Bool, sample: None, command: "resize" },
    FlagProbe { name: "--overwrite-data", kind: Kind::Bool, sample: None, command: "new" },
    FlagProbe { name: "--part-number", kind: Kind::Value, sample: Some("2"), command: "add" },
    FlagProbe { name: "--random", kind: Kind::Bool, sample: None, command: "set" },
    FlagProbe { name: "--replay-table-only", kind: Kind::Bool, sample: None, command: "undo" },
    FlagProbe { name: "--sector-size", kind: Kind::Value, sample: Some("4096"), command: "info" },
    FlagProbe { name: "--start", kind: Kind::Value, sample: Some("2048"), command: "move" },
    FlagProbe { name: "--table", kind: Kind::Value, sample: Some("gpt"), command: "new" },
    FlagProbe {
        name: "--type-guid",
        kind: Kind::Value,
        sample: Some("0fc63daf-8483-4772-8e79-3d69d8477de4"),
        command: "add",
    },
    FlagProbe { name: "--yes", kind: Kind::Bool, sample: None, command: "delete" },
];
