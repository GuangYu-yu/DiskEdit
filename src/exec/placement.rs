//! 放置决策：本次作业的步骤由宿主执行，还是交给 VM 载体。
//!
//! 判据只有一条：宿主是否具备执行本作业**全部**步骤的能力。能力清单由
//! [`crate::fsops`] 的工具能力矩阵派生，与原生执行器读的是同一份；宿主能力不足时，
//! 可用的载体 bundle 接走整份计划；两者都没有就以 6 退出并给出补救路径。
//!
//! 判定发生在首次写盘之前、一次性完成：步骤之间不换执行器。

use crate::dev::WriteSource;
use crate::fsops::{self, Needs};
use crate::outcome::RunOutcome;
use crate::planner::{FsSetAttr, UuidValue};
use crate::recovery::{Audit, PlacementAudit, ToolVersion};

use super::PreparedStep;
use super::Placement;
use super::qemu::Bundle;

/// 一次作业的放置结论与它的载体材料
pub(crate) struct Decision {
    pub(crate) placement: Placement,
    pub(crate) bundle: Option<Bundle>,
    /// 判定期取到的能力清单；记录尾段的工具版本按它写
    needs: Needs,
}

impl Decision {
    /// 本作业的审计尾段。字段只写出，续跑与回滚判定不读
    pub(crate) fn audit(&self) -> Audit {
        Audit {
            placement: match self.placement {
                Placement::Native => PlacementAudit::Native,
                Placement::Vm => PlacementAudit::Vm,
            },
            worker_digest: self.bundle.as_ref().map(|b| b.worker_digest),
            tool_versions: self
                .needs
                .tools
                .iter()
                .filter_map(|t| {
                    t.version().map(|version| ToolVersion { tool: t.name().to_string(), version })
                })
                .collect(),
        }
    }
}

