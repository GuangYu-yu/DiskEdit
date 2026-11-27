//! 文件系统命令共用的准入与收尾：分区节点、表形态、占用、身份与记录。
//!
//! `mkfs` 与 `resize --fs-only` 只差"选哪种内容能力"和"计划哪一步"，其余前置完
//! 全一致；这里只保留那部分一致的事实判断，不产生任何写。

use diskedit_core::fingerprint::Fingerprint;
use diskedit_core::region::Region;

use crate::app::observe::table::TableView;
use crate::app::observe::{FilesystemSnapshot, PartitionFacts, PartitionSelector, PriorState};
use crate::platform::Occupancy;

use super::{Decision, Plan, Refusal};

/// 写命令的公共前置：分区节点、表形态、占用。
pub fn admission(snapshot: &FilesystemSnapshot, command: &str) -> Option<Refusal> {
    if snapshot.is_partition_node {
        return Some(Refusal::PartitionNodeTarget {
            command: command.to_owned(),
            target: snapshot.target.display.clone(),
        });
    }
    match &snapshot.view {
        TableView::None => return Some(Refusal::TableMissing),
        TableView::Damaged { why, .. } => {
            return Some(Refusal::TableDamaged { why: why.clone() });
        }
        TableView::Gpt(_) | TableView::Mbr(_) => {}
    }
    match &snapshot.occupancy {
        Occupancy::Idle => None,
        Occupancy::Used { evidence } => Some(Refusal::Occupied {
            evidence: evidence.clone(),
        }),
        Occupancy::Unknown { why } => Some(Refusal::OccupancyUnknown { why: why.clone() }),
    }
}

/// 选择器在表里的落点。
pub fn selected(snapshot: &FilesystemSnapshot) -> Result<&PartitionFacts, Refusal> {
    snapshot
        .selected
        .as_ref()
        .ok_or_else(|| match snapshot.selector {
            Some(PartitionSelector::Number(number)) => Refusal::PartitionMissing { number },
            Some(PartitionSelector::Last) => Refusal::TableEdit {
                why: "there is no partition to point the selector at".to_owned(),
            },
            None => Refusal::TableEdit {
                why: "`mkfs` needs a partition: write <TARGET>:N".to_owned(),
            },
        })
}

/// 目标自身证明的稳定身份。
pub fn identity(snapshot: &FilesystemSnapshot) -> Result<Fingerprint, Refusal> {
    snapshot
        .target
        .identity
        .ok_or_else(|| Refusal::IdentityUnprovable {
            why: "the target gives no comparable identity".to_owned(),
        })
}

/// 分区覆盖的字节区间。
pub fn region(partition: &PartitionFacts) -> Region {
    Region::sized(partition.base_bytes, partition.bytes).expect("由当前盘面导出的区间自洽")
}

/// 处理未收尾记录：同计划续跑放行，异计划或不可读拒绝。
pub fn finish(
    snapshot: &FilesystemSnapshot,
    plan: Result<Plan, Refusal>,
    note: String,
) -> Decision {
    let plan = match plan {
        Ok(plan) => plan,
        Err(refusal) => return Decision::Refuse(refusal),
    };
    match &snapshot.prior {
        PriorState::Absent => {}
        PriorState::Unreadable => {
            return Decision::Refuse(Refusal::RecordUnreadable {
                path: record_path(snapshot),
            });
        }
        PriorState::Present(prior) if prior.pending && prior.plan_digest != plan.digest() => {
            return Decision::Refuse(Refusal::RecordConflict {
                path: record_path(snapshot),
            });
        }
        PriorState::Present(_) => {}
    }
    Decision::Execute { plan, note }
}

fn record_path(snapshot: &FilesystemSnapshot) -> String {
    snapshot
        .record
        .file
        .as_ref()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| "(record path unresolved)".to_owned())
}
