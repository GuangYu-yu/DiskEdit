//! 布局操作的**后置条件契约**。
//!
//! 命令的承诺不是"运行了什么"，而是"完成后哪些条件成立"：
//! 默认契约 = 分区布局改变 + 空间对该分区可用（FS 扩展 / swap 重建 / LVM 链）。
//! 退出码只是这个契约满足情况的外显，因此映射集中在此处一处，
//! 不允许各调用点自行拼装数字
//!
//! 未完成的情形按三条轴分：**本次是否写盘**（能否对盘上状态作断言）、
//! **成因在请求还是在环境**（改参数重试是否有意义）、**平台是否具备能力**：
//! - 参数或命令语法错误 → 2，未写盘
//! - 请求与目标现状不匹配 + 未写盘 → 3，改参数有意义
//! - 环境/盘内容故障 + 确定未写盘 → 4，无持久化修改
//! - 已写盘但后置条件未全满足，或无法断言写没写 → 5，须给恢复指引
//! - 平台或目标能力不支持 → 6
//! - 未预期的内部错误 → 1
//!
//! **分类必须在知道成因的那一层做**：同一个成因（例如表读不出来）会因"无表"与
//! "表非法"落进两个变体，而 io::Error 一旦成形就再也分不出来，只会在调用点被压成一个码

pub const EXIT_OK: u8 = 0;
/// 未预期的内部错误：断言失败、内部不变量被破坏
pub const EXIT_INTERNAL: u8 = 1;
/// 参数或命令语法错误：argv 结构不成立、值域不符、未知命令/选项
pub const EXIT_USAGE: u8 = 2;
/// 策略拒绝或前置条件不满足：无表、无此分区、缺 --yes、表结构非法、目标忙
pub const EXIT_REFUSED: u8 = 3;
/// 操作失败，且本次运行未向目标发出过写：判据是本次运行从未调到写原语
/// （[`target_may_have_been_written`] 在写原语之前置位，取反即「从未调用」）。
/// 不排除外部写者改过盘；锁、恢复记录、日志等伴随文件不计入
pub const EXIT_NO_MUTATION: u8 = 4;
/// 写盘已发生，但结论不是整体成功：部分完成、已应用而后续步骤待办，或表已写而内核
/// 视图未同步（[`KernelSync::Stale`]）。补救指引随结果打印
pub const EXIT_PARTIAL: u8 = 5;
/// 平台或能力不支持：工具缺失、平台无对应 API、目标类型不在支持范围
pub const EXIT_UNSUPPORTED: u8 = 6;

/// 统一出口在「本次未写盘」的失败之后追加的一行。生成文档引用本常量，
/// 不另抄字面量
pub(crate) const NOTHING_WRITTEN: &str = "— nothing was written by this run";

thread_local! {
    /// 一次运行的写事实。线程作用域：生产一条命令一个线程一次运行，单测每个用例各占
    /// 一个线程；同一线程被复用（如串行跑测试）时由 [`reset_run_facts`] 显式复位
    static RUN_FACTS: RunFacts = const { RunFacts::new() };
}

struct RunFacts {
    /// 本次运行是否可能已经写过目标字节。置位点即全部目标写入通道：写句柄的
    /// `write_raw`、mkfs 前的 `wipe_zero`、启动后会改动目标的外部工具
    /// （`fsops::env::run_write*`）。一次运行内置位后不复位
    target_may_be_written: std::cell::Cell<bool>,
    /// 恢复记录是否已越过不可逆写入点（`restore_blocked` 或已提交）
    record_past_irreversible: std::cell::Cell<bool>,
    /// 本次运行是否已记下首写步骤。区分"还没写过"与"首写不可归属"：后者记为空，
    /// 但此后带归属的写不能改写它
    first_write_seen: std::cell::Cell<bool>,
    /// 本次运行第一次向目标写出字节时所属的步骤。由施加者在写入口传入，只记第一次。
    /// 未经写入口的首写（`wipe_zero`、`fsops::env::run_write*`）记为空，此后即使有
    /// 带归属的写入也不能改写它——诊断行陈述的是"首写"，不是"首个可归属的写"
    first_write_step: std::cell::Cell<Option<WriteStep>>,
    /// 本次运行是否已经建出或接管过一份恢复记录。呈现层据此区分"本次没写、但记录还在
    /// 盘上可续"与"本次什么也没留下"
    record_present: std::cell::Cell<bool>,
    /// 接管到的记录是否已越过提交点。已提交的记录说明目标字节已定型，只差释放；
    /// 未提交的才是可以接着做的作业
    record_finalized: std::cell::Cell<bool>,
    /// 本次运行是否把恢复记录写出过盘。回答的是「本次运行落过盘没有」，不是记录内容：
    /// 不决定记录能否清理，也不决定能否报 `Applied`
    record_persisted: std::cell::Cell<bool>,
}

impl RunFacts {
    const fn new() -> Self {
        Self {
            target_may_be_written: std::cell::Cell::new(false),
            record_past_irreversible: std::cell::Cell::new(false),
            first_write_seen: std::cell::Cell::new(false),
            first_write_step: std::cell::Cell::new(None),
            record_present: std::cell::Cell::new(false),
            record_finalized: std::cell::Cell::new(false),
            record_persisted: std::cell::Cell::new(false),
        }
    }
}

/// 复位本次运行的写事实。两处运行边界各调一次：命令入口与每次构造
/// [`crate::dev::WriteSource`]
pub(crate) fn reset_run_facts() {
    RUN_FACTS.with(|f| {
        f.target_may_be_written.set(false);
        f.record_past_irreversible.set(false);
        f.first_write_seen.set(false);
        f.first_write_step.set(None);
        f.record_present.set(false);
        f.record_finalized.set(false);
        f.record_persisted.set(false);
    });
}

