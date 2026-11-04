//! 命令实现：每条命令的参数消费、逻辑与帮助文本同处一个模块。

pub(crate) mod abandon;
pub(crate) mod confirm;
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

/// 一条命令的全部声明。名字、旗标白名单、详助文本、与目标盘的打开关系、处理函数
/// **同出一源**：分派、`help <CMD>` 路由、旗标消费对账、成功后的 journal 清理
/// 都从这张表派生。
///
/// 若这几处各留一份命令清单，"改了分派忘了白名单"会让命令静默接受并忽略一个旗标，
/// 而"忘了 journal 清理"会留下撤销窗口。同一契约只有一处可改，才不会有第三处漏改
pub(crate) struct CommandSpec {
    /// 命令名：也是 `help <CMD>` 的主题与旗标对账的键
    pub(crate) name: &'static str,
    /// 允许进入该命令解析流程的旗标；未列出的旗标一律拒绝（fail-closed）。
    /// 列入不等于"会被采纳"：`resize` 的 `--start`/`--end` 列在这里只为走到它自己
    /// 那条针对性拒绝。静默忽略用户显式给出的旗标是最危险的参数漂移：命令做了
    /// 旗标明确排除的事却报成功
    pub(crate) flags: &'static [&'static str],
    /// 详助正文（`diskedit help <CMD>` / `<CMD> --help`）
    pub(crate) help: &'static str,
    /// 位置参数上限（含第 0 位的目标路径）。与旗标同罪：多给即拒绝——
    /// 静默忽略多余位置参数会让用户以为写到了别处（如把 SIZE 落在错位上）却报成功
    pub(crate) max_pos: usize,
    /// 撤销窗口的生命周期（见 `TargetMode`）：main 据此决定成功后是否关闭它
    pub(crate) mode: TargetMode,
    /// 本命令需要的副作用能力。由能力门在**命令函数之前**判定，不交给各命令自行守门
    pub(crate) capabilities: &'static [Capability],
    /// SIZE 落在 `pos` 的哪个下标。以 `-` 开头的相对尺寸只在这个槽位被接受：
    /// 放宽到任何位置，`info -10` 这种旗标笔误会变成一次"打开失败"而不是用法拒绝
    pub(crate) size_pos: Option<usize>,
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

/// 一条命令会造成的副作用类别：能力门据此执行平台策略。
///
/// 分派表里**每条命令都必须显式声明**；空声明不是"无需副作用"，而是漏声明，
/// 能力门把它与超出平台保证的声明同样拒绝（fail-closed）。
///
/// 分类按"谁动手"而不是"动什么"：一条命令可以同时落入几档（resize 既改盘面字节、
/// 又建 journal、又启动 resize2fs、又改 loop 分区设备）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Capability {
    /// 纯读：只经只读句柄读盘面与元数据，不取锁、不建伴随文件、不启动外部工具
    ReadOnly,
    /// 改盘面字节（分区表与数据区都算）
    TargetData,
    /// 建/改/删伴随文件：lock、journal、checkpoint、log
    CompanionFile,
    /// 启动会写盘的外部程序：mkfs、mount、resize2fs、fsadm、btrfs、fsck、LVM 工具等
    ExternalTool,
    /// 改设备状态：loop attach/detach、BLKPG、BLKRRPART 等
    DeviceState,
}

