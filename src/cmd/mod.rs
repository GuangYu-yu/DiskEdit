//! 命令实现：每条命令的参数消费、逻辑与帮助文本同处一个模块。

pub(crate) mod fs;
pub(crate) mod info;
pub(crate) mod layout;
pub(crate) mod plan;
pub(crate) mod resize;
pub(crate) mod undo;

use crate::args::{help_cmd, usage, Args};

/// 一条命令的全部声明。名字/别名、旗标白名单、详助文本、与目标盘的打开关系、处理函数
/// **同出一源**：分派、`help <CMD>` 路由、旗标消费对账、成功后的 journal 清理
/// 都从这张表派生。
///
/// 若这几处各留一份命令清单，"改了分派忘了白名单"会让命令静默接受并忽略一个旗标，
/// 而"忘了 journal 清理"会留下撤销窗口。同一契约只有一处可改，才不会有第三处漏改
pub(crate) struct CommandSpec {
    /// 主名：也是 `help <CMD>` 的主题与旗标对账的键
    pub(crate) name: &'static str,
    /// 等价写法（分派与 help 主题同样接受）
    pub(crate) aliases: &'static [&'static str],
    /// 命令层会读到的旗标；未列出的旗标一律拒绝（fail-closed）。
    /// 静默忽略用户显式给出的旗标是最危险的参数漂移：命令做了旗标明确排除的事却报成功
    pub(crate) flags: &'static [&'static str],
    /// 详助正文（`diskedit help <CMD>` / `<CMD> --help`）
    pub(crate) help: &'static str,
    /// 与目标盘的打开关系（见 `TargetMode`）；main 据此决定成功后是否关闭撤销窗口
    pub(crate) mode: TargetMode,
    /// 处理函数。收命令名是因为 plan/apply 共用一个实现、要按名字分干跑与执行
    pub(crate) run: fn(&str, &Args) -> u8,
}

/// 命令与目标盘的打开关系。`opens_undo_journal` 由它派生，没有第二个声明。
/// 若按行为命名（"写没写盘"）会撒谎——mkfs 与 undo 都会写目标盘，但它们不建 journal
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TargetMode {
    /// 只读打开（`open_target_ro`）：不写目标盘
    ReadOnly,
    /// 读写打开（`open_target`）：写目标盘，但撤销窗口由调用方自理
    WriteNoJournal,
    /// 读写打开并建 journal（`open_target_for_write`）：成功后须关闭撤销窗口
    WriteJournal,
}

