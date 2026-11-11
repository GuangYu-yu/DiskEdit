pub(crate) mod abandon;
pub(crate) mod confirm;
pub(crate) mod fs;
pub(crate) mod info;
pub(crate) mod inspect;
pub(crate) mod layout;
pub(crate) mod plan;
pub(crate) mod records;
pub(crate) mod resize;
pub(crate) mod undo;

use crate::cli::{CommandDef, Confirmation, Request, TargetMode};
use crate::domain::MediaKind;
use crate::outcome::RunOutcome;
use crate::platform::capabilities::HostPlatform;
use crate::platform::{CapabilityId, CapabilitySnapshot, CapabilityState};
use crate::support::bail_fail;

/// 测试与分派共用的命令表查询
#[cfg(test)]
pub(crate) use crate::cli::find;
pub(crate) use crate::cli::Capability;
#[cfg(test)]
pub(crate) use crate::cli::COMMANDS;

/// 命令分派的结果。退出码之外还要保住"撤销窗口能不能关"这一事实：
/// 5 既可能来自"布局已写、待办非空"，也可能来自"布局已写、仅内核视图过期"，
/// 两者对 journal 的去留结论相反，压成 u8 之后再也分不出来
pub(crate) enum CmdResult {
    /// 命令持有可判定的后置条件契约（布局写入及其后续链）
    RunOutcome(RunOutcome),
    /// 没有布局事务可判定：按退出码处理
    Code(u8),
}

impl CmdResult {
    pub(crate) fn code(&self) -> u8 {
        match self {
            Self::RunOutcome(o) => o.exit_code(),
            Self::Code(c) => *c,
        }
    }

    /// 收尾步骤失败：只有"已完整应用"的结果降为 5；没有后置条件契约的命令
    /// 只带退出码，其成功路径也不存在可降级的 Applied 形态
    pub(crate) fn degrade_partial(self, cause: impl Into<String>) -> Self {
        match self {
            Self::RunOutcome(o) => Self::RunOutcome(o.degrade_partial(cause)),
            Self::Code(code) => Self::Code(code),
        }
    }

    /// 出口归责：返回结果同样要按本次写事实对齐；只带退出码的命令没有可改判的形态
    pub(crate) fn reconcile_with_writes(self) -> Self {
        match self {
            Self::RunOutcome(o) => Self::RunOutcome(o.reconcile_with_writes()),
            Self::Code(code) => Self::Code(code),
        }
    }
}

impl From<RunOutcome> for CmdResult {
    fn from(o: RunOutcome) -> Self {
        Self::RunOutcome(o)
    }
}

impl From<u8> for CmdResult {
    fn from(code: u8) -> Self {
        Self::Code(code)
    }
}

/// 没有证据后端时的兜底文案。非 Linux 上 `inspect` / `records *` 由能力门先拒
/// （它们需要 `CompanionFile` → `AtomicReplace` 在非 Linux 不支持），下面的 cfg 分支
/// 只是让这些 Linux 专用模块在其它平台仍可编译，运行期不可达
#[cfg(not(target_os = "linux"))]
const NO_EVIDENCE_BACKEND: &str = "this command needs the directory-relative evidence backend, \
     which is Linux-only.";

/// 运行期唯一的能力判据：`CommandDef.capabilities` → [`crate::planner::needs_for`] →
/// [`CapabilitySnapshot::first_gap`]，与文档矩阵读同一份事实来源。
///
/// 门在命令函数之前、目标尚未打开，因此按**本平台能打开的载体形态**逐一取快照
/// （[`HostPlatform::openable_media`]）：Linux 有块设备与镜像两种，其余平台只有镜像。
/// 「不适用」按放行处理——它与载体形态绑定的能力在另一形态上不构成缺口。
/// 不查本平台打不开的形态，否则块设备列的一项缺口会反过来挡住可运行的只读镜像命令
fn capability_gap(caps: &[Capability]) -> Option<(MediaKind, CapabilityId, CapabilityState)> {
    for media in HostPlatform::current().openable_media() {
        let snapshot = CapabilitySnapshot::design(HostPlatform::current(), *media);
        if let Some((id, state)) = gap_with(&snapshot, caps) {
            return Some((*media, id, state));
        }
    }
    None
}