pub(crate) const COMMANDS: &[CommandSpec] = &[
    CommandSpec {
        name: "info",
        flags: &["--sector-size"],
        help: info::HELP,
        max_pos: 1,
        mode: TargetMode::ReadOnly,
        capabilities: &[Capability::ReadOnly],
        size_pos: None,
        run: |_, a| info::cmd_info(a).into(),
    },
    CommandSpec {
        name: "resize",
        // --start/--end 在此不是"被消费的旗标"而是"必须明确拒绝的旗标"：声明它们，
        // 请求才会走到 resize 自己那条"改大小不挪位，用 move/resize-part"的提示，
        // 而不是被白名单拦成一句笼统的"不是本命令的选项"。--start 与 --end 是同一
        // 语义（挪位）的两半，缺一个另一半就会先死在白名单上
        flags: &[
            "--sector-size", "--no-fs", "--grow-lv", "--lv", "--yes",
            "--allow-move", "--chunk-size", "--start", "--end",
        ],
        help: resize::HELP,
        max_pos: 2, // TARGET:N + SIZE
        mode: TargetMode::WriteJournal,
        capabilities: &[
            Capability::TargetData,
            Capability::CompanionFile,
            Capability::ExternalTool,
            Capability::DeviceState,
        ],
        size_pos: Some(1), // TARGET:N + SIZE
        run: |_, a| resize::cmd_resize(a).into(),
    },
    CommandSpec {
        name: "move",
        flags: &["--sector-size", "--start", "--align", "--chunk-size", "--no-fs", "--yes"],
        help: layout::HELP_MOVE,
        max_pos: 1,
        mode: TargetMode::WriteJournal,
        capabilities: &[
            Capability::TargetData,
            Capability::CompanionFile,
            Capability::ExternalTool,
            Capability::DeviceState,
        ],
        size_pos: None,
        run: |_, a| layout::cmd_move(a).into(),
    },
    CommandSpec {
        name: "copy",
        flags: &["--sector-size", "--start", "--align", "--chunk-size", "--name", "--keep-partuuid", "--yes"],
        help: layout::HELP_COPY,
        max_pos: 1,
        mode: TargetMode::WriteJournal,
        capabilities: &[Capability::TargetData, Capability::CompanionFile, Capability::DeviceState],
        size_pos: None,
        run: |_, a| layout::cmd_copy(a).into(),
    },
    CommandSpec {
        name: "create",
        flags: &["--sector-size", "--size", "--fs", "--name", "--yes"],
        help: layout::HELP_CREATE,
        max_pos: 1,
        mode: TargetMode::WriteJournal,
        capabilities: &[
            Capability::TargetData,
            Capability::CompanionFile,
            Capability::ExternalTool,
            Capability::DeviceState,
        ],
        size_pos: None,
        run: |_, a| layout::cmd_create(a).into(),
    },
    CommandSpec {
        name: "delete",
        flags: &["--sector-size", "--yes"],
        help: layout::HELP_DELETE,
        max_pos: 1,
        mode: TargetMode::WriteJournal,
        capabilities: &[Capability::TargetData, Capability::CompanionFile, Capability::DeviceState],
        size_pos: None,
        run: |_, a| layout::cmd_delete(a).into(),
    },
    CommandSpec {
        name: "set",
        flags: &["--sector-size", "--random"],
        help: fs::HELP_SET,
        max_pos: 4, // TARGET:N + key + value + state
        mode: TargetMode::WriteJournal,
        capabilities: &[
            Capability::TargetData,
            Capability::CompanionFile,
            Capability::ExternalTool,
            Capability::DeviceState,
        ],
        size_pos: None,
        run: |_, a| fs::cmd_set(a).into(),
    },
    CommandSpec {
        name: "check",
        flags: &["--sector-size"],
        // check 把分区交给外部工具，自己不动分区表；不需要撤销窗口
        help: fs::HELP_CHECK,
        max_pos: 1,
        mode: TargetMode::WriteNoJournal,
        capabilities: &[Capability::CompanionFile, Capability::ExternalTool, Capability::DeviceState],
        size_pos: None,
        run: |_, a| fs::cmd_check(a).into(),
    },
    // mkfs 是一次真正的**事务**（不可回滚，但要走完整的 begin/commit）：它先落一条屏障
    // 说明"这个分区上创建了文件系统"，成功即关闭事务，失败/崩溃则留下待 abandon 的现场。
    // 之所以能安全地进撤销窗口：`begin` 在目标被别的事务占用时就会拒绝，所以它开自己的
    // 撤销窗口时，目标上不会还留着别人的现场
    CommandSpec {
        name: "mkfs",
        flags: &["--sector-size", "--yes"],
        help: fs::HELP_MKFS,
        max_pos: 2, // TARGET:N + FSTYPE
        mode: TargetMode::WriteJournal,
        capabilities: &[Capability::CompanionFile, Capability::ExternalTool, Capability::DeviceState],
        size_pos: None,
        run: |_, a| fs::cmd_mkfs(a).into(),
    },
    CommandSpec {
        name: "resizefs",
        flags: &["--sector-size"],
        help: fs::HELP_RESIZEFS,
        max_pos: 2, // MOUNTPOINT + BYTES
        mode: TargetMode::WriteNoJournal,
        capabilities: &[Capability::CompanionFile, Capability::ExternalTool, Capability::DeviceState],
        size_pos: Some(1), // MOUNTPOINT + BYTES
        run: |_, a| fs::cmd_resizefs(a).into(),
    },
    CommandSpec {
        name: "undo",
        flags: &["--sector-size", "--yes"],
        help: undo::HELP,
        max_pos: 1,
        mode: TargetMode::WriteNoJournal,
        capabilities: &[
            Capability::TargetData,
            Capability::CompanionFile,
            Capability::ExternalTool,
            Capability::DeviceState,
        ],
        size_pos: None,
        run: |_, a| undo::cmd_undo(a).into(),
    },
    // abandon 既不建 journal、也不回放：它把现场字节原样改名交出去，盘上一个字节都不写。
    // 打开是只读的且不走 --sector-size（扇区大小对"改名交出去"无意义），白名单不收。
    // 归 WriteNoJournal 的后果正是要的——成功时不触发 main 的 journal 清理
    CommandSpec {
        name: "abandon",
        flags: &["--yes"],
        help: abandon::HELP,
        max_pos: 1,
        mode: TargetMode::WriteNoJournal,
        // 只改伴随文件的名字与位置，盘面一个字节不写
        capabilities: &[Capability::CompanionFile],
        size_pos: None,
        run: |_, a| abandon::cmd_abandon(a).into(),
    },
    CommandSpec {
        name: "new",
        flags: &["--sector-size", "--table", "--yes", "--overwrite-partition-data"],
        help: layout::HELP_NEW,
        max_pos: 1,
        mode: TargetMode::WriteJournal,
        capabilities: &[Capability::TargetData, Capability::CompanionFile, Capability::DeviceState],
        size_pos: None,
        run: |_, a| layout::cmd_new(a).into(),
    },
    CommandSpec {
        name: "add",
        flags: &["--sector-size", "--start", "--end", "--align", "--name", "--type"],
        help: layout::HELP_ADD,
        max_pos: 1,
        mode: TargetMode::WriteJournal,
        capabilities: &[Capability::TargetData, Capability::CompanionFile, Capability::DeviceState],
        size_pos: None,
        run: |_, a| layout::cmd_add(a).into(),
    },
    CommandSpec {
        name: "resize-part",
        flags: &["--sector-size", "--start", "--end", "--grow-to-end", "--align", "--chunk-size", "--no-fs", "--yes"],
        help: layout::HELP_RESIZE_PART,
        max_pos: 1,
        mode: TargetMode::WriteJournal,
        capabilities: &[
            Capability::TargetData,
            Capability::CompanionFile,
            Capability::ExternalTool,
            Capability::DeviceState,
        ],
        size_pos: None,
        run: |_, a| layout::cmd_resize_part(a).into(),
    },
    CommandSpec {
        name: "plan",
        flags: &["--sector-size", "--grow"],
        help: plan::HELP_PLAN,
        max_pos: 1,
        mode: TargetMode::ReadOnly,
        // 走 open_target_ro：不取锁，也不建 ckpt/journal
        capabilities: &[Capability::ReadOnly],
        size_pos: None,
        run: plan::cmd_plan_apply,
    },
    CommandSpec {
        name: "apply",
        // --yes 只在几何推导出"挡路分区要真的搬数据"时才被消费（见 cmd::confirm）：
        // 空盘扩容不需要授权，白名单放行但确认门的判据为空
        flags: &["--sector-size", "--grow", "--chunk-size", "--no-fs", "--yes"],
        help: plan::HELP_APPLY,
        max_pos: 1,
        mode: TargetMode::WriteJournal,
        capabilities: &[
            Capability::TargetData,
            Capability::CompanionFile,
            Capability::ExternalTool,
            Capability::DeviceState,
        ],
        size_pos: None,
        run: plan::cmd_plan_apply,
    },
];