pub(crate) const COMMANDS: &[CommandSpec] = &[
    CommandSpec {
        name: "info",
        aliases: &[],
        flags: &["--sector-size"],
        help: info::HELP,
        mode: TargetMode::ReadOnly,
        run: |_, a| info::cmd_info(a),
    },
    CommandSpec {
        name: "resize",
        aliases: &[],
        // --start 在此不是"被消费的旗标"而是"必须明确拒绝的旗标"：声明它，
        // 请求才会走到 resize 自己那条"改大小不挪位，用 move/resize-part"的提示，
        // 而不是被白名单拦成一句笼统的"不是本命令的选项"
        flags: &[
            "--sector-size", "--size", "--grow-to-end", "--no-fs", "--grow-lv", "--lv", "--yes",
            "--allow-move", "--chunk-size", "--start",
        ],
        help: resize::HELP,
        mode: TargetMode::WriteJournal,
        run: |_, a| resize::cmd_resize(a),
    },
    CommandSpec {
        name: "move",
        aliases: &[],
        flags: &["--sector-size", "--start", "--align", "--chunk-size", "--no-fs"],
        help: layout::HELP_MOVE,
        mode: TargetMode::WriteJournal,
        run: |_, a| layout::cmd_move(a),
    },
    CommandSpec {
        name: "copy",
        aliases: &[],
        flags: &["--sector-size", "--start", "--align", "--chunk-size", "--name"],
        help: layout::HELP_COPY,
        mode: TargetMode::WriteJournal,
        run: |_, a| layout::cmd_copy(a),
    },
    CommandSpec {
        name: "create",
        aliases: &[],
        flags: &["--sector-size", "--size", "--fs", "--name"],
        help: layout::HELP_CREATE,
        mode: TargetMode::WriteJournal,
        run: |_, a| layout::cmd_create(a),
    },
    CommandSpec {
        name: "delete",
        aliases: &["del"],
        flags: &["--sector-size", "--yes"],
        help: layout::HELP_DELETE,
        mode: TargetMode::WriteJournal,
        run: |_, a| layout::cmd_del(a),
    },
    CommandSpec {
        name: "set",
        aliases: &[],
        flags: &["--sector-size", "--random"],
        help: fs::HELP_SET,
        mode: TargetMode::WriteJournal,
        run: |_, a| fs::cmd_set(a),
    },
    CommandSpec {
        name: "check",
        aliases: &[],
        flags: &["--sector-size"],
        // check 把分区交给外部工具，自己不动分区表；不需要撤销窗口
        help: fs::HELP_CHECK,
        mode: TargetMode::WriteNoJournal,
        run: |_, a| fs::cmd_check(a),
    },
    // mkfs 破坏的是分区内容而非分区表，走 open_target（不建 journal）：
    // 它若也进撤销窗口，成功时会顺手清掉先前某次失败操作留下的 journal
    CommandSpec {
        name: "mkfs",
        aliases: &[],
        flags: &["--sector-size", "--yes"],
        help: fs::HELP_MKFS,
        mode: TargetMode::WriteNoJournal,
        run: |_, a| fs::cmd_mkfs(a),
    },
    CommandSpec {
        name: "resizefs",
        aliases: &[],
        flags: &["--sector-size", "--online", "--size"],
        help: fs::HELP_RESIZEFS,
        mode: TargetMode::WriteNoJournal,
        run: |_, a| fs::cmd_resizefs(a),
    },
    CommandSpec {
        name: "undo",
        aliases: &[],
        flags: &["--sector-size", "--yes"],
        help: undo::HELP,
        mode: TargetMode::WriteNoJournal,
        run: |_, a| undo::cmd_undo(a),
    },
    CommandSpec {
        name: "new",
        aliases: &[],
        flags: &["--sector-size", "--table", "--yes"],
        help: layout::HELP_NEW,
        mode: TargetMode::WriteJournal,
        run: |_, a| layout::cmd_new(a),
    },
    CommandSpec {
        name: "add",
        aliases: &[],
        flags: &["--sector-size", "--start", "--end", "--align", "--name", "--type"],
        help: layout::HELP_ADD,
        mode: TargetMode::WriteJournal,
        run: |_, a| layout::cmd_add(a),
    },
    CommandSpec {
        name: "resize-part",
        aliases: &[],
        flags: &["--sector-size", "--start", "--end", "--grow-to-end", "--align", "--chunk-size", "--no-fs"],
        help: layout::HELP_RESIZE_PART,
        mode: TargetMode::WriteJournal,
        run: |_, a| layout::cmd_resize_part(a),
    },
    CommandSpec {
        name: "plan",
        aliases: &[],
        flags: &["--sector-size", "--grow"],
        help: plan::HELP,
        mode: TargetMode::ReadOnly,
        run: plan::cmd_plan_apply,
    },
    CommandSpec {
        name: "apply",
        aliases: &[],
        flags: &["--sector-size", "--grow", "--chunk-size", "--no-fs", "--yes"],
        help: plan::HELP,
        mode: TargetMode::WriteJournal,
        run: plan::cmd_plan_apply,
    },
];

/// 按主名或别名查表
pub(crate) fn find(name: &str) -> Option<&'static CommandSpec> {
    COMMANDS.iter().find(|c| c.name == name || c.aliases.contains(&name))
}

/// `diskedit help <主题>` 的文本路由；未知主题返回 None
pub(crate) fn help_text(name: &str) -> Option<&'static str> {
    find(name).map(|c| c.help).filter(|h| !h.is_empty())
}

/// 该命令声明消费的旗标（fail-closed 白名单）；未声明的命令没有白名单
pub(crate) fn flags_for(name: &str) -> Option<&'static [&'static str]> {
    find(name).map(|c| c.flags)
}

