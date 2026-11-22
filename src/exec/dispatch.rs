//! 步骤的放置分派：同一步骤在宿主进程内执行，还是经 VM 载体执行。
//!
//! 分派在这里一次完成，`run_plan` 只拿一个 [`Driver`]。**跨载体的屏障也在这里落**：
//! 一旦步骤交给另一个进程，宿主就失去"首字节之前"这个时点，因此交出之前先让记录
//! 承认"目标可能已被修改"，而不是等结果回来再补——补不回来的那次崩溃正是屏障要防的

use super::{
    CarrierStep, Placement, PreparedStep, Progress, StepApplied, StepRun, Verdict, placement,
    protocol, remote,
};
use crate::dev::WriteSource;
use crate::execution::{StepSink, TransferState};
use crate::outcome::RunOutcome;

/// 一步算数之后要做的记录动作。**推进只有这一个入口**：调用方选"推进什么"，
/// 但执行它必须出示 `Applied` 的凭据，因此"没判定就推进"在编译期就不成立
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Advance {
    /// 不推进：整组搬移里的数据、表项、重建各步都由组末的条目游标一起收口
    Nothing,
    /// 一个条目整组完成：推进条目游标并落"表步骤完成"
    Entry { before: FaultPoint },
    /// 一步表提交完成
    TableStep { before: FaultPoint },
    /// 一步文件系统动作完成
    FsStep,
}

/// 推进之前必须经过的相位边界。崩溃注入点在这里而不在调用点：它们的语义就是
/// "记录尚未推进"，放在推进之后会把这个语义反掉
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum FaultPoint {
    None,
    /// 换区条目的重建已落位、条目游标尚未推进
    AfterSwapEntry(usize),
    /// 表已写完、表步骤尚未推进；`grow` 时再叠一个"扩容表已落、记录未提交"的边界
    AfterTableWrite { grow: bool },
}

impl FaultPoint {
    fn fire(self) {
        match self {
            Self::None => {}
            Self::AfterSwapEntry(entry) => crate::movepart::fault_after_swap_entry(entry),
            Self::AfterTableWrite { grow } => {
                crate::movepart::fault_after_table_write();
                if grow {
                    crate::movepart::fault_after_grow_table_write();
                }
            }
        }
    }
}


/// 一次步骤执行的上下文：目标写句柄、进度状态、落盘回调、步骤序号与待办。
/// 打成一个对象是为了让"执行并判定"只有一个入口，且签名不靠九个位置参数维持
pub(super) struct StepCall<'a> {
    pub src: &'a mut WriteSource,
    pub state: &'a mut TransferState,
    pub sink: &'a mut dyn StepSink,
    pub index: usize,
    pub pending: &'a mut Vec<crate::outcome::Pending>,
}

/// 一份计划的执行器：Native 直接调用单步实现，VM 保持一条会话
pub(super) enum Driver {
    Native,
    /// 其余平台的放置决定不会走到这一支：VM 载体不存在，所以这一变体在那里没有
    /// 构造点。按平台说明，而不是全局关闭死代码检查
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    Vm(remote::Session),
}