/// 单张快照上的判定：与文档矩阵的 `Commands` 表同一函数（`first_gap`），
/// 用例因此可以不经过平台门直接喂进手工构造的快照
fn gap_with(snapshot: &CapabilitySnapshot, caps: &[Capability]) -> Option<(CapabilityId, CapabilityState)> {
    snapshot.first_gap(&crate::planner::needs_for(caps))
}

/// 拒绝文案里的载体形态名
fn media_name(media: MediaKind) -> &'static str {
    match media {
        MediaKind::Block => "block-device",
        MediaKind::Image => "image-file",
    }
}

/// 只读命令共用的打开入口。只读命令只消费数据句柄，不消费身份、不取锁。
///
/// 身份不可得（`Unusable`）不构成失败：身份是**写命令**的门槛，
/// 只读命令照常工作。
///
/// 失败分类：
/// - `SectorOverrideOnBlock`：`--sector-size` 落在块设备上，属「选项与目标形态不符」→ 3
/// - `SectorSizeNotConstructible`：扇区大小不在允许集合，几何无法构造 → 6
/// - `Io`：打开失败，尚未写目标字节 → 4
pub(crate) fn open_read_only_target(
    target: &str,
    sector_size: Option<crate::domain::SectorSize>,
) -> Result<crate::dev::FileSource, crate::support::RunOutcome> {
    use crate::platform::probe::{ReadOnlyOpen, open_read_only_source};
    use crate::platform::TargetSpec;

    let spec = TargetSpec::new(target, sector_size);
    open_read_only_source(&spec).map_err(|e| match e {
        ReadOnlyOpen::SectorOverrideOnBlock(reason) => crate::support::RunOutcome::refused(reason),
        ReadOnlyOpen::SectorSizeNotConstructible => crate::support::RunOutcome::unsupported(
            "the target's sector size is outside the supported set — geometry cannot be constructed",
        ),
        ReadOnlyOpen::Io(e) => crate::support::RunOutcome::no_mutation(format!("open failed: {e}")),
    })
}