/// 一次写入的归属。诊断行只报这个值，不报命令名——同一命令的多个写步骤
/// 失败含义不同，命令名区分不了
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteStep {
    /// 数据搬移/复制的块写入；`part` 是被搬移的条目号
    Relocate { part: u32 },
    /// 分区表写入
    Table,
    /// 文件系统内部字段的写入
    Fs { part: u32 },
    /// 不含数据搬移的修复写入
    Repair,
}

impl std::fmt::Display for WriteStep {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Relocate { part } => write!(f, "Relocate(part {part})"),
            Self::Table => write!(f, "Table"),
            Self::Fs { part } => write!(f, "Fs(part {part})"),
            Self::Repair => write!(f, "Repair"),
        }
    }
}

pub(crate) fn note_target_write() {
    RUN_FACTS.with(|f| {
        if !f.first_write_seen.get() {
            f.first_write_seen.set(true);
            f.first_write_step.set(None);
        }
        f.target_may_be_written.set(true);
    });
}

/// 写入口专用：记下首写步骤，并照常置位写事实
pub(crate) fn note_target_write_step(step: WriteStep) {
    RUN_FACTS.with(|f| {
        if !f.first_write_seen.get() {
            f.first_write_seen.set(true);
            f.first_write_step.set(Some(step));
        }
        f.target_may_be_written.set(true);
    });
}

pub(crate) fn first_write_step() -> Option<WriteStep> {
    RUN_FACTS.with(|f| f.first_write_step.get())
}

pub(crate) fn target_may_have_been_written() -> bool {
    RUN_FACTS.with(|f| f.target_may_be_written.get())
}

/// 接管盘上已有的记录时置位。调用方只在记录确实从盘上读到时才调它；
/// 本次运行内存里新建的记录由 [`note_record_persisted`] 在落盘成功后置位
pub(crate) fn note_record_present(committed: bool) {
    RUN_FACTS.with(|f| {
        f.record_present.set(true);
        if committed {
            f.record_finalized.set(true);
        }
    });
}

pub(crate) fn record_is_present() -> bool {
    RUN_FACTS.with(|f| f.record_present.get())
}

/// 记录已从落点移除：不再有盘上记录可续跑或需释放。名字空间移除与目录持久化
/// 是否确认不影响这条事实，两者都由调用方按自己的三态分别陈述
pub(crate) fn note_record_removed() {
    RUN_FACTS.with(|f| f.record_present.set(false));
}

/// `Committed` 已持久化：本次新建的记录越过 durable boundary 后由 `commit` 置位。
/// 置位点只在 Linux：其余平台没有记录落点
#[cfg(target_os = "linux")]
pub(crate) fn note_record_finalized() {
    RUN_FACTS.with(|f| f.record_finalized.set(true));
}

/// 记录是否已定型：接管时 `commit == Committed`（表步已提交，只剩释放）
pub(crate) fn record_is_finalized() -> bool {
    RUN_FACTS.with(|f| f.record_finalized.get())
}

pub(crate) fn note_record_past_irreversible_point() {
    RUN_FACTS.with(|f| f.record_past_irreversible.set(true));
}

/// 作业是否已越过不可逆写入点：本次运行写过目标字节，或恢复记录已落屏障/
/// 提交。出口归责、FS 步的"表已扩"分支与装配期的越界判定都只问这一处
pub(crate) fn crossed_irreversible_point() -> bool {
    RUN_FACTS.with(|f| f.target_may_be_written.get() || f.record_past_irreversible.get())
}

/// 未完成点的归责分岔：本次运行是否可能已写目标字节。
///
/// 写事实只在这里读一次；调用点只提供两支各自的结论，不自行判定
/// 「写了还是没写」。已写的一支按 5（或带待办）上报，未写的一支保留
/// 该判定点自己的拒绝 / 无修改分类
pub(crate) fn attribute<T>(written: impl FnOnce() -> T, unwritten: impl FnOnce() -> T) -> T {
    if crossed_irreversible_point() {
        written()
    } else {
        unwritten()
    }
}

/// 置位点在 `RecordJournal::persist` 的整份替换/创建成功之后（含目录项已改、
/// 目录 fsync 未确认的分支）。barrier 只在记录尚未落盘时经它落盘，表区 preimage
/// 等路径同样经它落盘；只读打开既有记录不置位。落盘成功同时说明盘上已有记录，
/// 因此与 [`note_record_present`] 一样置 `record_present`
#[cfg(target_os = "linux")]
pub(crate) fn note_record_persisted() {
    RUN_FACTS.with(|f| {
        f.record_persisted.set(true);
        f.record_present.set(true);
    });
}

pub(crate) fn record_persisted() -> bool {
    RUN_FACTS.with(|f| f.record_persisted.get())
}

/// 外部 FS 工具改的是文件系统内容，journal 只记分区表写入。一次命令可能走多个外部
/// 工具步骤（先缩后扩、或在同一命令里既有表写入又有 FS 写入），故每进程只提示一次。
/// 这条提示补的是"undo 成功"与实际回滚范围之间的差：表回去了，文件系统没有
pub(crate) fn note_fs_writes_not_journaled() {
    use std::sync::atomic::{AtomicBool, Ordering};
    static NOTED: AtomicBool = AtomicBool::new(false);
    if !NOTED.swap(true, Ordering::Relaxed) {
        eprintln!(
            "note: filesystem writes are made by external tools and are not journaled — `undo` reverts \
             partition-table writes only; verify the filesystem's actual size before further operations"
        );
    }
}

