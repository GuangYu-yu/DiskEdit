//! 确认：把计划与其摘要、目标身份绑定后交给执行层。
//!
//! 只有 [`confirm`] 能产出 [`ConfirmedPlan`]；执行层只接受该类型。

use diskedit_core::evidence::Observation;
use diskedit_core::fingerprint::Fingerprint;

use super::plan::Plan;
use super::refusal::Refusal;
use super::step::Step;

/// 已确认、可执行的计划。
///
/// 只能由 [`confirm`] 产出；不实现 `Clone`，一份确认不会复制成两份可执行副本。
#[derive(Debug, PartialEq, Eq)]
pub struct ConfirmedPlan {
    steps: Vec<Step>,
    digest: [u8; 32],
    request_digest: [u8; 32],
    identity: Fingerprint,
}

impl ConfirmedPlan {
    pub fn steps(&self) -> &[Step] {
        &self.steps
    }

    pub fn digest(&self) -> [u8; 32] {
        self.digest
    }

    /// 记录的比对值：跨次重跑稳定的请求摘要。
    pub fn request_digest(&self) -> [u8; 32] {
        self.request_digest
    }

    pub fn identity(&self) -> Fingerprint {
        self.identity
    }
}

/// 校验确认旗标、目标身份与计划摘要。
pub fn confirm(
    plan: Plan,
    confirmed: bool,
    current: &Observation<Fingerprint>,
) -> Result<ConfirmedPlan, Refusal> {
    if !confirmed {
        return Err(Refusal::ConfirmationRequired);
    }
    if !plan.digest_is_current() {
        return Err(Refusal::PlanDigestMismatch);
    }
    let planned = plan.identity().ok_or_else(|| Refusal::IdentityUnprovable {
        why: "a read-only plan cannot be confirmed as a write plan".to_owned(),
    })?;
    let observed = match current {
        Observation::Known { value, .. } => *value,
        Observation::Unknown { why } => {
            return Err(Refusal::IdentityUnprovable { why: why.clone() });
        }
        Observation::NotApplicable { why } => {
            return Err(Refusal::IdentityUnprovable { why: why.clone() });
        }
    };
    if !planned.matches(observed) {
        return Err(Refusal::IdentityChanged);
    }
    Ok(ConfirmedPlan {
        steps: plan.steps().to_vec(),
        digest: plan.digest(),
        request_digest: plan.request_digest(),
        identity: planned,
    })
}
