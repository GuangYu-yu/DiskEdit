//! 载体步骤：计划步骤的唯一写入路径与放置分派。
//!
//! 四类步骤 [`CarrierStep`]（表提交、数据搬移、文件系统、探测）在 Native 放置下由
//! [`native`] 映射到 [`crate::execution`] 的单步实现。计划驱动 [`run_plan`] 预扫描一次，
//! 把每个写入点经 [`run_step`] 交出；命令层不直接调工具或写表。
//!
//! 每类步骤的结果只有两值 [`StepOutcome`]：`Applied` 与 `Unknown`。**"没做成"不是
//! 第三值**——它以 `Err(RunOutcome)` 走与宿主进程内失败完全相同的那条通道，于是
//! "失败"在全流程只有一种表示；`Unknown` 只留给"宿主无从断言、必须停在原地"。跨进程
//! 或跨载体的提交依据仍要由探测与记录复核给出。

mod dispatch;
mod native;
pub(crate) mod placement;
pub(crate) mod protocol;
pub(crate) mod qemu;
/// VM 载体的宿主侧会话。其余平台没有可用的载体，放置决定不会给出 `Vm`，
/// 因此这一模块在那里没有调用点——"存在但不可达"由这条按平台的 allow 说明，
/// 不是全局关闭死代码检查
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
mod remote;
pub(crate) mod worker;

use crate::dev::WriteSource;
use crate::domain::ContentKind;
use crate::execution::copy::CopyPartition;
use crate::execution::fs::FsResize;
use crate::execution::mkfs::FsCreate;
use crate::execution::relocate::Relocate;
use crate::execution::set::FsSet;
use crate::execution::swap::SwapRebuild;
use crate::execution::{
    PreparedStep, StepFlow, StepSink, TransferState, journal_state, prepare_steps,
    verify_tables_before_write,
};
use crate::outcome::{Pending, RunOutcome};
use crate::planner::ConfirmedPlan;
use crate::table::TableStep;

use dispatch::Advance;

/// 执行放置：本作业的步骤在宿主进程内还是经 VM 载体执行
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Placement {
    Native,
    Vm,
}