/// 后续步骤的种类：决定补救提示里出现哪条命令。
/// Fs/Swap 之外的做法：标签随变体由调用方携带——本层不认识具体机制，只做呈现
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PendingKind {
    Fs,
    Swap,
    /// 该步被推迟到目标系统启动期完成（如 OpenWrt 的 overlay 初始化）：本命令没有做，
    /// 也不保证那一侧一定成功。与 Fs/Swap 的区别在"由谁完成"，不在"是否算完成"
    RuntimeInitialization,
    /// 其余机制（LVM 链等）：显示标签由调用方给出
    Other(&'static str),
}

/// 一条未满足的后置条件。补救信息由 FS 层生成（工具与包名的知识在那里），
/// 出口只负责格式化，避免"该跑什么命令"的字符串在两层各写一遍而漂移
#[derive(Debug, Clone)]
pub struct Pending {
    pub part: u32,
    pub kind: PendingKind,
    /// 失败原因（外部工具的原话）
    pub detail: String,
    /// 补救命令（可含具体设备路径）
    pub hint: String,
}

impl Pending {
    pub fn new(part: u32, kind: PendingKind, detail: impl Into<String>, hint: impl Into<String>) -> Self {
        Self { part, kind, detail: detail.into(), hint: hint.into() }
    }
}

/// 内核分区视图是否已跟上盘上的表。与 `pending` 正交：`pending` 是"业务后置条件是否完成"，
/// 本项是"内核可见性"，两者可同时成立（如：表已写、内核未同步、FS 步也因此未做）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KernelSync {
    Synchronized,
    Stale,
}

/// `Partial` 的来源。退出码相同，只能靠这一项分清四种成因：执行器在写原语处观察到的失败
/// （`ObservedWriteFailure`）、写已成功但写后核对或后置条件不成立（`PostconditionAfterWrite`）、
/// 已应用但读回证据拿不到（`UnverifiedAfterWrite`）、归责点据"本次已写过"把别的结论改判
/// （`DerivedFromPriorWrite`）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PartialOrigin {
    /// 执行器在写原语处直接观察到失败（写调用返回 Err）
    ObservedWriteFailure,
    /// 写已成功，写后核对或后置条件不成立
    PostconditionAfterWrite,
    /// 已应用，但读回证据拿不到：不是失败，也不构成 Applied
    UnverifiedAfterWrite,
    /// 归责点据「作业已越过不可逆写入点」把 Refused/NoMutation/Internal 改判为 5
    DerivedFromPriorWrite,
}

impl PartialOrigin {
    fn as_str(self) -> &'static str {
        match self {
            Self::ObservedWriteFailure => "observed-write-failure",
            Self::PostconditionAfterWrite => "postcondition-after-write",
            Self::UnverifiedAfterWrite => "unverified-after-write",
            Self::DerivedFromPriorWrite => "derived-from-prior-write",
        }
    }
}

/// 一次布局操作的结果。`Applied` 之外的分支覆盖未完成的每种成因，
/// 它们的退出码与报告文字都不同，必须由类型区分
#[derive(Debug, Clone)]
pub enum RunOutcome {
    /// argv 结构、值域或命令名不成立——本次没有触碰目标
    Usage(String),
    /// 校验阶段拒绝——成因在请求与目标现状不匹配
    Refused(String),
    /// 环境/盘内容故障——I/O 失败、表结构非法
    NoMutation { cause: String },
    /// 平台或目标能力不支持——换参数无意义
    Unsupported { cause: String },
    /// 未预期的内部错误——说明工具自身有缺陷
    Internal(String),
    /// 执行阶段失败——**不对"是否已落盘"作断言**：写调用报错时无从得知
    /// 实际写入多少，磁盘状态可能已改变，故只承诺"可能已改变，需验证"。
    /// `pending` 是已知但尚未满足的后置条件：失败发生在它们之前/中途时随失败一并报出。
    /// `origin` 记录这个 5 从哪来：执行器观察到的失败、写后核对不成立、读回拿不到证据，
    /// 或归责点据本次写事实改判
    Partial {
        cause: String,
        pending: Vec<Pending>,
        origin: PartialOrigin,
    },
    /// 布局写入完成；`pending` 为空且内核视图已同步即后置条件全部满足
    Applied {
        pending: Vec<Pending>,
        kernel_sync: KernelSync,
    },
}

impl RunOutcome {
    pub fn usage(msg: impl Into<String>) -> Self {
        Self::Usage(msg.into())
    }

    pub fn refused(msg: impl Into<String>) -> Self {
        Self::Refused(msg.into())
    }

    pub fn no_mutation(cause: impl Into<String>) -> Self {
        Self::NoMutation { cause: cause.into() }
    }

    pub fn unsupported(cause: impl Into<String>) -> Self {
        Self::Unsupported { cause: cause.into() }
    }

    pub fn internal(msg: impl Into<String>) -> Self {
        Self::Internal(msg.into())
    }

    pub fn partial(cause: impl Into<String>) -> Self {
        Self::Partial {
            cause: cause.into(),
            pending: Vec::new(),
            origin: PartialOrigin::ObservedWriteFailure,
        }
    }

    /// 已写盘或无法断言的失败，另带已知未完成的后置条件
    pub fn partial_with(cause: impl Into<String>, pending: Vec<Pending>) -> Self {
        Self::Partial { cause: cause.into(), pending, origin: PartialOrigin::ObservedWriteFailure }
    }