/// 按命令名查表
pub(crate) fn find(name: &str) -> Option<&'static CommandSpec> {
    COMMANDS.iter().find(|c| c.name == name)
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

/// 平台能力不足时的唯一文案。门在命令分派处、任何伴随文件创建之前拒绝，
/// 所以"没有写入"覆盖目标字节与 lock/journal/ckpt/log 全部落点
const UNSUPPORTED_WRITE: &str = "this platform does not support safe disk writes — run this on Linux. \
     Nothing was written (no target bytes, lock, journal, checkpoint, or log).";

/// 平台策略：Linux 是本工具承诺安全写盘的平台，放行全部能力；其他平台放行纯读与只改伴随
/// 文件的能力——`abandon` 只改名现场、盘面一个字节不写，且它正是 busy 文案给出的唯一出路，
/// 留下它就等于在那些平台上把目标锁死。
///
/// 非 Linux 的放行集合是**白名单**：能力必须显式出现在 `permitted` 里才放行，新命令或
/// 新能力不会被默认继承；名单本身由 `capability_gate_is_fail_closed` 逐条固定。
///
/// 空声明按漏声明处理并拒绝——能力门不替命令猜测"什么都没声明 = 什么都不做"
fn platform_allows(caps: &[Capability]) -> bool {
    let permitted: &[Capability] = if cfg!(target_os = "linux") {
        &[
            Capability::ReadOnly,
            Capability::TargetData,
            Capability::CompanionFile,
            Capability::ExternalTool,
            Capability::DeviceState,
        ]
    } else {
        &[Capability::ReadOnly, Capability::CompanionFile]
    };
    !caps.is_empty() && caps.iter().all(|c| permitted.contains(c))
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
                // 统一能力门：先于任何命令函数，因而也先于锁/日志/journal/checkpoint
                // 的一切创建。写命令在不承诺安全写盘的平台上在这里结束，不落到下层
                if !platform_allows(c.capabilities) {
                    bail_fail(Fail::infra(UNSUPPORTED_WRITE));
                }
                let result = (c.run)(cmd, a);
                // 开了撤销窗口的命令必须带回后置状态：只给退出码时 closes_journal
                // 退回"码是 0 即完成"，20 的待办非空与仅内核视图过期分不出来
                if c.mode == TargetMode::WriteJournal {
                    assert!(
                        !matches!(result, CmdResult::Code(_)),
                        "WriteJournal command `{cmd}` returned CmdResult::Code"
                    );
                }
                result
            }
            None => usage(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 命令表的自洽：名字互不重复；每条详助都写明自己的名字。
    /// 详助正文无法从表里机械生成，但"主题 → 文本"的路由是按名字走的，
    /// 文本里写着别的命令就是帮助漂移，这条对账把它变成构建期错误
    #[test]
    fn command_table_is_self_consistent() {
        let mut seen = std::collections::HashSet::new();
        for c in COMMANDS {
            assert!(seen.insert(c.name), "duplicate command name {}", c.name);
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

    /// 白名单是 fail-closed 的——未声明的命令没有白名单，
    /// resize 声明 --start 只为走到它自己的针对性拒绝（见 cmd::resize）
    #[test]
    fn flag_whitelists() {
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

    /// 每条命令的副作用能力都是逐条裁定过的：表中漏一条（新命令没写 capabilities）
    /// 与改了能力同样报错。空声明由 `capability_gate_is_fail_closed` 单独拒绝
    #[test]
    fn capabilities_are_as_reviewed() {
        use Capability::{CompanionFile, DeviceState, ExternalTool, ReadOnly, TargetData};
        let expected: &[(&str, &[Capability])] = &[
            ("info", &[ReadOnly]),
            ("resize", &[TargetData, CompanionFile, ExternalTool, DeviceState]),
            ("move", &[TargetData, CompanionFile, ExternalTool, DeviceState]),
            ("copy", &[TargetData, CompanionFile, DeviceState]),
            ("create", &[TargetData, CompanionFile, ExternalTool, DeviceState]),
            ("delete", &[TargetData, CompanionFile, DeviceState]),
            ("set", &[TargetData, CompanionFile, ExternalTool, DeviceState]),
            ("check", &[CompanionFile, ExternalTool, DeviceState]),
            ("mkfs", &[CompanionFile, ExternalTool, DeviceState]),
            ("resizefs", &[CompanionFile, ExternalTool, DeviceState]),
            ("undo", &[TargetData, CompanionFile, ExternalTool, DeviceState]),
            ("abandon", &[CompanionFile]),
            ("new", &[TargetData, CompanionFile, DeviceState]),
            ("add", &[TargetData, CompanionFile, DeviceState]),
            ("resize-part", &[TargetData, CompanionFile, ExternalTool, DeviceState]),
            ("plan", &[ReadOnly]),
            ("apply", &[TargetData, CompanionFile, ExternalTool, DeviceState]),
        ];
        for (name, caps) in expected {
            let c = find(name).unwrap_or_else(|| panic!("{name}: not a command"));
            assert_eq!(c.capabilities, *caps, "{name}: declared capabilities changed");
        }
        for c in COMMANDS {
            assert!(
                expected.iter().any(|(n, _)| *n == c.name),
                "{}: no expected capabilities — decide them and add them here",
                c.name
            );
        }
    }

    /// 能力门 fail-closed：空声明不是"无副作用"，不能被默认放行；非 Linux 上
    /// 只有纯读命令与只改伴随文件的 `abandon` 能过门
    #[test]
    fn capability_gate_is_fail_closed() {
        assert!(!platform_allows(&[]), "empty declaration must be denied, not default-allowed");
        assert!(platform_allows(&[Capability::ReadOnly]));
        assert_eq!(
            platform_allows(&[Capability::TargetData]),
            cfg!(target_os = "linux"),
            "only Linux carries the safe-write guarantee"
        );
        // busy 文案把 abandon 指为唯一出路：那条建议必须过与命令相同的门，
        // 否则在非 Linux 上就是一句注定失败的指引
        assert!(
            platform_allows(find("abandon").unwrap().capabilities),
            "the way out recommended by the busy message must be admitted on every platform"
        );
        for c in COMMANDS {
            assert!(!c.capabilities.is_empty(), "{}: capabilities not declared", c.name);
        }
        if !cfg!(target_os = "linux") {
            let permitted: Vec<&str> =
                COMMANDS.iter().filter(|c| platform_allows(c.capabilities)).map(|c| c.name).collect();
            assert_eq!(
                permitted,
                ["info", "abandon", "plan"],
                "only read-only commands and the recovery-scene release may pass this platform's gate"
            );
        }
    }

    /// SIZE 槽位声明必须落在该命令的位置参数范围内；声明了 SIZE 的命令恰好是
    /// 把尺寸放在位置参数上的那两条（`--size`/`--grow` 走旗标，不在此列）
    #[test]
    fn size_slot_is_within_the_positional_range() {
        for c in COMMANDS {
            if let Some(i) = c.size_pos {
                assert!(i < c.max_pos, "{}: SIZE slot {i} is outside max_pos {}", c.name, c.max_pos);
            }
        }
        assert_eq!(find("resize").unwrap().size_pos, Some(1), "resize takes SIZE at pos 1");
        assert_eq!(find("resizefs").unwrap().size_pos, Some(1), "resizefs takes BYTES at pos 1");
    }

    /// SYNOPSIS（帮助第一行）上的旗标名：剥掉 `[ ] ( ) ,` 包裹与 `=值`。
    /// 反向对账只扫这一行——正文会合法地引用别的命令的旗标（copy 提到
    /// resize 的 --allow-move，resize 点名拒绝 --start/--end）
    fn synopsis_tokens(help: &str) -> Vec<&str> {
        help.lines().next().unwrap_or("")
            .split_whitespace()
            .map(|t| t.trim_matches(|c| matches!(c, '[' | ']' | '(' | ')' | ',')))
            .collect()
    }

    fn synopsis_flags(help: &str) -> Vec<&str> {
        synopsis_tokens(help).into_iter()
            .filter(|t| t.starts_with("--"))
            .map(|t| t.split('=').next().unwrap_or(t))
            .collect()
    }

    /// 白名单与详助的双向对账：白名单里的旗标必须在帮助里出现（"这条命令接受
    /// 什么"要能在它自己的页面上查到）；SYNOPSIS 里出现的旗标必须被接受
    /// （帮助不许承诺解析器不认的能力）
    #[test]
    fn help_matches_flag_whitelists() {
        for c in COMMANDS {
            for f in synopsis_flags(c.help) {
                assert!(
                    c.flags.contains(&f),
                    "{}: synopsis advertises {f}, which the command does not accept",
                    c.name
                );
            }
            for f in c.flags {
                assert!(c.help.contains(f), "{}: accepts {f} but its help never mentions it", c.name);
            }
        }
    }

    /// SYNOPSIS 上的旗标形态与注册表一致：带值旗标必须带占位符（`--size SIZE`
    /// 或 `--size=SIZE`）；布尔旗标不许带——用户看不出带值旗标要参数，或误以为
    /// 布尔旗标也要取值
    #[test]
    fn synopsis_flag_shape_matches_the_registry() {
        for c in COMMANDS {
            let tokens = synopsis_tokens(c.help);
            for (i, t) in tokens.iter().enumerate() {
                let Some(spec) = crate::args::flag_spec(t.split('=').next().unwrap_or(t)) else { continue };
                let inline = t.contains('=');
                let next = tokens.get(i + 1).copied().unwrap_or("");
                if spec.value.is_none() {
                    assert!(
                        !inline && (next.is_empty() || next.starts_with("--")),
                        "{}: boolean {t} is shown with a value",
                        c.name
                    );
                    continue;
                }
                if inline {
                    continue;
                }
                assert!(
                    !next.is_empty() && !next.starts_with("--"),
                    "{}: {t} takes a value but the synopsis gives it no placeholder",
                    c.name
                );
            }
        }
    }

    /// 白名单与旗标注册表的对账：两边不许出现对方没有的条目。拼错名字会让
    /// 旗标静默失效（白名单认、解析臂不认），或让帮助提到解析器不认识的值
    #[test]
    fn flag_whitelists_match_the_registry() {
        use crate::args::FLAGS;
        let registry: Vec<&str> = FLAGS.iter().map(|f| f.name).collect();
        for c in COMMANDS {
            for f in c.flags {
                assert!(registry.contains(f), "{}: {f} is not in the flag registry", c.name);
            }
        }
        for spec in FLAGS {
            assert!(
                COMMANDS.iter().any(|c| c.flags.contains(&spec.name)),
                "{}: registered but no command accepts it",
                spec.name
            );
        }
    }
}