/// 命令分派：请求变体与处理函数一一对应。帮助与用法错误在 [`crate::cli::parse`] 里
/// 已退出进程，走到这里的都是结构完整的请求
pub(crate) fn dispatch(def: &CommandDef, req: &Request) -> CmdResult {
    // 运行边界：一条命令一次运行，写事实从入口起算
    crate::outcome::reset_run_facts();
    // 请求变体与分派所用声明必须同一条命令；确认规则与 `--yes` 的声明也必须一致
    debug_assert_eq!(req.def().name, def.name, "request variant and command definition disagree");
    match def.confirmation {
        Confirmation::None => {
            debug_assert!(!def.accepts("--yes"), "{}: None but accepts --yes", def.name)
        }
        Confirmation::Required => {
            debug_assert!(def.accepts("--yes"), "{}: requires --yes but does not accept it", def.name)
        }
    }
    // 声明门：空 capabilities 是漏声明，不是"无副作用"。能力门不替命令猜这个
    if def.capabilities.is_empty() {
        bail_fail(crate::support::RunOutcome::unsupported(format!(
            "command `{}` declares no capabilities — the gate refuses an undeclared command.",
            def.name
        )));
    }
    // 能力门是唯一的平台判据：先于任何命令函数，因而也先于锁/日志/journal/checkpoint
    // 的一切创建。拒绝文案点名缺失的原语与载体形态，不另立"Linux only"的第二套判据
    if let Some((media, id, state)) = capability_gap(def.capabilities) {
        bail_fail(crate::support::RunOutcome::unsupported(format!(
            "command `{}` needs the {} primitive, which is {} on this platform for a {} target.",
            def.name,
            id.as_str(),
            state.as_str(),
            media_name(media)
        )));
    }
    let result = match req {
        Request::Info(a) => info::cmd_info(a).into(),
        #[cfg(target_os = "linux")]
        Request::Inspect(a) => inspect::cmd_inspect(a),
        #[cfg(not(target_os = "linux"))]
        Request::Inspect(_) => bail_fail(crate::support::RunOutcome::unsupported(NO_EVIDENCE_BACKEND)),
        #[cfg(target_os = "linux")]
        Request::RecordsList(a) => records::cmd_records_list(a),
        #[cfg(not(target_os = "linux"))]
        Request::RecordsList(_) => bail_fail(crate::support::RunOutcome::unsupported(NO_EVIDENCE_BACKEND)),
        #[cfg(target_os = "linux")]
        Request::RecordsVerify(a) => records::cmd_records_verify(a),
        #[cfg(not(target_os = "linux"))]
        Request::RecordsVerify(_) => bail_fail(crate::support::RunOutcome::unsupported(NO_EVIDENCE_BACKEND)),
        #[cfg(target_os = "linux")]
        Request::RecordsScan(a) => records::cmd_records_scan(a),
        #[cfg(not(target_os = "linux"))]
        Request::RecordsScan(_) => bail_fail(crate::support::RunOutcome::unsupported(NO_EVIDENCE_BACKEND)),
        Request::Resize(a) => resize::cmd_resize(a).into(),
        Request::Move(a) => layout::cmd_move(a).into(),
        Request::Copy(a) => layout::cmd_copy(a).into(),
        Request::Delete(a) => layout::cmd_delete(a).into(),
        Request::Set(a) => fs::cmd_set(a).into(),
        Request::Check(a) => fs::cmd_check(a).into(),
        Request::Mkfs(a) => fs::cmd_mkfs(a).into(),
        Request::Resizefs(a) => fs::cmd_resizefs(a).into(),
        Request::Undo(a) => undo::cmd_undo(a).into(),
        Request::Abandon(a) => abandon::cmd_abandon(a).into(),
        Request::New(a) => layout::cmd_new(a).into(),
        Request::Add(a) => layout::cmd_add(a).into(),
        Request::ResizePart(a) => layout::cmd_resize_part(a).into(),
        Request::Plan(a) => plan::cmd_plan(a).into(),
    };
    // 开了撤销窗口的命令必须带回后置状态：只给退出码时退回"码是 0 即完成"，
    // 5 的待办非空与仅内核视图过期分不出来
    if def.mode == TargetMode::WriteJournal {
        assert!(
            !matches!(result, CmdResult::Code(_)),
            "WriteJournal command `{}` returned CmdResult::Code",
            def.name
        );
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 每条命令与撤销窗口的关系都是逐条裁定过的：这里把结论钉住——
    /// 表中漏一条（新命令随手选了个 mode）与改了 mode 同样报错：
    /// 错了的代价是静默留下一个撤销窗口，或静默抹掉别人的撤销窗口
    #[test]
    fn journal_modes_are_as_reviewed() {
        use TargetMode::{ReadOnly, WriteJournal, WriteNoJournal};
        let expected: &[(&str, TargetMode)] = &[
            ("info", ReadOnly),
            ("inspect", ReadOnly),
            ("records list", ReadOnly),
            ("records verify", ReadOnly),
            ("records scan", ReadOnly),
            ("resize", WriteJournal),
            ("move", WriteJournal),
            ("copy", WriteJournal),
            ("delete", WriteJournal),
            ("set", WriteJournal),
            ("check", ReadOnly),
            ("mkfs", WriteJournal),
            ("resizefs", WriteJournal),
            ("undo", WriteNoJournal),
            ("abandon", WriteNoJournal),
            ("new", WriteJournal),
            ("add", WriteJournal),
            ("resize-part", WriteJournal),
            ("plan", ReadOnly),
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
            ("inspect", &[ReadOnly, CompanionFile]),
            ("records list", &[ReadOnly, CompanionFile]),
            ("records verify", &[ReadOnly, CompanionFile]),
            ("records scan", &[ReadOnly, CompanionFile]),
            ("resize", &[TargetData, CompanionFile, ExternalTool, DeviceState]),
            ("move", &[TargetData, CompanionFile, ExternalTool, DeviceState]),
            ("copy", &[TargetData, CompanionFile, DeviceState]),
            ("delete", &[TargetData, CompanionFile, DeviceState]),
            ("set", &[TargetData, CompanionFile, ExternalTool, DeviceState]),
            ("check", &[ReadOnly, ExternalTool]),
            ("mkfs", &[CompanionFile, ExternalTool, DeviceState]),
            ("resizefs", &[CompanionFile, ExternalTool, DeviceState]),
            ("undo", &[TargetData, CompanionFile, ExternalTool, DeviceState]),
            ("abandon", &[CompanionFile]),
            ("new", &[TargetData, CompanionFile, DeviceState]),
            ("add", &[TargetData, CompanionFile, DeviceState]),
            ("resize-part", &[TargetData, CompanionFile, ExternalTool, DeviceState]),
            ("plan", &[ReadOnly]),
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

    /// 能力门只查本平台能打开的载体形态：Linux 两种全放行；非 Linux 只有镜像可打开，
    /// 纯只读命令（info/plan）放行，任何写侧/证据侧需求都命中缺口。
    /// 空声明由 dispatch 单独拒绝——needs_for 空集不会自己产生缺口，不能默认放行
    #[test]
    fn capability_gate_admits_only_what_the_platform_can_open() {
        use crate::domain::MediaKind;
        use crate::platform::CapabilitySnapshot;
        use crate::platform::capabilities::HostPlatform;

        assert_eq!(capability_gap(&[]), None, "an empty declaration produces no gap; dispatch must refuse it");
        let platform = HostPlatform::current();
        for media in platform.openable_media() {
            let snapshot = CapabilitySnapshot::design(platform, *media);
            for c in COMMANDS {
                assert!(!c.capabilities.is_empty(), "{}: capabilities not declared", c.name);
                let gap = gap_with(&snapshot, c.capabilities);
                if platform == HostPlatform::Linux {
                    assert_eq!(gap, None, "{}: {media:?}", c.name);
                } else {
                    let read_only = matches!(c.capabilities, [Capability::ReadOnly]);
                    assert_eq!(gap.is_none(), read_only, "{}: {media:?} gap={gap:?}", c.name);
                }
            }
        }
        // 非 Linux 的块设备列即使有缺口，也不该进入门：本平台打不开块设备
        if platform != HostPlatform::Linux {
            assert_eq!(platform.openable_media(), &[MediaKind::Image]);
        }
    }

    /// 能力门必须真的读矩阵：快照给「不支持」时判定命中，而不是照常执行。
    /// 今天的 Linux 设计矩阵全绿，这种组合只能取非 Linux 的矩阵单元
    #[test]
    fn an_unsupported_primitive_is_a_gap() {
        use crate::domain::MediaKind;
        use crate::platform::CapabilitySnapshot;
        use crate::platform::capabilities::HostPlatform;

        let windows = CapabilitySnapshot::design(HostPlatform::Windows, MediaKind::Image);
        assert_eq!(
            gap_with(&windows, &[Capability::TargetData]),
            Some((CapabilityId::WriteTable, CapabilityState::Unsupported)),
            "an unsupported primitive must be reported, not skipped"
        );
    }

    /// Linux 上每条命令在块设备与镜像两种形态都过能力门
    #[cfg(target_os = "linux")]
    #[test]
    fn every_command_passes_the_capability_gate_on_linux() {
        use crate::domain::MediaKind;
        use crate::platform::CapabilitySnapshot;
        use crate::platform::capabilities::HostPlatform;

        for c in COMMANDS {
            for media in [MediaKind::Block, MediaKind::Image] {
                let snapshot = CapabilitySnapshot::design(HostPlatform::Linux, media);
                assert_eq!(gap_with(&snapshot, c.capabilities), None, "{}: {media:?}", c.name);
            }
            assert_eq!(capability_gap(c.capabilities), None, "{}", c.name);
        }
    }

    /// SIZE 槽位由位置参数声明给出
    #[test]
    fn positional_slots_declare_size() {
        use crate::cli::Slot;
        assert!(find("resize").unwrap().has_slot(Slot::Size));
        assert!(find("resizefs").unwrap().has_slot(Slot::Size));
        assert!(!find("info").unwrap().has_slot(Slot::Size));
    }

    /// 剥掉 `[ ] ( ) ,` 包裹
    fn strip(token: &'static str) -> &'static str {
        token.trim_matches(|ch| matches!(ch, '[' | ']' | '(' | ')' | ','))
    }

    /// 单条用法行上的词。形态检查按行进行——跨行拼接会让上一行末尾的布尔旗标
    /// 把下一行行首的占位符当成自己的取值
    fn line_tokens(line: &'static str) -> Vec<&'static str> {
        line.split_whitespace().map(strip).collect()
    }

    /// 用法行上的旗标名：按声明顺序扫全部用法行。
    /// 反向对账只扫用法行——正文会合法地引用别的命令的旗标（copy 提到
    /// resize 的 --allow-move，resize 点名拒绝 --start/--end）
    fn synopsis_tokens(c: &CommandDef) -> Vec<&'static str> {
        c.syntax.iter().flat_map(|line| line_tokens(line)).collect()
    }

    fn synopsis_flags(c: &CommandDef) -> Vec<&'static str> {
        synopsis_tokens(c).into_iter()
            .filter(|t| t.starts_with("--"))
            .map(|t| t.split('=').next().unwrap_or(t))
            .collect()
    }

    /// 白名单与详助的双向对账：白名单里的旗标必须在帮助里出现（"这条命令接受
    /// 什么"要能在它自己的页面上查到）；用法行里出现的旗标必须被接受
    /// （帮助不许承诺解析器不认的能力）
    #[test]
    fn help_matches_flag_whitelists() {
        for c in COMMANDS {
            let help = crate::docs::help_text(c.name).expect("every command has help");
            for f in synopsis_flags(c) {
                assert!(
                    c.accepts(f),
                    "{}: synopsis advertises {f}, which the command does not accept",
                    c.name
                );
            }
            for f in c.options.iter().map(|o| o.name) {
                assert!(help.contains(f), "{}: accepts {f} but its help never mentions it", c.name);
            }
        }
    }

    /// 用法行上的旗标形态与注册表一致：带值旗标必须带占位符（`--size SIZE`
    /// 或 `--size=SIZE`）；布尔旗标不许带——用户看不出带值旗标要参数，或误以为
    /// 布尔旗标也要取值。被 `[ ]`/`( )` 包住的下一段是可选的独立成分，不算取值
    #[test]
    fn synopsis_flag_shape_matches_the_registry() {
        for c in COMMANDS {
            for line in c.syntax {
                let raw: Vec<&'static str> = line.split_whitespace().collect();
                let tokens: Vec<&'static str> = raw.iter().copied().map(strip).collect();
                for (i, t) in tokens.iter().enumerate() {
                    let Some(spec) = crate::cli::flag_spec(t.split('=').next().unwrap_or(t)) else { continue };
                    let inline = t.contains('=');
                    let next = raw.get(i + 1).copied().unwrap_or("");
                    let joins = !next.is_empty()
                        && !next.starts_with("--")
                        && !next.starts_with('[')
                        && !next.starts_with('(');
                    if !spec.domain.takes_value() {
                        assert!(
                            !inline && !joins,
                            "{}: boolean {t} is shown with a value",
                            c.name
                        );
                        continue;
                    }
                    if inline {
                        continue;
                    }
                    assert!(
                        joins,
                        "{}: {t} takes a value but the synopsis gives it no placeholder",
                        c.name
                    );
                }
            }
        }
    }
}
