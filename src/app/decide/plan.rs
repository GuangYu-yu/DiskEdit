//! 已判定的计划：有序步骤、摘要与目标身份。
//!
//! 写目标的计划必须携带强身份；只读计划不带身份。构造时计算摘要，之后不再
//! 改动步骤，确认阶段只做比对。

use diskedit_core::fingerprint::{Fingerprint, Strength};

use super::OccupancySnapshot;
use super::digest::plan_digest;
use super::step::{Access, Step, StepKind};

/// 一份有序计划。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Plan {
    steps: Vec<Step>,
    digest: [u8; 32],
    identity: Option<Fingerprint>,
    /// 跨次重跑稳定的请求摘要；没有时为 `None`，记录退回使用完整摘要。
    request: Option<[u8; 32]>,
    /// 计划期的占用现场；`None` 表示这份计划没有采集占用（不适用），
    /// 与探测读不出的 [`crate::platform::Occupancy::Unknown`] 不同。
    occupancy: Option<OccupancySnapshot>,
}

impl Plan {
    /// 构造会写目标的计划；身份必须是可比较等级。
    pub fn write(steps: Vec<Step>, identity: Fingerprint) -> Result<Self, super::Refusal> {
        if identity.strength() != Strength::Strong {
            return Err(super::Refusal::IdentityUnprovable {
                why: "target identity is insufficient for the plan".to_owned(),
            });
        }
        validate_steps(&steps)?;
        let digest = plan_digest(Some(identity), &steps);
        Ok(Self {
            steps,
            digest,
            identity: Some(identity),
            request: None,
            occupancy: None,
        })
    }

    /// 构造只读计划。
    pub fn read_only(steps: Vec<Step>) -> Self {
        let digest = plan_digest(None, &steps);
        Self {
            steps,
            digest,
            identity: None,
            request: None,
            occupancy: None,
        }
    }

    /// 构造落盘由外部工具或内核接口承担的计划。
    ///
    /// 这类计划同样要求确认与强身份：外部工具会改动目标字节或目标之外的
    /// 外部状态。
    pub fn external(steps: Vec<Step>, identity: Fingerprint) -> Result<Self, super::Refusal> {
        if identity.strength() != Strength::Strong {
            return Err(super::Refusal::IdentityUnprovable {
                why: "target identity is insufficient for the external-state plan".to_owned(),
            });
        }
        validate_steps(&steps)?;
        let digest = plan_digest(Some(identity), &steps);
        Ok(Self {
            steps,
            digest,
            identity: Some(identity),
            request: None,
            occupancy: None,
        })
    }

    /// 绑定跨次稳定的请求摘要；记录用它判断能否续跑。
    pub fn with_request_digest(mut self, request: [u8; 32]) -> Self {
        self.request = Some(request);
        self
    }

    /// 绑定计划期的占用现场；执行层在写表前据此比对。
    pub fn with_occupancy(mut self, occupancy: OccupancySnapshot) -> Self {
        self.occupancy = Some(occupancy);
        self
    }

    pub fn steps(&self) -> &[Step] {
        &self.steps
    }

    pub fn digest(&self) -> [u8; 32] {
        self.digest
    }

    /// 记录的比对值：优先请求摘要，否则完整计划摘要。
    pub fn request_digest(&self) -> [u8; 32] {
        self.request.unwrap_or(self.digest)
    }

    pub fn identity(&self) -> Option<Fingerprint> {
        self.identity
    }

    /// 计划期的占用现场；`None` 表示这份计划不涉及占用比对。
    pub fn occupancy(&self) -> Option<&OccupancySnapshot> {
        self.occupancy.as_ref()
    }

    /// 是否包含写目标字节的步骤。
    pub fn writes_target(&self) -> bool {
        self.steps
            .iter()
            .any(|step| step.access() == Access::WritesTarget)
    }