/// 数据搬移类步骤
#[derive(Clone, Copy)]
pub(crate) enum TransferStep<'a> {
    Relocate(&'a Relocate),
    /// 复制条目连同它与目标之间的块字节数
    Copy { step: &'a CopyPartition, chunk: u64 },
}

/// 文件系统类步骤
#[derive(Clone, Copy)]
pub(crate) enum FsStep<'a> {
    Resize(&'a FsResize),
    Create(&'a FsCreate),
    Set(&'a FsSet),
    RebuildSwap(&'a SwapRebuild),
    /// 表提交之后按新起点修正 NTFS 的 HiddenSectors
    HiddenSectors(&'a Relocate),
}

/// 四类载体步骤。`Probe` 只读，用于复核写入后的盘面事实
#[derive(Clone, Copy)]
pub(crate) enum CarrierStep<'a> {
    TableCommit(&'a TableStep),
    Transfer(TransferStep<'a>),
    Fs(FsStep<'a>),
    Probe(ProbeRequest),
}

/// 探测请求：分区号与它当前的落点（LBA 与扇区数）
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ProbeRequest {
    pub(crate) part: u32,
    pub(crate) first_lba: u64,
    pub(crate) sectors: u64,
}

/// 探测的只读结论
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ProbeReport {
    pub(crate) part: u32,
    pub(crate) content: ContentKind,
}

/// 载体步骤的两值结果。**"没做成"不在其中**：它以 `Err(RunOutcome)` 走与宿主
/// 进程内失败完全相同的那条通道，因此"失败"在全流程只有一种表示。
///
/// 三值里的第三值只剩"不知道"——那正是宿主无从断言、必须停在原地的那一档。
/// 两个值都只能由 [`StepRun`] 的构造器产生、只能由 [`StepRun::verdict`] 读取
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StepOutcome {
    Applied,
    Unknown,
}

/// 一次步骤执行的结果。
///
/// `Unknown` 必须带成因：报告要能说出"为什么不知道"，因此不存在"结论给不出
/// 成因"的实例
#[must_use]
pub(crate) struct StepRun {
    pub(crate) outcome: StepOutcome,
    /// 未达成的后置条件（FS 步把它交成待办，不折进结论里）
    pub(crate) pending: Vec<Pending>,
    pub(crate) probe: Option<ProbeReport>,
    /// `Unknown` 的成因：没有待办可说明时，报告靠它说明"为什么不知道"
    pub(crate) unfinished: Option<String>,
}

impl StepRun {
    pub(crate) fn applied() -> Self {
        Self { outcome: StepOutcome::Applied, pending: Vec::new(), probe: None, unfinished: None }
    }

    pub(crate) fn with_pending(pending: Vec<Pending>) -> Self {
        let outcome =
            if pending.is_empty() { StepOutcome::Applied } else { StepOutcome::Unknown };
        Self { outcome, pending, probe: None, unfinished: None }
    }

    pub(crate) fn probed(report: ProbeReport) -> Self {
        Self { probe: Some(report), ..Self::applied() }
    }

    /// 写没写不知道：传输中断、外部工具没有回答、落点未证实
    pub(crate) fn unknown(reason: impl Into<String>) -> Self {
        Self {
            outcome: StepOutcome::Unknown,
            pending: Vec::new(),
            probe: None,
            unfinished: Some(reason.into()),
        }
    }

    /// 探测没能证实这一步：结论退成 `Unknown`，已带的待办不再成立
    pub(crate) fn probe_unconfirmed(&mut self, reason: impl Into<String>) {
        self.outcome = StepOutcome::Unknown;
        self.pending.clear();
        self.unfinished = Some(reason.into());
    }

    /// 线格式重建。只接受"算数"与"不知道"两种结论：宿主侧判定过的失败
    /// 不走这里，它在 [`super::protocol::Decoded`] 里直接成为 `Err`
    pub(crate) fn decoded(
        outcome: StepOutcome,
        pending: Vec<Pending>,
        probe: Option<ProbeReport>,
        cause: Option<String>,
    ) -> Self {
        let unfinished = match outcome {
            StepOutcome::Unknown => cause,
            StepOutcome::Applied => None,
        };
        Self { outcome, pending, probe, unfinished }
    }
}

/// "这一步算数"的凭据。**唯一的构造点是 [`StepRun::verdict`] 的 `Applied` 分支**
/// （构造器 `pub(super)`，只有本模块能造），而推进游标、落"步骤完成"都要求出示它。
/// 由此"结论不是 `Applied` 却已经推进了游标"不是靠约定避免，而是编译不过：
/// 没有算数的凭据，就没有推进的入口
#[derive(Clone, Copy, Debug)]
pub(crate) struct StepApplied(());

impl StepApplied {
    /// 只对 `exec` 内可见：`dispatch` 用它执行记录动作，别处拿不到
    pub(super) fn evidence() -> Self {
        Self(())
    }
}

/// 一步判定之后计划的走向。两态：失败走 `Err`，继续与停步都在这里
pub(crate) enum Verdict {
    /// 这一步算数，计划继续；凭据就是唯一可以推进的东西
    Applied(StepApplied),
    /// 结论不确定：不改记录、不推进游标，整份计划停在此步，重跑从同一处继续
    Stopped,
}

/// 记录动作执行完之后计划的走向：失败仍走 `Err`，这里只剩"继续"与"停住"，
/// 且只有 `Verdict::Applied` 的那一支能走到"继续"
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Progress {
    /// 这一步算数且记录动作已落，计划继续
    Continue,
    /// 结论不确定：不改记录、不推进游标，整份计划在此停住
    Stop,
}

impl StepRun {
    /// **两值结论的唯一解释点。**
    ///
    /// `Applied` 才产出 [`StepApplied`]——推进游标与"步骤完成"的落盘以它为凭据
    /// （`TransferState::note_step` / `advance_entry` 的入参就是它），因此"结论不是
    /// `Applied` 却已经推进了游标"不是靠约定避免，而是编译不过。`Unknown` 留下一笔
    /// 待办（没有更具体待办时用成因说明）并停在原地，记录仍指着这一步，重跑从同一处
    /// 继续。失败由调用方以 `Err` 交回，与进程内执行走同一条通道
    pub(crate) fn verdict(self, pending: &mut Vec<Pending>) -> Verdict {
        match self.outcome {
            StepOutcome::Applied => {
                pending.extend(self.pending);
                Verdict::Applied(StepApplied::evidence())
            }
            StepOutcome::Unknown => {
                if self.pending.is_empty() {
                    let detail = self
                        .unfinished
                        .unwrap_or_else(|| "the carrier gave no answer for this step".to_string());
                    // 分区号 0：停步是整份计划层面的，不归某个分区
                    pending.push(Pending::new(
                        0,
                        crate::outcome::PendingKind::Other("step stopped"),
                        detail,
                        "re-run the command; the record still points at this step",
                    ));
                } else {
                    pending.extend(self.pending);
                }
                Verdict::Stopped
            }
        }
    }
}

/// 在宿主进程内执行一个载体步骤。只对分派层可见：这是"执行但不判定"的半成品，
/// 能拿到它的调用点必须自己负责判定与记录，因此不给 `exec` 之外任何入口
pub(super) fn run_step(
    src: &mut WriteSource,
    step: CarrierStep<'_>,
    sector_size: u64,
    state: &mut TransferState,
    sink: &mut dyn StepSink,
) -> Result<StepRun, RunOutcome> {
    native::execute(src, step, sector_size, state, sink)
}

/// **已提交态的回填入口**：表项已在目标区间、引导扇区仍带旧起点时，重跑只补这一处，
/// 不再走计划步骤。
///
/// 与计划路径的 [`crate::execution::step_hidden_sectors`] 分工明确：那一侧是步骤实现，
/// 自己识别内容类型、按位移决定要不要修；这里由调用方先判定"这是 NTFS、起点变了"，
/// 只负责把一次修补交出去。两者都落到同一个写入实现（`fix_ntfs_hidden_sectors`），
/// 名字上也因此不再重合
pub(crate) fn backfill_hidden_sectors(
    src: &mut WriteSource,
    first_lba: u64,
    sector_size: u64,
    part: u32,
    log: &mut dyn FnMut(&str),
) -> std::io::Result<()> {
    crate::execution::fix_ntfs_hidden_sectors(src, first_lba, sector_size, part, log)
}

/// 执行一份计划并交出待办。放置在这里一次决定，之后整份计划只用一个执行器。
///
/// `extra` 是这条命令后续计划的能力需求：命令拆成多份计划时，放置按整条命令问一次，
/// 步骤之间不再换执行器
pub(crate) fn run_plan(
    src: &mut WriteSource,
    plan: &ConfirmedPlan,
    chunk_bytes: Option<u64>,
    sink: &mut dyn StepSink,
    pending: &mut Vec<Pending>,
    extra: &[crate::fsops::Needs],
) -> Result<StepFlow, RunOutcome> {
    let sector_size = plan.plan().sector_size().get();
    let record = src.record_journal().and_then(|j| j.relocation());
    let steps = prepare_steps(src, plan, chunk_bytes, sector_size, record)?;
    // 数据搬移与表写入都在这个核对之后：外部在计划读出之后改了表，要在这里以 3 拒绝，
    // 而不是搬完数据才在表步发现
    verify_tables_before_write(src, &steps)?;
    let decision = placement::decide(src, &steps, sector_size, extra)?;
    if let Some(journal) = src.record_journal_mut() {
        journal
            .record_audit(decision.audit())
            .map_err(|e| RunOutcome::no_mutation(e.record_failure("the recovery record")))?;
    }
    let mut driver = dispatch::Driver::open(src, &decision, &steps, sector_size)?;
    let mut state = journal_state(src, &steps)?;
    // 只有搬移计划才带"搬完之后的那一次扩容"；delete/add/set 的单表步与它无关，
    // 不能让测试注入点在那些命令上误触发
    let has_transfer = steps.iter().any(|s| matches!(s, PreparedStep::Transfer { .. }));
    let mut entry = 0u32;
    // 记录里已提交的搬移条目整组跳过：计数在进循环前取一次，循环里只递减，
    // 不按条目回读进度字段
    let mut committed_entries = state.durable().0;
    let mut grow_faulted = false;
    for (index, step) in steps.into_iter().enumerate() {
        match step {
            PreparedStep::Transfer { reloc, commit, swap } => {
                // 记录里已提交的条目整组跳过：数据与表都不碰
                if committed_entries > 0 {
                    committed_entries -= 1;
                    entry += 1;
                    continue;
                }
                if swap.is_none() {
                    // 数据搬移算数不等于条目算数：条目要等表项落位、重建做完再一起收口
                    if driver
                        .step(
                            dispatch::StepCall {
                                src: &mut *src,
                                state: &mut state,
                                sink: &mut *sink,
                                index,
                                pending: &mut *pending,
                            },
                            CarrierStep::Transfer(TransferStep::Relocate(&reloc)),
                            sector_size,
                            Advance::Nothing,
                        )?
                        .is_stopped()
                    {
                        return Ok(StepFlow::Stopped);
                    }
                }
                crate::movepart::fault_before_entry_commit();
                sink.entry_table_started(src, entry)?;
                if driver
                    .step(
                        dispatch::StepCall {
                            src: &mut *src,
                            state: &mut state,
                            sink: &mut *sink,
                            index,
                            pending: &mut *pending,
                        },
                        CarrierStep::TableCommit(commit),
                        sector_size,
                        Advance::Nothing,
                    )?
                    .is_stopped()
                {
                    return Ok(StepFlow::Stopped);
                }
                // 换区条目的表项刚落位、内容尚未重建：重放必须在同一个记录游标上重做重建
                if swap.is_some() {
                    crate::movepart::fault_after_swap_commit(entry as usize);
                }
                // 起点变了的 NTFS 分区要修 HiddenSectors：数据是字节拷贝，boot sector 还带着旧值。
                // 排在表提交之后、推进游标之前——这一步没做成，重跑就该重试它。
                // 换区条目走另一支：内容可弃，表项已落位，重建是它的后置条件；没做成
                // 就停在这里，后续条目不能在这条尚未可用的布局上继续搬，扩容更不能做
                if let Some(swap_step) = swap {
                    // 重建是换区条目的后置条件：没做成就不推进条目游标
                    if driver
                        .step(
                            dispatch::StepCall {
                                src: &mut *src,
                                state: &mut state,
                                sink: &mut *sink,
                                index,
                                pending: &mut *pending,
                            },
                            CarrierStep::Fs(FsStep::RebuildSwap(&swap_step)),
                            sector_size,
                            Advance::Entry {
                                before: dispatch::FaultPoint::AfterSwapEntry(entry as usize),
                            },
                        )?
                        .is_stopped()
                    {
                        return Ok(StepFlow::Stopped);
                    }
                } else {
                    // 组末这一步同时收口整个条目：HiddenSectors 落定即条目算数
                    if driver
                        .step(
                            dispatch::StepCall {
                                src: &mut *src,
                                state: &mut state,
                                sink: &mut *sink,
                                index,
                                pending: &mut *pending,
                            },
                            CarrierStep::Fs(FsStep::HiddenSectors(&reloc)),
                            sector_size,
                            Advance::Entry { before: dispatch::FaultPoint::None },
                        )?
                        .is_stopped()
                    {
                        return Ok(StepFlow::Stopped);
                    }
                    sink.note(&format!(
                        "partition {} relocated (delta {} sectors)",
                        reloc.part,
                        reloc.dst_first.abs_diff(reloc.src_first)
                    ));
                }
                crate::movepart::fault_after_entry_commit(entry as usize);
                entry += 1;
            }
            PreparedStep::Copy { copy, commit } => {
                let chunk = chunk_bytes
                    .expect("prepare_steps refuses a copy plan without a chunk size");
                if driver
                    .step(
                        dispatch::StepCall {
                            src: &mut *src,
                            state: &mut state,
                            sink: &mut *sink,
                            index,
                            pending: &mut *pending,
                        },
                        CarrierStep::Transfer(TransferStep::Copy { step: &copy, chunk }),
                        sector_size,
                        Advance::Nothing,
                    )?
                    .is_stopped()
                {
                    return Ok(StepFlow::Stopped);
                }
                // 复制步骤只在写完数据之后返回 Ok；表步之前先落"表可能已写"
                sink.table_step_started(src)?;
                // 表提交落定 = 这一步整体算数，于是推进并落表完成
                if driver
                    .step(
                        dispatch::StepCall {
                            src: &mut *src,
                            state: &mut state,
                            sink: &mut *sink,
                            index,
                            pending: &mut *pending,
                        },
                        CarrierStep::TableCommit(commit),
                        sector_size,
                        Advance::TableStep { before: dispatch::FaultPoint::None },
                    )?
                    .is_stopped()
                {
                    return Ok(StepFlow::Stopped);
                }
                crate::movepart::fault_after_table_step();
            }
            PreparedStep::TableOnly(commit) => {
                // 全部条目搬完之后的那一个表步骤就是扩容：崩溃注入点在这个相位边界上
                if has_transfer && !grow_faulted {
                    crate::movepart::fault_before_grow();
                    grow_faulted = true;
                }
                // 表步之前先落"表可能已写"；落不下去就不写表
                sink.table_step_started(src)?;
                if driver
                    .step(
                        dispatch::StepCall {
                            src: &mut *src,
                            state: &mut state,
                            sink: &mut *sink,
                            index,
                            pending: &mut *pending,
                        },
                        CarrierStep::TableCommit(commit),
                        sector_size,
                        Advance::TableStep {
                            // 表已写完而记录未推进：注入点必须落在推进之前
                            before: dispatch::FaultPoint::AfterTableWrite { grow: has_transfer },
                        },
                    )?
                    .is_stopped()
                {
                    return Ok(StepFlow::Stopped);
                }
                crate::movepart::fault_after_table_step();
            }
            PreparedStep::FsResize(step) => {
                // 文件系统没做成、或落点未证实：表已扩而空间尚未可用。游标不推进，
                // 重跑从这一处继续——推进了就会把没做成的扩容当成做过
                if driver
                    .step(
                        dispatch::StepCall {
                            src: &mut *src,
                            state: &mut state,
                            sink: &mut *sink,
                            index,
                            pending: &mut *pending,
                        },
                        CarrierStep::Fs(FsStep::Resize(&step)),
                        sector_size,
                        Advance::FsStep,
                    )?
                    .is_stopped()
                {
                    return Ok(StepFlow::Stopped);
                }
            }
            PreparedStep::FsCreate(step) => {
                // 建立步骤只出现在单步计划里，失败原因直接沿用 fsops 的措辞，不加步骤前缀
                if driver
                    .step(
                        dispatch::StepCall {
                            src: &mut *src,
                            state: &mut state,
                            sink: &mut *sink,
                            index,
                            pending: &mut *pending,
                        },
                        CarrierStep::Fs(FsStep::Create(&step)),
                        sector_size,
                        Advance::FsStep,
                    )?
                    .is_stopped()
                {
                    return Ok(StepFlow::Stopped);
                }
            }
            PreparedStep::FsSet(step) => {
                // 属性步骤同样只出现在单步计划里，屏障在启动外部工具之前落盘
                if driver
                    .step(
                        dispatch::StepCall {
                            src: &mut *src,
                            state: &mut state,
                            sink: &mut *sink,
                            index,
                            pending: &mut *pending,
                        },
                        CarrierStep::Fs(FsStep::Set(&step)),
                        sector_size,
                        Advance::FsStep,
                    )?
                    .is_stopped()
                {
                    return Ok(StepFlow::Stopped);
                }
            }
        }
    }
    Ok(StepFlow::Complete)
}

impl Progress {
    fn is_stopped(self) -> bool {
        self == Progress::Stop
    }
}

/// 执行一份不含后置条件步骤的计划：表步骤、FS 建立与属性步骤都不留未达成的后置条件。
///
/// 待办非空说明装配出了本命令不该有的步骤（只有换区重建会产生待办）。按 5 带出明细
/// 而不是丢弃——报成内部错误会把"盘已改变"洗成"未产生持久化修改"。
pub(crate) fn run_plan_complete(
    src: &mut WriteSource,
    plan: &ConfirmedPlan,
    chunk_bytes: Option<u64>,
    sink: &mut dyn StepSink,
) -> Result<(), RunOutcome> {
    let mut pending = Vec::new();
    match run_plan(src, plan, chunk_bytes, sink, &mut pending, &[])? {
        StepFlow::Complete if pending.is_empty() => Ok(()),
        _ => Err(RunOutcome::partial_postcondition(
            "a step of this plan left its postcondition unmet",
            pending,
        )),
    }
}