/// 该命令是否开了 undo journal（因而成功时需关闭撤销窗口）。由 `CommandSpec::mode` 派生
pub(crate) fn opens_undo_journal(name: &str) -> bool {
    find(name).is_some_and(|c| c.mode == TargetMode::WriteJournal)
}

/// 命令分派。usage/help_cmd 不返回（直接退出进程）
pub(crate) fn dispatch(cmd: &str, a: &Args) -> u8 {
    match cmd {
        "help" | "--help" | "-h" => match a.pos.first() {
            Some(t) => help_cmd(t),
            None => usage(),
        },
        _ => match find(cmd) {
            Some(c) => (c.run)(cmd, a),
            None => usage(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 命令表的自洽：名字与别名互不重复；每条详助都写明自己的名字。
    /// 详助正文无法从表里机械生成，但"主题 → 文本"的路由是按名字走的，
    /// 文本里写着别的命令就是帮助漂移，这条对账把它变成构建期错误
    #[test]
    fn command_table_is_self_consistent() {
        let mut seen = std::collections::HashSet::new();
        for c in COMMANDS {
            assert!(seen.insert(c.name), "duplicate command name {}", c.name);
            for a in c.aliases {
                assert!(seen.insert(a), "duplicate alias {a}");
            }
            assert!(!c.help.is_empty(), "{}: missing help text", c.name);
            assert!(
                c.help.contains(&format!("diskedit {}", c.name)),
                "{}: help text must name the command",
                c.name
            );
            for f in c.flags {
                assert!(f.starts_with("--"), "{}: {f} is not a long option", c.name);
            }
        }
    }

    /// 别名解析回主名；白名单是 fail-closed 的——未声明的命令没有白名单，
    /// resize 声明 --start 只为走到它自己的针对性拒绝（见 cmd::resize）
    #[test]
    fn aliases_and_flag_whitelists() {
        assert_eq!(find("del").map(|c| c.name), Some("delete"));
        assert!(flags_for("resize").unwrap().contains(&"--start"));
        assert!(!flags_for("resize").unwrap().contains(&"--align"));
        assert!(flags_for("move").unwrap().contains(&"--start"));
        assert_eq!(flags_for("nosuchcmd"), None);
        assert!(opens_undo_journal("apply") && !opens_undo_journal("plan"));
        // undo 与 mkfs 不经 open_target_for_write：会把已存在的 journal 误清掉
        assert!(!opens_undo_journal("undo") && !opens_undo_journal("mkfs"));
    }

    /// 每条命令与撤销窗口的关系都是逐条裁定过的：这里把结论钉住——
    /// 表中漏一条（新命令随手选了个 mode）与改了 mode 同样报错：
    /// 错了的代价是静默留下一个撤销窗口，或静默抹掉别人的撤销窗口
    #[test]
    fn journal_modes_are_as_reviewed() {
        use TargetMode::{ReadOnly, WriteJournal, WriteNoJournal};
        let expected: &[(&str, TargetMode)] = &[
            ("info", ReadOnly),
            ("resize", WriteJournal),
            ("move", WriteJournal),
            ("copy", WriteJournal),
            ("create", WriteJournal),
            ("delete", WriteJournal),
            ("set", WriteJournal),
            ("check", WriteNoJournal),
            ("mkfs", WriteNoJournal),
            ("resizefs", WriteNoJournal),
            ("undo", WriteNoJournal),
            ("new", WriteJournal),
            ("add", WriteJournal),
            ("resize-part", WriteJournal),
            ("plan", ReadOnly),
            ("apply", WriteJournal),
        ];
        for (name, mode) in expected {
            let c = find(name).unwrap_or_else(|| panic!("{name}: not a command"));
            assert_eq!(c.mode, *mode, "{name}: journal mode changed");
        }
        for c in COMMANDS {
            assert!(
                expected.iter().any(|(n, _)| *n == c.name),
                "{}: no expected journal mode — decide it and add it here",
                c.name
            );
        }
    }
}