    /// 摘要是否与当前步骤一致。
    pub fn digest_is_current(&self) -> bool {
        plan_digest(self.identity, &self.steps) == self.digest
    }
}

/// 写出前的步骤集合自洽检查：空集合、重复步骤与缺失区间都不是一份可执行计划。
///
/// 空计划会让确认与记录都成立却无事可做；同一步骤出现两次会在执行时重复写同一
/// 区间；会改写目标字节的步骤不声明区间，计划就说不清这一步要写到哪里。三者在
/// 计划成形处拒绝，不留给执行层去猜。
fn validate_steps(steps: &[Step]) -> Result<(), super::Refusal> {
    if steps.is_empty() {
        return Err(super::Refusal::PlanDigestMismatch);
    }
    for (index, step) in steps.iter().enumerate() {
        if steps[..index].contains(step) {
            return Err(super::Refusal::PlanDigestMismatch);
        }
        if writes_target_bytes(&step.kind) && step.regions.is_empty() {
            return Err(super::Refusal::PlanDigestMismatch);
        }
    }
    Ok(())
}

/// 步骤是否会改写目标字节。
///
/// 直接写目标的步骤由执行层落盘；在线扩缩由文件系统工具在挂载的分区上改写。
/// 两者都必须在步骤上声明区间。恢复步骤的回写范围取决于记录内容，内核视图只改
/// 外部状态，二者不需要声明区间。
fn writes_target_bytes(kind: &StepKind) -> bool {
    match kind {
        StepKind::WriteTable { .. }
        | StepKind::AddPartition { .. }
        | StepKind::DeletePartition { .. }
        | StepKind::ResizePartition { .. }
        | StepKind::SetProperty { .. }
        | StepKind::MakeFilesystem { .. }
        | StepKind::GrowFilesystem { .. }
        | StepKind::ShrinkFilesystem { .. }
        | StepKind::RebuildSwap { .. }
        | StepKind::GrowFilesystemOnline { .. }
        | StepKind::ShrinkFilesystemOnline { .. }
        | StepKind::CopyPartition { .. }
        | StepKind::MovePartition { .. }
        | StepKind::LvmPvShrink { .. }
        | StepKind::LvmGrowChain { .. } => true,
        StepKind::Inspect
        | StepKind::Check { .. }
        | StepKind::ListRecords
        | StepKind::VerifyRecord { .. }
        | StepKind::ScanRecords
        | StepKind::RefreshKernelView { .. }
        | StepKind::RecoverRecords { .. } => false,
    }
}

#[cfg(test)]
mod tests {
    use diskedit_core::region::Region;

    use super::*;
    use crate::app::decide::Refusal;
    use crate::app::decide::step::StepKind;

    fn identity() -> Fingerprint {
        Fingerprint::strong([0x21; 32])
    }

    fn write_step() -> Step {
        Step::new(
            StepKind::VerifyRecord {
                token: "abc".to_owned(),
            },
            vec![Region::sized(0, 512).expect("区间")],
        )
    }

    #[test]
    fn an_empty_plan_is_refused_before_it_can_be_confirmed() {
        assert!(matches!(
            Plan::write(Vec::new(), identity()),
            Err(Refusal::PlanDigestMismatch)
        ));
        assert!(matches!(
            Plan::external(Vec::new(), identity()),
            Err(Refusal::PlanDigestMismatch)
        ));
    }

    #[test]
    fn a_repeated_step_is_refused() {
        let step = write_step();
        assert!(matches!(
            Plan::write(vec![step.clone(), step], identity()),
            Err(Refusal::PlanDigestMismatch)
        ));
    }

    #[test]
    fn a_distinct_step_set_is_accepted() {
        let first = write_step();
        let second = Step::new(StepKind::Inspect, Vec::new());
        let plan = Plan::write(vec![first, second], identity()).expect("互不相同的步骤可成计划");
        assert_eq!(plan.steps().len(), 2);
    }
}