/// 决定放置。`Err` 只在两种情况下出现：计划本身要求做不到的事（换宿主也做不到），
/// 或宿主能力不足且没有可用的载体 bundle。
///
/// `extra` 是同一条命令里本次计划之后还要执行的需求（搬移之后的扩容收尾就是这种）。
/// 只看当前这份计划，同一条命令会在步骤之间换执行器：搬移腿判成原生，扩容腿才交给载体
pub(crate) fn decide(
    src: &WriteSource,
    steps: &[PreparedStep<'_>],
    sector_size: u64,
    extra: &[Needs],
) -> Result<Decision, RunOutcome> {
    let mut needs = needs_of(src, steps, sector_size)?;
    for other in extra {
        needs.merge(other.clone());
    }
    match needs.require_host("run this job") {
        Ok(()) => Ok(Decision { placement: Placement::Native, bundle: None, needs }),
        Err(gap) => match Bundle::discover()? {
            Some(bundle) => Ok(Decision { placement: Placement::Vm, bundle: Some(bundle), needs }),
            None => Err(RunOutcome::unsupported(recipe(&gap))),
        },
    }
}

/// 宿主能力不足以执行、但确实配了载体目录：交给 [`decide`] 在首次写盘前做完整校验。
/// 预检只看"有没有配置"，bundle 的摘要与架构由启动前的那一次校验回答
fn admit(needs: &Needs, what: &str) -> Result<(), RunOutcome> {
    match needs.require_host(what) {
        Ok(()) => Ok(()),
        Err(_) if super::qemu::carrier_possible() => Ok(()),
        Err(gap) => Err(RunOutcome::unsupported(recipe(&gap))),
    }
}

/// 扩容预检。命令层在首次写盘前确认"宿主能做，或有载体接走"。返回这份需求，供
/// 同一条命令里更早的那份计划做整体放置判定（见 [`decide`] 的 `extra`）
pub(crate) fn require_grow(
    g: &fsops::Growable,
    is_block: bool,
) -> Result<Needs, RunOutcome> {
    let needs = fsops::grow_step_needs(g, is_block).map_err(RunOutcome::from)?;
    admit(&needs, "grow the filesystem")?;
    Ok(needs)
}

/// 已知 FS 类型的扩容预检
pub(crate) fn require_grow_fs(
    fstype: crate::domain::ContentKind,
    needs_loop: bool,
) -> Result<(), RunOutcome> {
    let needs = fsops::grow_needs(fstype, needs_loop).map_err(RunOutcome::from)?;
    admit(&needs, &format!("grow {}", crate::fsid::content_name(fstype)))
}

/// 换区条目落位后按旧头重建 swap 的预检
pub(crate) fn require_swap_rebuild(is_block: bool) -> Result<(), RunOutcome> {
    admit(&fsops::swap_rebuild_needs(is_block), "rebuild swap")
}

/// 建 FS 预检
pub(crate) fn require_mkfs(fstype: fsops::MkfsFs) -> Result<(), RunOutcome> {
    let needs = fsops::mkfs_needs(fstype).map_err(RunOutcome::from)?;
    admit(&needs, "create the filesystem")
}

/// 写卷标预检
pub(crate) fn require_label(fstype: Option<crate::domain::ContentKind>) -> Result<(), RunOutcome> {
    let needs = fsops::label_needs(fstype).map_err(RunOutcome::from)?;
    admit(&needs, "write the filesystem label")
}

/// 写 UUID 预检
pub(crate) fn require_uuid(
    fstype: Option<crate::domain::ContentKind>,
    random: bool,
) -> Result<(), RunOutcome> {
    let needs = fsops::uuid_needs(fstype, random).map_err(RunOutcome::from)?;
    admit(&needs, "write the filesystem UUID")
}

/// 本作业全部步骤的能力并集。只有 FS 类步骤会提出能力要求；表与数据搬移
/// 是纯字节读写，各处都能执行
fn needs_of(
    src: &WriteSource,
    steps: &[PreparedStep<'_>],
    sector_size: u64,
) -> Result<Needs, RunOutcome> {
    let mut needs = Needs::none();
    for step in steps {
        match step {
            // 换区条目的表项落位之后要按旧头重建 swap
            PreparedStep::Transfer { swap: Some(_), .. } => {
                needs.merge(fsops::swap_rebuild_needs(src.is_block));
            }
            PreparedStep::Transfer { swap: None, .. }
            | PreparedStep::Copy { .. }
            | PreparedStep::TableOnly(_) => {}
            PreparedStep::FsResize(step) => {
                let (base, len) = step.byte_range(sector_size).map_err(RunOutcome::internal)?;
                let growable =
                    fsops::grow_target_at(src, step.part, base, len).map_err(RunOutcome::from)?;
                needs.merge(fsops::grow_step_needs(&growable, src.is_block).map_err(RunOutcome::from)?);
            }
            PreparedStep::FsCreate(step) => {
                needs.merge(fsops::mkfs_needs(step.fstype).map_err(RunOutcome::from)?);
            }
            PreparedStep::FsSet(step) => {
                let required = match &step.attr {
                    FsSetAttr::Label(_) => fsops::label_needs(step.fstype),
                    FsSetAttr::Uuid(UuidValue::Generate) => fsops::uuid_needs(step.fstype, true),
                    FsSetAttr::Uuid(UuidValue::Explicit(_)) => fsops::uuid_needs(step.fstype, false),
                };
                needs.merge(required.map_err(RunOutcome::from)?);
            }
        }
    }
    Ok(needs)
}

/// 宿主能力不足且没有 bundle 时的退出 6 文案：先如实说缺口，再给出路。出路随本平台
/// 能不能交棒而变——没有这条路径的平台不该被劝去配一个自己用不了的 bundle
fn recipe(gap: &fsops::FsError) -> String {
    recipe_text(gap, super::qemu::platform_can_carry())
}

fn recipe_text(gap: &fsops::FsError, can_carry: bool) -> String {
    if !can_carry {
        return format!(
            "this host cannot run the filesystem steps of the job: {gap}\n\
             this platform cannot hand a target to a VM carrier — there is no file-descriptor \
             hand-off and no /dev/fdset here. Run the job on a Linux host: as root it stays native, \
             and a non-root Linux host can use a carrier bundle"
        );
    }
    format!(
        "this host cannot run the filesystem steps of the job: {gap}\n\
         install the missing tools and re-run, or point {env} at a VM carrier bundle \
         (manifest.json, kernel and initramfs whose sha256 match the manifest, built for this \
         executable's version and architecture)",
        env = super::qemu::BUNDLE_ENV,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 没有交棒路径的平台，拒绝文案里不能出现 bundle 这个名字：它只会把人引向一件
    /// 在这台机器上永远做不到的事
    #[test]
    fn a_platform_without_hand_off_is_not_pointed_at_a_bundle() {
        let gap = fsops::FsError::platform("no filesystem tool runs here");
        let text = recipe_text(&gap, false);
        assert!(!text.contains(super::super::qemu::BUNDLE_ENV), "{text}");
        assert!(text.contains("cannot hand a target"), "{text}");
        let text = recipe_text(&gap, true);
        assert!(text.contains(super::super::qemu::BUNDLE_ENV), "{text}");
    }
}