impl Driver {
    pub(super) fn open(
        src: &WriteSource,
        decision: &placement::Decision,
        steps: &[PreparedStep<'_>],
        sector_size: u64,
    ) -> Result<Self, RunOutcome> {
        match decision.placement {
            Placement::Native => Ok(Self::Native),
            Placement::Vm => Self::open_vm(src, decision, steps, sector_size),
        }
    }

    #[cfg(target_os = "linux")]
    fn open_vm(
        src: &WriteSource,
        decision: &placement::Decision,
        steps: &[PreparedStep<'_>],
        sector_size: u64,
    ) -> Result<Self, RunOutcome> {
        let bundle = decision
            .bundle
            .as_ref()
            .ok_or_else(|| RunOutcome::internal("a VM placement carries no carrier bundle"))?;
        Ok(Self::Vm(remote::Session::open_vm(bundle, src, sector_size, job_label(steps))?))
    }

    /// 本平台没有 VM 载体：放置决定不会给出 `Vm`，走到这里说明装配出了问题
    #[cfg(not(target_os = "linux"))]
    fn open_vm(
        _src: &WriteSource,
        _decision: &placement::Decision,
        _steps: &[PreparedStep<'_>],
        _sector_size: u64,
    ) -> Result<Self, RunOutcome> {
        Err(RunOutcome::internal("the VM carrier is not available on this platform"))
    }

    /// **执行一步并作出决定。** 分派、判定与"算数之后的那一笔记录"在这里合成一次，
    /// 调用方拿到的只有"继续/停住"——它拿不到原始结果，也就没有"执行了却不判定"
    /// 或者"判定了却不记录"的第二条路。步骤序号只在这里补进错误文案
    pub(super) fn step(
        &mut self,
        call: StepCall<'_>,
        step: CarrierStep<'_>,
        sector_size: u64,
        advance: Advance,
    ) -> Result<Progress, RunOutcome> {
        let StepCall { src, state, sink, index, pending } = call;
        let run = self
            .run(src, step, sector_size, state, sink)
            .map_err(|e| e.context(&format!("plan step {}", index + 1)))?; // audit-allow: display-only
        match run.verdict(pending) {
            Verdict::Applied(applied) => {
                advance.record(applied, src, state, sink, index)?;
                Ok(Progress::Continue)
            }
            Verdict::Stopped => Ok(Progress::Stop),
        }
    }

    /// 原始分派。只对 `step` 可见：这一步是唯一能让结果与记录配对的入口
    fn run(
        &mut self,
        src: &mut WriteSource,
        step: CarrierStep<'_>,
        sector_size: u64,
        state: &mut TransferState,
        sink: &mut dyn StepSink,
    ) -> Result<StepRun, RunOutcome> {
        match self {
            Self::Native => super::run_step(src, step, sector_size, state, sink),
            Self::Vm(session) => {
                // 目标字节从此由另一个进程写。宿主在交出这一步之前落屏障：
                // 此后无论崩溃、超时还是断连，记录都承认"可能已写"
                if let Err(e) = src.mark_non_reversible() {
                    let why = e.record_failure("the carrier barrier");
                    return Err(crate::outcome::choose(
                        RunOutcome::partial(why.clone()),
                        RunOutcome::no_mutation(why),
                    ));
                }
                let wire = protocol::WireStep::try_from(step)?;
                session.run_step(src, &wire, state, sink)
            }
        }
    }
}

impl Advance {
    /// 凭据在手才执行记录动作。`state` 的游标推进方法与这里一一对应，
    /// 它们同样要求 [`StepApplied`]——两处都是"没有算数证据就没有推进入口"
    fn record(
        self,
        applied: StepApplied,
        src: &mut WriteSource,
        state: &mut TransferState,
        sink: &mut dyn StepSink,
        index: usize,
    ) -> Result<(), RunOutcome> {
        match self {
            Self::Nothing => {}
            Self::Entry { before: point } => {
                point.fire();
                state.advance_entry(applied);
                sink.table_step_done(src, index, &*state)?;
            }
            Self::TableStep { before: point } => {
                point.fire();
                state.note_step(applied);
                sink.table_step_done(src, index, &*state)?;
            }
            Self::FsStep => {
                state.note_step(applied);
                sink.fs_step_done(src, index, &*state)?;
            }
        }
        Ok(())
    }
}

/// 握手里的作业类别：只用于会话诊断，不参与判定
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn job_label(steps: &[PreparedStep<'_>]) -> &'static str {
    if steps.iter().any(|step| matches!(step, PreparedStep::Transfer { .. })) {
        "transfer"
    } else if steps.iter().any(|step| {
        matches!(
            step,
            PreparedStep::Copy { .. }
                | PreparedStep::FsResize(_)
                | PreparedStep::FsCreate(_)
                | PreparedStep::FsSet(_)
        )
    }) {
        "filesystem"
    } else {
        "table"
    }
}