    /// 写已成功、写后核对或后置条件不成立
    pub fn partial_postcondition(cause: impl Into<String>, pending: Vec<Pending>) -> Self {
        Self::Partial {
            cause: cause.into(),
            pending,
            origin: PartialOrigin::PostconditionAfterWrite,
        }
    }

    /// 已应用但读回证据拿不到：不是失败，也不构成 Applied
    pub fn partial_unverified(cause: impl Into<String>, pending: Vec<Pending>) -> Self {
        Self::Partial {
            cause: cause.into(),
            pending,
            origin: PartialOrigin::UnverifiedAfterWrite,
        }
    }

    /// 布局已写、后置条件待办；内核视图默认按"已同步"起算，由调用方在 resync 失败后置位
    pub fn applied_with(pending: Vec<Pending>) -> Self {
        Self::Applied { pending, kernel_sync: KernelSync::Synchronized }
    }

    /// 布局已写但内核视图未跟上（在线路径表已落盘、partx/BLKPG 未同步时直接构造）。
    /// 唯一调用方是 online（仅 Linux）——非 Linux 构建下没有在线路径可产生"内核视图过期"
    #[cfg(target_os = "linux")]
    pub fn applied_stale_kernel() -> Self {
        Self::Applied { pending: Vec::new(), kernel_sync: KernelSync::Stale }
    }

    /// 内核重读失败：把"内核视图过期"这一事实记入结果（只对 Applied 有意义）
    pub fn mark_kernel_stale(&mut self) {
        if let Self::Applied { kernel_sync, .. } = self {
            *kernel_sync = KernelSync::Stale;
        }
    }

    /// 收尾步骤（关闭撤销窗口、释放资源）没有完成：已经完整应用的结果降级为部分完成（5）。
    /// 其他形态保持原样——它们已经陈述了更差的事实，同一个收尾故障不该覆盖它
    pub fn degrade_partial(self, cause: impl Into<String>) -> Self {
        if self.is_complete() {
            Self::Partial {
                cause: cause.into(),
                pending: Vec::new(),
                origin: PartialOrigin::PostconditionAfterWrite,
            }
        } else {
            self
        }
    }

    /// 归责投影：按作业是否已越过不可逆写入点（[`crossed_irreversible_point`]）对齐
    /// 结论。越过时，"没写"的四种结论（拒绝、无修改、内部错误、能力缺口）改判为 5
    /// 并保留原结论的正文；`Partial`/`Applied` 原样返回，来源不被改写，
    /// 因此重复调用不会重复改写正文。
    ///
    /// 私有：外部只能经 [`Self::settled`]（对齐、不打印）或 [`Self::report_final`]
    /// （对齐 + 打印）取得归责后的结论。调用点因此只有一个，归责语义也只有这一处产生
    fn reconcile_with_writes(self) -> Self {
        if !crossed_irreversible_point() {
            return self;
        }
        match self {
            Self::Refused(msg) => Self::Partial {
                cause: format!("{msg} — the job has already written the target, so the refusal does not describe the on-disk state"),
                pending: Vec::new(),
                origin: PartialOrigin::DerivedFromPriorWrite,
            },
            Self::NoMutation { cause } => Self::Partial {
                cause: format!("{cause} — the job has already written the target, so this is not a no-mutation failure"),
                pending: Vec::new(),
                origin: PartialOrigin::DerivedFromPriorWrite,
            },
            Self::Internal(msg) => Self::Partial {
                cause: format!(
                    "internal error (bug): {msg} — the job has already written the target; keep the recovery record and report it"
                ),
                pending: Vec::new(),
                origin: PartialOrigin::DerivedFromPriorWrite,
            },
            Self::Unsupported { cause } => Self::Partial {
                cause: format!(
                    "{cause} — the job has already written the target, so this is a partial completion, not an unsupported operation"
                ),
                pending: Vec::new(),
                origin: PartialOrigin::DerivedFromPriorWrite,
            },
            other => other,
        }
    }

    /// 出口归责的唯一投影：把本次运行的写事实折进结论，不打印。
    ///
    /// 命令层的归责出口与 `main` 的兜底都从这里取，不直接调用
    /// [`Self::reconcile_with_writes`]——投影点只有一处
    pub(crate) fn settled(self) -> Self {
        self.reconcile_with_writes()
    }

    /// 打印前归责：投影 → 报告 → 把归责后的结果交回调用方，
    /// 退出码与打印出的变体因此同源
    pub(crate) fn report_final(self) -> Self {
        let outcome = self.settled();
        outcome.report();
        outcome
    }

    /// 同一次操作里两条收尾链（布局 + 其后的 FS/LVM 链）合并成最终结果：
    /// 待办取并集，内核视图按更差的一侧记。两条链都必须已越过写盘边界——
    /// 任何一侧不是 `Applied` 时，未应用的那一侧才是最终结论
    pub fn merge_applied(self, other: Self) -> Self {
        match (self, other) {
            (
                Self::Applied { pending: mut mine, kernel_sync: a },
                Self::Applied { pending: theirs, kernel_sync: b },
            ) => {
                mine.extend(theirs);
                Self::Applied {
                    pending: mine,
                    kernel_sync: if a == KernelSync::Stale || b == KernelSync::Stale {
                        KernelSync::Stale
                    } else {
                        KernelSync::Synchronized
                    },
                }
            }
            (mine, theirs) => {
                if theirs.is_layout_applied() {
                    mine
                } else {
                    theirs
                }
            }
        }
    }

