//! 命令实现：每条命令的参数消费、逻辑与帮助文本同处一个模块。

pub(crate) mod abandon;
pub(crate) mod fs;
pub(crate) mod info;
pub(crate) mod layout;
pub(crate) mod plan;
pub(crate) mod resize;
pub(crate) mod undo;

use crate::args::{help_cmd, refuse_unconsumed_flags, usage, usage_help, Args};
use crate::outcome::{Outcome, EXIT_OK};
use crate::support::{bail_fail, Fail};

/// 命令分派的结果。退出码之外还要保住"撤销窗口能不能关"这一事实：
/// 20 既可能来自"布局已写、待办非空"，也可能来自"布局已写、仅内核视图过期"，
/// 两者对 journal 的去留结论相反，压成 u8 之后再也分不出来
pub(crate) enum CmdResult {
    /// 命令持有可判定的后置条件契约（布局写入及其后续链）
    Outcome(Outcome),
    /// 没有布局事务可判定：按退出码处理
    Code(u8),
}

impl CmdResult {
    pub(crate) fn code(&self) -> u8 {
        match self {
            Self::Outcome(o) => o.exit_code(),
            Self::Code(c) => *c,
        }
    }

    /// 布局写入已完成且没有已知待办 ⇒ 撤销窗口可以关闭。
    /// 待办非空时保留 journal：它是 `undo` 撤销本次表写入的唯一凭据
    /// （`new --mkfs` 的 PARTIAL 契约，见 tests/cli/journal.rs）
    pub(crate) fn closes_journal(&self) -> bool {
        match self {
            Self::Outcome(o) => matches!(o, Outcome::Applied { pending, .. } if pending.is_empty()),
            Self::Code(code) => *code == EXIT_OK,
        }
    }
}

impl From<Outcome> for CmdResult {
    fn from(o: Outcome) -> Self {
        Self::Outcome(o)
    }
}

impl From<u8> for CmdResult {
    fn from(code: u8) -> Self {
        Self::Code(code)
    }
}

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
    /// 位置参数上限（含第 0 位的目标路径）。与旗标同罪：多给即拒绝——
    /// 静默忽略多余位置参数会让用户以为写到了别处（如把 SIZE 落在错位上）却报成功
    pub(crate) max_pos: usize,
    /// 撤销窗口的生命周期（见 `TargetMode`）：main 据此决定成功后是否关闭它
    pub(crate) mode: TargetMode,
    /// 处理函数。收命令名是因为 plan/apply 共用一个实现、要按名字分干跑与执行。
    /// 返回 `CmdResult` 而不是裸退出码：main 要按"已应用且无待办"决定是否关闭
    /// 撤销窗口，而码值 20 分不出待办非空与仅内核视图过期
    pub(crate) run: fn(&str, &Args) -> CmdResult,
}

