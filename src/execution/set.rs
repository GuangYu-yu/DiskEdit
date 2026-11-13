//! FsSet 步骤：按属性改写分区文件系统内部的 label 或 UUID。
//!
//! 载荷自洽在首写前判定；改写本身仍走 [`fsops::set_label`] / [`fsops::set_uuid`]——
//! 工具映射与参数拼装都在那里，本层只把计划里的属性与类型取出来交给它。随机 UUID 在
//! 装配期按目标能力落地：能接受具体值的 FS 拿到文本，只能自生成的 FS 拿到生成意图。

use crate::dev::WriteSource;
use crate::domain::ContentKind;
use crate::fsops::{self, UuidRequest};
use crate::outcome::RunOutcome;
use crate::planner::{FsSetAttr, UuidValue};

/// 一个 FsSet 步骤：字段与 [`crate::planner::PlanStep::FsSet`] 一一对应。
pub(crate) struct FsSet {
    pub part: u32,
    pub attr: FsSetAttr,
    pub fstype: Option<ContentKind>,
}

impl FsSet {
    /// 属性名，只用于失败文案
    fn name(&self) -> &'static str {
        match &self.attr {
            FsSetAttr::Label(_) => "label",
            FsSetAttr::Uuid(_) => "uuid",
        }
    }

    /// 载荷自洽：分区号为 0 是计划自身不合法 → 1；卷标超容量属
    /// 准入期拒绝 → 3；UUID 文本不能解析为 UUID → 2
    fn check(&self) -> Result<(), RunOutcome> {
        if self.part == 0 {
            return Err(RunOutcome::internal("the step targets partition 0"));
        }
        match &self.attr {
            FsSetAttr::Label(label) => {
                fsops::check_label_limits(self.fstype, label)
                    .map_err(|why| RunOutcome::refused(why.to_string()))
            }
            FsSetAttr::Uuid(UuidValue::Explicit(text)) => {
                if crate::support::parse_guid(text).is_some() {
                    Ok(())
                } else {
                    Err(RunOutcome::usage(format!("{text:?} is not a UUID")))
                }
            }
            FsSetAttr::Uuid(UuidValue::Generate) => Ok(()),
        }
    }
}

/// 首次写入前的预扫描：载荷自洽，任何失败都发生在写目标字节之前。
pub(crate) fn preflight(step: &FsSet) -> Result<(), RunOutcome> {
    step.check()
}

/// 屏障落在调用外部工具的同一函数里、紧贴首次写盘之前：记录随屏障落盘，之后工具无论
/// 成败，目标都处于不可回滚状态。本步之前的失败按各自的原变体返回；越过首写之后
/// 的失败由退出点的归责统一改判，调用点不自行降级
pub(crate) fn execute(src: &mut WriteSource, step: &FsSet) -> Result<(), RunOutcome> {
    // 预扫描已经判过同一份载荷，这里的失败只是兜底
    step.check()?;
    if let Err(e) = src.mark_non_reversible() {
        let msg = e.record_failure("the recovery barrier");
        return Err(if e.record_bytes_persisted() {
            RunOutcome::partial(msg)
        } else {
            RunOutcome::no_mutation(msg)
        });
    }
    let result = match &step.attr {
        FsSetAttr::Label(label) => fsops::set_label(src, step.part, step.fstype, label),
        FsSetAttr::Uuid(uuid) => {
            let request = match uuid {
                UuidValue::Explicit(text) => UuidRequest::Explicit(text.clone()),
                UuidValue::Generate => UuidRequest::NewRandom,
            };
            fsops::set_uuid(src, step.part, step.fstype, &request)
        }
    };
    result.map_err(|e| RunOutcome::from(e).context(&format!("set {} failed", step.name())))
}