    /// 本次运行的布局写入已经应用（`Applied` 的任何形态，包括待办非空与内核视图过期）。
    /// 只回答"这次运行是否给出过 `Applied`"，不回答"盘上是否可能已改过"——后者看
    /// [`Self::disk_may_have_changed`]
    pub(crate) fn is_layout_applied(&self) -> bool {
        matches!(self, Self::Applied { .. })
    }

    /// 盘上字节可能已经改变：本次已应用、本次运行已置写事实，或恢复记录里留着写入证据。
    /// 只用于**保守动作**（内核重读、状态复核）；不决定记录能否清理，也不决定能否报 `Applied`
    pub(crate) fn disk_may_have_changed(&self, record: Option<&crate::recovery::Record>) -> bool {
        self.is_layout_applied()
            || target_may_have_been_written()
            || record.is_some_and(crate::recovery::Record::dirty)
    }

    /// 后置条件全部满足：无待办且内核视图已同步。这是**唯一**对应退出码 0 的形态，
    /// 调用点要问"是不是完全成功"时用它而不是比退出码——比数字会把"部分完成"误判成成功
    pub fn is_complete(&self) -> bool {
        matches!(
            self,
            Self::Applied { pending, kernel_sync }
                if pending.is_empty() && *kernel_sync == KernelSync::Synchronized
        )
    }

    /// 唯一的 RunOutcome → 退出码映射点
    pub fn exit_code(&self) -> u8 {
        match self {
            Self::Usage(_) => EXIT_USAGE,
            Self::Refused(_) => EXIT_REFUSED,
            Self::NoMutation { .. } => EXIT_NO_MUTATION,
            Self::Unsupported { .. } => EXIT_UNSUPPORTED,
            Self::Internal(_) => EXIT_INTERNAL,
            Self::Partial { .. } => EXIT_PARTIAL,
            applied if applied.is_complete() => EXIT_OK,
            Self::Applied { .. } => EXIT_PARTIAL,
        }
    }

    /// 统一输出。成功且无遗留时不打印（调用方自报成功信息），
    /// 其余情况必须让用户知道盘上实际发生了什么、下一步该做什么。
    /// 调用方须在"内核重读"之后调用本函数，否则打不出内核视图过期这一项
    pub fn report(&self) {
        match self {
            Self::Usage(msg) => eprintln!("usage error: {msg}"),
            Self::Refused(msg) => eprintln!("refused: {msg}\n{NOTHING_WRITTEN}"),
            // 与 Partial 不同话：本次没写盘，不能提示"盘可能已改变"
            Self::NoMutation { cause } => eprintln!("error: {cause}\n{NOTHING_WRITTEN}"),
            Self::Unsupported { cause } => eprintln!("unsupported: {cause}"),
            Self::Internal(msg) => eprintln!("internal error: {msg}"),
            Self::Partial { cause, pending, origin } => {
                eprintln!("error: {cause}");
                Self::report_attribution(*origin);
                Self::report_partial_guidance();
                if !pending.is_empty() {
                    eprintln!("left undone: {} known follow-up step(s)", pending.len());
                    Self::report_pending(pending);
                }
            }
            Self::Applied { pending, kernel_sync } => {
                if pending.is_empty() && *kernel_sync == KernelSync::Synchronized {
                    return;
                }
                if !pending.is_empty() {
                    // 头部不点名"分区表"：纯 FS 路径（fs / superfloppy）也走 Applied，
                    // 那里被写下的是文件系统。头部只陈述"主步骤已生效、后置条件未全满足"
                    eprintln!("the requested change is applied, but {} follow-up step(s) are pending:", pending.len());
                    Self::report_pending(pending);
                }
                if *kernel_sync == KernelSync::Stale {
                    eprintln!(
                        "partition table is on disk but the kernel partition view is stale — \
                         run `partprobe <device>` (or partx/blkpg) before relying on the new layout"
                    );
                }
                Self::report_attribution(PartialOrigin::PostconditionAfterWrite);
                Self::report_partial_guidance();
            }
        }
        // 本次一个目标字节也没写，但记录还在盘上：已提交的记录只差释放，未提交的才能接着做
        if record_is_present() && !target_may_have_been_written() {
            if record_is_finalized() {
                eprintln!(
                    "the recovery record on disk belongs to a job already applied — only its release \
                     remains: `diskedit undo` releases it (the recorded bytes are past their rollback \
                     point), `diskedit abandon` releases it as is; re-running the command does not \
                     continue it"
                );
            } else {
                eprintln!(
                    "the unfinished recovery record is still on disk — `diskedit undo` or re-running the \
                     command continues from it"
                );
            }
        }
    }

    /// 5 的诊断行：来源、首个写步骤、记录侧事实。首个写步骤取自写入口记录的
    /// [`first_write_step`]；未经写入口的首写（wipe_zero、外部工具）报 unknown。
    /// 记录侧只报本次运行有没有把记录落过盘
    fn report_attribution(origin: PartialOrigin) {
        let origin = match origin {
            PartialOrigin::UnverifiedAfterWrite => "unverified-after-write (unverified, not a failure)",
            other => other.as_str(),
        };
        let first = match first_write_step() {
            Some(step) => step.to_string(),
            None => "unknown (write entry not instrumented)".to_string(),
        };
        let record = if record_persisted() {
            "persisted by this run"
        } else {
            "not persisted by this run"
        };
        eprintln!("origin={origin} first write: {first} record: {record}");
    }

    /// 5 的报告必须同时给出三件事：盘可能已改变、证据路径、下一步命令。
    /// 证据路径取本进程真正建出的 journal 落点——没有建过就直说没有，不编路径
    pub(crate) fn report_partial_guidance() {
        for line in Self::partial_guidance_lines() {
            eprintln!("{line}");
        }
    }