/// 命令与撤销窗口的关系：唯一的消费者是 `opens_undo_journal`（main 据此决定成功后
/// 是否删 journal），不描述每个命令实际怎样打开目标——`abandon` 归 `WriteNoJournal`，
/// 而它基本不打开目标。若按行为命名（"写没写盘"）会撒谎：mkfs 与 undo 都写目标盘，
/// 但都不建 journal
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TargetMode {
    /// 不参与撤销窗口：只读命令，成功时没有要关闭的窗口
    ReadOnly,
    /// 写盘，但撤销窗口由命令自己处理（undo 回放既有 journal；check/resizefs 动手前
    /// 过闸口）；成功后不得替别的失败运行清掉 journal
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
        max_pos: 1,
        mode: TargetMode::ReadOnly,
        run: |_, a| info::cmd_info(a).into(),
    },
    CommandSpec {
        name: "resize",
        aliases: &[],
        // --start/--end 在此不是"被消费的旗标"而是"必须明确拒绝的旗标"：声明它们，
        // 请求才会走到 resize 自己那条"改大小不挪位，用 move/resize-part"的提示，
        // 而不是被白名单拦成一句笼统的"不是本命令的选项"。--start 与 --end 是同一
        // 语义（挪位）的两半，缺一个另一半就会先死在白名单上
        flags: &[
            "--sector-size", "--size", "--grow-to-end", "--no-fs", "--grow-lv", "--lv", "--yes",
            "--allow-move", "--chunk-size", "--start", "--end",
        ],
        help: resize::HELP,
        max_pos: 2, // TARGET:N + SIZE
        mode: TargetMode::WriteJournal,
        run: |_, a| resize::cmd_resize(a).into(),
    },
    CommandSpec {
        name: "move",
        aliases: &[],
        flags: &["--sector-size", "--start", "--align", "--chunk-size", "--no-fs"],
        help: layout::HELP_MOVE,
        max_pos: 1,
        mode: TargetMode::WriteJournal,
        run: |_, a| layout::cmd_move(a).into(),
    },
    CommandSpec {
        name: "copy",
        aliases: &[],
        flags: &["--sector-size", "--start", "--align", "--chunk-size", "--name", "--clone"],
        help: layout::HELP_COPY,
        max_pos: 1,
        mode: TargetMode::WriteJournal,
        run: |_, a| layout::cmd_copy(a).into(),
    },
    CommandSpec {
        name: "create",
        aliases: &[],
        flags: &["--sector-size", "--size", "--fs", "--name"],
        help: layout::HELP_CREATE,
        max_pos: 1,
        mode: TargetMode::WriteJournal,
        run: |_, a| layout::cmd_create(a).into(),
    },
    CommandSpec {
        name: "delete",
        aliases: &["del"],
        flags: &["--sector-size", "--yes"],
        help: layout::HELP_DELETE,
        max_pos: 1,
        mode: TargetMode::WriteJournal,
        run: |_, a| layout::cmd_del(a).into(),
    },
    CommandSpec {
        name: "set",
        aliases: &[],
        flags: &["--sector-size", "--random"],
        help: fs::HELP_SET,
        max_pos: 4, // TARGET:N + key + value + state
        mode: TargetMode::WriteJournal,
        run: |_, a| fs::cmd_set(a).into(),
    },
    CommandSpec {
        name: "check",
        aliases: &[],
        flags: &["--sector-size"],
        // check 把分区交给外部工具，自己不动分区表；不需要撤销窗口
        help: fs::HELP_CHECK,
        max_pos: 1,
        mode: TargetMode::WriteNoJournal,
        run: |_, a| fs::cmd_check(a).into(),
    },
    // mkfs 是一次真正的**事务**（不可回滚，但要走完整的 begin/commit）：它先落一条屏障
    // 说明"这个分区上创建了文件系统"，成功即关闭事务，失败/崩溃则留下待 abandon 的现场。
    // 之所以能安全地进撤销窗口：`begin` 在目标被别的事务占用时就会拒绝，所以它开自己的
    // 撤销窗口时，目标上不会还留着别人的现场
    CommandSpec {
        name: "mkfs",
        aliases: &[],
        flags: &["--sector-size", "--yes"],
        help: fs::HELP_MKFS,
        max_pos: 2, // TARGET:N + FSTYPE
        mode: TargetMode::WriteJournal,
        run: |_, a| fs::cmd_mkfs(a).into(),
    },
    CommandSpec {
        name: "resizefs",
        aliases: &[],
        flags: &["--sector-size", "--online", "--size"],
        help: fs::HELP_RESIZEFS,
        max_pos: 2, // MOUNTPOINT + BYTES
        mode: TargetMode::WriteNoJournal,
        run: |_, a| fs::cmd_resizefs(a).into(),
    },
    CommandSpec {
        name: "undo",
        aliases: &[],
        flags: &["--sector-size", "--yes"],
        help: undo::HELP,
        max_pos: 1,
        mode: TargetMode::WriteNoJournal,
        run: |_, a| undo::cmd_undo(a).into(),
    },
    // abandon 既不建 journal、也不回放：它把现场字节原样改名交出去，盘上一个字节都不写。
    // 打开是只读的且不走 --sector-size（扇区大小对"改名交出去"无意义），白名单不收。
    // 归 WriteNoJournal 的后果正是要的——成功时不触发 main 的 journal 清理
    CommandSpec {
        name: "abandon",
        aliases: &[],
        flags: &["--yes"],
        help: abandon::HELP,
        max_pos: 1,
        mode: TargetMode::WriteNoJournal,
        run: |_, a| abandon::cmd_abandon(a).into(),
    },
    CommandSpec {
        name: "new",
        aliases: &[],
        flags: &["--sector-size", "--table", "--yes"],
        help: layout::HELP_NEW,
        max_pos: 1,
        mode: TargetMode::WriteJournal,
        run: |_, a| layout::cmd_new(a).into(),
    },
    CommandSpec {
        name: "add",
        aliases: &[],
        flags: &["--sector-size", "--start", "--end", "--align", "--name", "--type"],
        help: layout::HELP_ADD,
        max_pos: 1,
        mode: TargetMode::WriteJournal,
        run: |_, a| layout::cmd_add(a).into(),
    },
    CommandSpec {
        name: "resize-part",
        aliases: &[],
        flags: &["--sector-size", "--start", "--end", "--grow-to-end", "--align", "--chunk-size", "--no-fs"],
        help: layout::HELP_RESIZE_PART,
        max_pos: 1,
        mode: TargetMode::WriteJournal,
        run: |_, a| layout::cmd_resize_part(a).into(),
    },
    CommandSpec {
        name: "plan",
        aliases: &[],
        flags: &["--sector-size", "--grow"],
        help: plan::HELP,
        max_pos: 1,
        mode: TargetMode::ReadOnly,
        run: plan::cmd_plan_apply,
    },
    CommandSpec {
        name: "apply",
        aliases: &[],
        // 无 --yes：apply 与 plan 是同一件事的两面——plan 干跑不触盘，apply 在**锁下**
        // 按盘上现状重新推导计划后执行（见 cmd::plan），没有第二个确认层；
        // 白名单声明却不消费的旗标 = 静默接受用户以为存在的确认
        flags: &["--sector-size", "--grow", "--chunk-size", "--no-fs"],
        help: plan::HELP,
        max_pos: 1,
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
pub(crate) fn dispatch(cmd: &str, a: &Args) -> CmdResult {
    match cmd {
        "help" | "--help" | "-h" => match a.pos.len() {
            // 主动请求帮助，非用法错误：打全文退 0（未知命令/缺参数才是 10）
            0 => usage_help(),
            // 命令位本身就是 help 的写法（`help <CMD>` / `--help <CMD>`）不走
            // parse_args 的旗标对账，这里补同一份白名单：帮助与执行看到同一套判据
            1 => {
                refuse_unconsumed_flags(&a.pos[0], a);
                help_cmd(&a.pos[0])
            }
            _ => bail_fail(Fail::refused(format!(
                "unexpected extra argument for `help`: {} (see `diskedit help`)",
                a.pos[1]
            ))),
        },
        _ => match find(cmd) {
            Some(c) => {
                // 位置参数与旗标同一对账：多出的第 max_pos 位即拒绝，不落命令函数
                if let Some(extra) = a.pos.get(c.max_pos) {
                    bail_fail(Fail::refused(format!(
                        "unexpected extra argument for `{cmd}`: {extra} (see `diskedit help {cmd}`)"
                    )));
                }
                (c.run)(cmd, a)
            }
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
            assert!(c.max_pos >= 1, "{}: max_pos must count the target", c.name);
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
        // undo 不经 open_target_for_write：它要回放的是已存在的那份 journal
        assert!(!opens_undo_journal("undo"));
        // mkfs 相反：它自己就是一次事务，成功时正该关闭自己的撤销窗口
        assert!(opens_undo_journal("mkfs"));
    }

    /// 撤销窗口的关闭判据是"布局已应用且没有已知待办"，不是退出码 0。20 有两个
    /// 正交成因：待办非空（保留 journal 供 undo 撤销表写入）与仅内核视图过期
    /// （事务已完成，必须关闭——否则下一次写命令会被自己残留的 journal 挡成 busy）
    #[test]
    fn journal_closes_only_when_applied_without_pending() {
        use crate::outcome::{EXIT_OK, EXIT_PARTIAL, KernelSync, Outcome, Pending, PendingKind};

        let complete = CmdResult::from(Outcome::applied_with(Vec::new()));
        assert!(complete.closes_journal());
        assert_eq!(complete.code(), EXIT_OK);

        let pending = CmdResult::from(Outcome::applied_with(vec![Pending::new(
            1, PendingKind::Fs, "detail", "hint",
        )]));
        assert!(!pending.closes_journal());
        assert_eq!(pending.code(), EXIT_PARTIAL);

        let stale = CmdResult::from(Outcome::Applied {
            pending: Vec::new(),
            kernel_sync: KernelSync::Stale,
        });
        assert!(stale.closes_journal());
        assert_eq!(stale.code(), EXIT_PARTIAL);

        // 不开事务的命令只带退出码：0 是它自己的成功，与撤销窗口无关
        assert!(CmdResult::Code(EXIT_OK).closes_journal());
        assert!(!CmdResult::Code(EXIT_PARTIAL).closes_journal());

        assert!(!CmdResult::from(Outcome::refused("no")).closes_journal());
        assert!(!CmdResult::from(Outcome::failed("boom")).closes_journal());
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
            ("mkfs", WriteJournal),
            ("resizefs", WriteNoJournal),
            ("undo", WriteNoJournal),
            ("abandon", WriteNoJournal),
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