//! `check` 的判定：选择检查程序与目标形态。
//!
//! 判定只从档案表取"该内容类别用什么程序"；目标如何变成可检查的设备节点由执行层
//! 与宿主端口决定。

use diskedit_core::fs::{Support, Verb, profile};

use crate::app::observe::PartitionSelector;
use crate::app::observe::snapshot::Snapshot;

use super::report::{self, CheckQuery, CheckTarget, Report};
use super::{Decision, Refusal, Step, StepKind};

/// 判定 `check`；`selector` 由解析层从命令形态带下来。
pub fn decide(snapshot: &Snapshot, selector: PartitionSelector) -> Decision {
    let number = match selector {
        PartitionSelector::Number(number) => number,
        PartitionSelector::Last => match snapshot.table.last() {
            Some(partition) => partition.number,
            None => return Decision::Refuse(Refusal::TableMissing),
        },
    };
    let Some(partition) = snapshot.table.find(number) else {
        return Decision::Refuse(Refusal::PartitionMissing { number });
    };
    let support = profile(partition.content).support(Verb::Check);
    let program = match support {
        Support::Tools(tools) => match tools.first() {
            Some(program) => *program,
            None => {
                return Decision::Refuse(Refusal::ContentUnsupported {
                    what: format!("check on {}", partition.content.as_str()),
                    why: "the check profile lists no program".to_owned(),
                });
            }
        },
        Support::NotApplicable => {
            return Decision::Refuse(Refusal::ContentUnsupported {
                what: format!("check on {}", partition.content.as_str()),
                why: "this content kind has nothing to check".to_owned(),
            });
        }
        Support::Unsupported(why) => {
            return Decision::Refuse(Refusal::ContentUnsupported {
                what: format!("check on {}", partition.content.as_str()),
                why: why.to_owned(),
            });
        }
    };
    let target = if snapshot.target.kind.is_block_device() {
        CheckTarget::PartitionDevice {
            disk: snapshot.target.path.clone(),
            number,
        }
    } else {
        CheckTarget::ImageRegion {
            path: snapshot.target.path.clone(),
            offset: partition.base_bytes,
            len: partition.bytes,
        }
    };
    let query = CheckQuery {
        program,
        partition: number,
        content: partition.content,
        target,
    };
    report::decision(
        Report::Check(query),
        vec![Step::new(StepKind::Check { partition: number }, Vec::new())],
    )
}