    fn partial_guidance_lines() -> [String; 3] {
        [
            "— the target may already have been changed; check with `diskedit info` before retrying"
                .to_string(),
            "evidence: `diskedit inspect <target>` names any recovery file left for the target".to_string(),
            // 无条件推荐 undo 会在"已越过回滚点"的形态上指向一条必然退 3 的路：
            // 能否回写由记录自身决定，这里只陈述两条命令各自做什么
            "next: `diskedit undo` restores the bytes recorded for this target when the record \
             still allows it; `diskedit abandon` releases the record and leaves those bytes in place"
                .to_string(),
        ]
    }

    /// 未完成后置条件的逐条呈现：Applied 与 Partial 两条路径共用同一份格式。
    /// 续行也带同一个 `[part N]` 标记：列对齐由格式决定，不靠字面空格——用空格对齐时
    /// 分区号一旦到两位数，整段就会偏一格，而这种偏移不会有任何一处报错
    fn report_pending(pending: &[Pending]) {
        for p in pending {
            let what = match p.kind {
                PendingKind::Fs => "fs grow",
                PendingKind::Swap => "swap rebuild",
                PendingKind::RuntimeInitialization => "overlay initialization",
                PendingKind::Other(what) => what,
            };
            eprintln!("  [part {}] {what}: {}", p.part, p.detail);
            if !p.hint.is_empty() {
                // 这一档的后续动作不在本机、也不由用户当场执行，提示的是核实方式
                let follow_up = match p.kind {
                    PendingKind::RuntimeInitialization => "after startup",
                    _ => "run manually",
                };
                eprintln!("  [part {}] {follow_up}: {}", p.part, p.hint);
            }
        }
    }
}

/// 文本只承载成因：退出码的分工在变体上，不在字符串里
impl std::fmt::Display for RunOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Usage(m) | Self::Refused(m) | Self::Internal(m) => f.write_str(m),
            Self::NoMutation { cause } | Self::Unsupported { cause } => f.write_str(cause),
            Self::Partial { cause, .. } => f.write_str(cause),
            Self::Applied { .. } => f.write_str("the requested change is applied"),
        }
    }
}

impl RunOutcome {
    /// 写盘前的 io 失败。存在的意义是让调用点**显式表态**：这个 `?` 位于本次调用的首次
    /// 目标写盘之前。默认的 `From<io::Error>` 给不出这个断言，故不设默认、必须逐点写明。
    ///
    /// 判据只关于**本次调用**写没写目标盘。上一次中断的运行留下的改动不在此编码范围内——
    /// 重试时工具会重读盘上几何（表项 / 记录），故不因省略"盘可能已改变"的提示而失去安全性；
    /// 而真正需要那句提示的是写调用报错（`Partial`），那种情况下写入量确实无法断定
    pub fn no_mutation_io(e: std::io::Error) -> Self {
        Self::NoMutation { cause: e.to_string() }
    }

    /// 给原因加上本层上下文（变体与退出码不变）。存在的意义是让调用点补一句"当时在做什么"
    /// 而不必丢弃已经得出的分类——为了拼文案而改用 `RunOutcome::no_mutation(..)` 会把
    /// `Refused` 悄悄升格成 `NoMutation`
    pub fn context(self, what: &str) -> Self {
        match self {
            Self::Usage(m) => Self::Usage(format!("{what}: {m}")),
            Self::Refused(m) => Self::Refused(format!("{what}: {m}")),
            Self::NoMutation { cause } => Self::NoMutation { cause: format!("{what}: {cause}") },
            Self::Unsupported { cause } => Self::Unsupported { cause: format!("{what}: {cause}") },
            Self::Internal(m) => Self::Internal(format!("{what}: {m}")),
            Self::Partial { cause, pending, origin } => {
                Self::Partial { cause: format!("{what}: {cause}"), pending, origin }
            }
            Self::Applied { pending, kernel_sync } => Self::Applied { pending, kernel_sync },
        }
    }
}

/// 默认按"可能已改变"归 `Partial`：写调用报错时无从得知实际写入多少，
/// 这是唯一安全的缺省。**写盘前的环境故障必须显式写 `RunOutcome::no_mutation`**——
/// 与 table::GptError 只提供单向压平同理：需要区分的地方由编译器强制它显式表态
impl From<std::io::Error> for RunOutcome {
    fn from(e: std::io::Error) -> Self {
        Self::partial(e.to_string())
    }
}

/// FS 层的失败分类 → 出口语义。**这是 `FsError` 唯一的解释处**：fsops 只回答
/// "操作层面发生了什么"，落成哪个退出码属应用层 policy，故不在 fsops 里做。
///
/// 「类型没有接线 / 参数与目标现状不符」→ 拒绝（3）；工具缺失 → 不支持（6）；
/// 环境故障发生在任何写盘之前 → 无持久化修改（4）；外部工具非零退出意味着工具
/// 已经运行——它写没写、写了多少无法断言 → 部分完成（5，报告带警示行）。
///
/// **本映射只适用于尚未写盘的调用点**：`Io` 变体在此归 NoMutation（断言"确定未写盘"），
/// 一旦本次调用已开始写目标盘（清零、格式化中……），调用点必须先把错误改判为
/// `CommandFailed`（→ Partial），不得让 `Io` 走进这个 `From`——那会把"可能已写"
/// 洗成"确定没写"
impl From<crate::fsops::FsError> for RunOutcome {
    fn from(e: crate::fsops::FsError) -> Self {
        use crate::fsops::FsError;
        match e {
            FsError::UnsupportedFs { detail, .. } => Self::unsupported(detail),
            FsError::UnsupportedPlatform(m) => Self::unsupported(m),
            FsError::InvalidArgument(m) => Self::refused(m),
            FsError::ToolMissing(m) => Self::unsupported(m),
            FsError::Io(err) => Self::no_mutation(err.to_string()),
            FsError::CommandFailed(m) => Self::partial(m),
        }
    }
}

