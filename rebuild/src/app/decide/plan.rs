//! 已判定的计划：有序步骤、摘要与目标身份。
//!
//! 写目标的计划必须携带强身份；只读计划不带身份。构造时计算摘要，之后不再
//! 改动步骤，确认阶段只做比对。

use diskedit_core::fingerprint::{Fingerprint, Strength};

use super::digest::plan_digest;
use super::step::{Access, Step};

/// 一份有序计划。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Plan {
    steps: Vec<Step>,
    digest: [u8; 32],
    identity: Option<Fingerprint>,
}

impl Plan {
    /// 构造会写目标的计划；身份必须是可比较等级。
    pub fn write(steps: Vec<Step>, identity: Fingerprint) -> Result<Self, super::Refusal> {
        if identity.strength() != Strength::Strong {
            return Err(super::Refusal::IdentityUnprovable {
                why: "计划所需的目标身份不足".to_owned(),
            });
        }
        let digest = plan_digest(Some(identity), &steps);
        Ok(Self {
            steps,
            digest,
            identity: Some(identity),
        })
    }

    /// 构造只读计划。
    pub fn read_only(steps: Vec<Step>) -> Self {
        let digest = plan_digest(None, &steps);
        Self {
            steps,
            digest,
            identity: None,
        }
    }

    /// 构造不写目标字节、但会改变外部状态（例如记录目录）的计划。
    ///
    /// 这类计划同样要求确认与强身份：恢复记录是写操作，只是不落到目标字节上。
    pub fn external(steps: Vec<Step>, identity: Fingerprint) -> Result<Self, super::Refusal> {
        if identity.strength() != Strength::Strong {
            return Err(super::Refusal::IdentityUnprovable {
                why: "外部状态计划所需的目标身份不足".to_owned(),
            });
        }
        let digest = plan_digest(Some(identity), &steps);
        Ok(Self {
            steps,
            digest,
            identity: Some(identity),
        })
    }

    pub fn steps(&self) -> &[Step] {
        &self.steps
    }

    pub fn digest(&self) -> [u8; 32] {
        self.digest
    }

    pub fn identity(&self) -> Option<Fingerprint> {
        self.identity
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
