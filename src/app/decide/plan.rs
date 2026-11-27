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
    /// 跨次重跑稳定的请求摘要；没有时为 `None`，记录退回使用完整摘要。
    request: Option<[u8; 32]>,
}

impl Plan {
    /// 构造会写目标的计划；身份必须是可比较等级。
    pub fn write(steps: Vec<Step>, identity: Fingerprint) -> Result<Self, super::Refusal> {
        if identity.strength() != Strength::Strong {
            return Err(super::Refusal::IdentityUnprovable {
                why: "target identity is insufficient for the plan".to_owned(),
            });
        }
        let digest = plan_digest(Some(identity), &steps);
        Ok(Self {
            steps,
            digest,
            identity: Some(identity),
            request: None,
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
        }
    }

    /// 构造不写目标字节、但会改变外部状态（例如记录目录）的计划。
    ///
    /// 这类计划同样要求确认与强身份：恢复记录是写操作，只是不落到目标字节上。
    pub fn external(steps: Vec<Step>, identity: Fingerprint) -> Result<Self, super::Refusal> {
        if identity.strength() != Strength::Strong {
            return Err(super::Refusal::IdentityUnprovable {
                why: "target identity is insufficient for the external-state plan".to_owned(),
            });
        }
        let digest = plan_digest(Some(identity), &steps);
        Ok(Self {
            steps,
            digest,
            identity: Some(identity),
            request: None,
        })
    }

    /// 绑定跨次稳定的请求摘要；记录用它判断能否续跑。
    pub fn with_request_digest(mut self, request: [u8; 32]) -> Self {
        self.request = Some(request);
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