/// 表结构错误 → 出口语义：读写失败归"环境故障、未写盘"（4）；签名、CRC、几何、
/// 条目越界都说明盘上的表不符合规范，属于"请求与现状不匹配"（3）
impl From<crate::table::GptError> for RunOutcome {
    fn from(e: crate::table::GptError) -> Self {
        match e {
            crate::table::GptError::Io(err) => Self::no_mutation(err.to_string()),
            other => Self::refused(other.to_string()),
        }
    }
}

/// 读表失败的文案由调用点保留（`parse failed: …` / `label probe failed: …`），
/// 本函数只负责分类：`GptError::Io` 是读盘失败（4），其余变体都说明盘上的表
/// 不符合规范（3）。只读命令同样按这条判：它们从不写盘，4 只留给真正的 I/O 失败
pub(crate) fn table_parse_failure(e: &crate::table::GptError, msg: String) -> RunOutcome {
    match e {
        crate::table::GptError::Io(_) => RunOutcome::no_mutation(msg),
        _ => RunOutcome::refused(msg),
    }
}

/// MBR 解析的 `io::Error` 同判：`InvalidData` 是条目不可操作（越界、起点 0、两两重叠），
/// 表本身非法 → 3；其余是读盘失败 → 4
pub(crate) fn mbr_parse_failure(e: &std::io::Error, msg: String) -> RunOutcome {
    if e.kind() == std::io::ErrorKind::InvalidData {
        RunOutcome::refused(msg)
    } else {
        RunOutcome::no_mutation(msg)
    }
}

