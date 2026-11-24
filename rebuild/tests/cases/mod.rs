//! 用例表：每条用例只描述输入与可观察结果，不复制实现细节。

/// 一条命令行用例。
pub struct Case {
    pub name: &'static str,
    pub argv: &'static [&'static str],
    /// 进程退出码。
    pub exit: u8,
    /// stdout 必须包含的片段；空表示不检查。
    pub stdout: &'static str,
    /// stderr 必须包含的片段；空表示不检查。
    pub stderr: &'static str,
}

pub const CASES: &[Case] = &[
    Case {
        name: "top-level help",
        argv: &["--help"],
        exit: 0,
        stdout: "diskedit",
        stderr: "",
    },
    Case {
        name: "short help flag",
        argv: &["-h"],
        exit: 0,
        stdout: "diskedit",
        stderr: "",
    },
    Case {
        name: "help subcommand",
        argv: &["help"],
        exit: 0,
        stdout: "diskedit",
        stderr: "",
    },
    Case {
        name: "version",
        argv: &["--version"],
        exit: 0,
        stdout: "diskedit",
        stderr: "",
    },
    Case {
        name: "no arguments is a usage error",
        argv: &[],
        exit: 2,
        stdout: "",
        stderr: "diskedit",
    },
    Case {
        name: "unknown command is a usage error",
        argv: &["bogus"],
        exit: 2,
        stdout: "",
        stderr: "diskedit",
    },
    Case {
        name: "help for one command",
        argv: &["help", "info"],
        exit: 0,
        stdout: "diskedit info",
        stderr: "",
    },
    Case {
        name: "help for a two-word command",
        argv: &["help", "records", "list"],
        exit: 0,
        stdout: "diskedit records list",
        stderr: "",
    },
    Case {
        name: "command help flag",
        argv: &["info", "--help"],
        exit: 0,
        stdout: "diskedit info",
        stderr: "",
    },
    Case {
        name: "unknown help topic is a usage error",
        argv: &["help", "bogus"],
        exit: 2,
        stdout: "",
        stderr: "diskedit",
    },
    Case {
        name: "incomplete help topic is a usage error",
        argv: &["help", "records"],
        exit: 2,
        stdout: "",
        stderr: "diskedit",
    },
];