/// 把内层失败并入一个"已越界"的调用点：此时对外结论只能是 `Partial`（盘可能已改变），
/// 内层是 Refused 也只是因为它自己那一层还没写盘——对本层已经不算数了。
/// 存在的意义是让这种"层级差"显式可见，而不是靠 `?` 悄悄把 Refused 透传出去。
/// 不提供 `From<RunOutcome> for io::Error`：压平必须逐点写明
#[cfg(target_os = "linux")]
pub fn into_io_error(e: RunOutcome) -> std::io::Error {
    std::io::Error::other(e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fsops::FsError;

    /// FsError 的分类只在这一处落成出口语义：不认得的类型 / 参数与目标现状不符 ⇒ 拒绝（3），
    /// 工具缺失 ⇒ 不支持（6），环境故障 ⇒ 无持久化修改（4），外部工具非零退出 ⇒ 部分完成（5）
    #[test]
    fn fs_error_maps_to_exit_codes() {
        let code = |e: FsError| RunOutcome::from(e).exit_code();
        assert_eq!(code(FsError::unsupported("no resize tool")), EXIT_UNSUPPORTED);
        assert_eq!(code(FsError::InvalidArgument("bad size".into())), EXIT_REFUSED);
        assert_eq!(code(FsError::ToolMissing("no mkfs.xfs".into())), EXIT_UNSUPPORTED);
        assert_eq!(code(FsError::Io(std::io::Error::other("EIO"))), EXIT_NO_MUTATION);
        assert_eq!(code(FsError::CommandFailed("mkswap failed".into())), EXIT_PARTIAL);
        // 同码不同变体：5 的语义差别（有无"盘可能已改变"警示行）只体现在变体上
        assert!(matches!(
            RunOutcome::from(FsError::CommandFailed("mkswap failed".into())),
            RunOutcome::Partial { .. }
        ));
        assert!(matches!(
            RunOutcome::from(FsError::ToolMissing("no mkfs.xfs".into())),
            RunOutcome::Unsupported { .. }
        ));
    }

    /// 越过 durable boundary 之后，同一分类不再有出口语义：压平即只剩 Partial
    /// （报告里必须带"盘可能已改变"，此时确实无法断言）
    #[test]
    fn fs_error_flattens_to_partial_after_the_boundary() {
        let after: std::io::Error = FsError::unsupported("no resize tool").into();
        assert!(matches!(RunOutcome::from(after), RunOutcome::Partial { .. }));
    }

    /// 上下文只改文案，不改分类
    #[test]
    fn context_keeps_the_variant() {
        assert!(
            matches!(RunOutcome::refused("x").context("doing y"), RunOutcome::Refused(m) if m == "doing y: x")
        );
        assert!(matches!(
            RunOutcome::no_mutation("x").context("doing y"),
            RunOutcome::NoMutation { cause } if cause == "doing y: x"
        ));
        assert!(matches!(
            RunOutcome::partial("x").context("doing y"),
            RunOutcome::Partial { cause, .. } if cause == "doing y: x"
        ));
    }

    #[test]
    fn exit_codes_are_the_contract() {
        assert_eq!(RunOutcome::usage("x").exit_code(), 2);
        assert_eq!(RunOutcome::refused("x").exit_code(), 3);
        assert_eq!(RunOutcome::no_mutation("x").exit_code(), 4);
        assert_eq!(RunOutcome::partial("x").exit_code(), 5);
        assert_eq!(RunOutcome::unsupported("x").exit_code(), 6);
        assert_eq!(RunOutcome::internal("x").exit_code(), 1);
        assert_eq!(RunOutcome::applied_with(Vec::new()).exit_code(), 0);
    }

    /// 每种成因落到哪个退出码都在这里钉住。
    /// 同一码可由不同变体产生，但"哪种成因落哪个码"不能漂
    #[test]
    fn mapping_table_rows_hold() {
        // Applied 的两种未完成形态都是 5：待办非空、内核视图过期
        let pending = RunOutcome::applied_with(vec![Pending::new(1, PendingKind::Fs, "d", "h")]);
        assert_eq!(pending.exit_code(), EXIT_PARTIAL);
        assert!(!pending.is_complete());
        let stale = RunOutcome::Applied { pending: Vec::new(), kernel_sync: KernelSync::Stale };
        assert_eq!(stale.exit_code(), EXIT_PARTIAL);
        assert!(!stale.is_complete());

        // 失败六变体：用法 2、拒绝 3、未写 4、不支持 6、内部 1、部分 5
        let code = |f: RunOutcome| f.exit_code();
        assert_eq!(code(RunOutcome::usage("bad flag")), EXIT_USAGE);
        assert_eq!(code(RunOutcome::refused("no such partition")), EXIT_REFUSED);
        assert_eq!(code(RunOutcome::no_mutation("EIO before the first write")), EXIT_NO_MUTATION);
        assert_eq!(code(RunOutcome::unsupported("tool missing")), EXIT_UNSUPPORTED);
        assert_eq!(code(RunOutcome::internal("invariant broken")), EXIT_INTERNAL);
        assert_eq!(code(RunOutcome::partial("write call reported an error")), EXIT_PARTIAL);

        // 表结构非法归 3（未写盘，改参数或换目标有意义）；表读写失败归 4
        assert_eq!(
            code(RunOutcome::from(crate::table::GptError::InvalidHeader("crc".into()))),
            EXIT_REFUSED
        );
        assert_eq!(
            code(RunOutcome::from(crate::table::GptError::Io(std::io::Error::other("EIO")))),
            EXIT_NO_MUTATION
        );

        // 越过 durable boundary 之后只有两个出口：完整 0、其余 5
        assert_eq!(RunOutcome::applied_with(Vec::new()).exit_code(), EXIT_OK);
        let w = vec![Pending::new(2, PendingKind::Swap, "d", "h")];
        assert_eq!(RunOutcome::applied_with(w).exit_code(), EXIT_PARTIAL);
        assert_eq!(RunOutcome::partial_with("fail after the boundary", Vec::new()).exit_code(), EXIT_PARTIAL);
    }

    /// 收尾失败（journal 未删、资源未释放）只把"已完整应用"降成 5；
    /// 已有更差结论的形态不被这条收尾故障覆盖
    #[test]
    fn cleanup_fence_degrades_only_a_complete_applied() {
        assert_eq!(
            RunOutcome::applied_with(Vec::new()).degrade_partial("journal not removed").exit_code(),
            EXIT_PARTIAL
        );
        // 待办非空本来就是 5，变体保持不变
        let applied_pending = RunOutcome::applied_with(vec![Pending::new(1, PendingKind::Fs, "d", "h")]);
        assert!(matches!(
            applied_pending.degrade_partial("journal not removed"),
            RunOutcome::Applied { .. }
        ));
        // 更差的成因不被降级改写
        assert_eq!(
            RunOutcome::refused("no such partition").degrade_partial("leak").exit_code(),
            EXIT_REFUSED
        );
        assert_eq!(
            RunOutcome::no_mutation("EIO").degrade_partial("leak").exit_code(),
            EXIT_NO_MUTATION
        );
    }

    /// 5 的 stderr 契约：三行齐备（盘可能已改变、证据路径、下一步命令），
    /// 且下一步不是空泛的"重试"
    #[test]
    fn partial_guidance_names_state_evidence_and_next_step() {
        let lines = RunOutcome::partial_guidance_lines();
        assert!(lines[0].contains("may already have been changed"), "{}", lines[0]);
        assert!(lines[1].contains("evidence:"), "{}", lines[1]);
        assert!(lines[2].contains("`diskedit undo`") && lines[2].contains("`diskedit abandon`"), "{}", lines[2]);
    }

    /// 本模块是"后置条件契约 + 退出码的唯一映射点"，不认识任何具体文件系统或工具——
    /// 那类知识属于 fsops/movepart。一旦落进来，它就会以"某类走某工具"的形式改变分类结论，
    /// 而分类错了是静默的。层的归属靠自觉守不住，故设门禁：写下一个名字就立刻红。
    /// 只扫代码：注解描述契约本身（含它覆盖哪些机制）是这一层的本分
    #[test]
    fn knows_no_concrete_filesystem_or_tool() {
        let src = include_str!("outcome.rs");
        let code = src[..src.find("#[cfg(test)]").unwrap_or(src.len())]
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .map(|l| &l[..l.find("//").unwrap_or(l.len())])
            .collect::<Vec<_>>()
            .join("\n")
            .to_lowercase();
        // 门禁自己也要有门禁：截取或去注释若吃掉了全文，下面的断言会全部空过
        assert!(code.contains("exit_refused"), "门禁没扫到代码：截取或去注释把它吃空了");
        for t in [
            // 文件系统类型
            "ntfs", "ext2", "ext3", "ext4", "xfs", "btrfs", "f2fs", "vfat", "exfat", "msdos",
            // 文件系统与卷管理工具
            "mkfs", "mkswap", "fsck", "tune2fs", "resize2fs", "lvm", "pvresize", "lvextend",
        ] {
            assert!(!code.contains(t), "本模块不该提到 {t:?}：那是 fsops/movepart 的知识");
        }
    }
}
