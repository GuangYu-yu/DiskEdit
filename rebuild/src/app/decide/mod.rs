//! 判定：把请求与快照变成拒绝原因或有序列步骤的计划。
//!
//! 这里只读快照，不写目标、不启动进程、不认识具体工具；任何未知事实都按拒绝
//! 处理，不折叠成空集合或默认值。

pub mod add;
pub mod check;
pub mod common;
pub mod confirmed;
pub mod copy;
pub mod delete;
pub mod digest;
pub mod info;
pub mod inspect;
pub mod mkfs;
#[path = "move.rs"]
pub mod move_partition;
pub mod new;
pub mod plan;
pub mod records;
pub mod recover;
pub mod refusal;
pub mod resize;
pub mod resizefs;
pub mod set;
pub mod step;

pub use confirmed::{ConfirmedPlan, confirm};
pub use plan::Plan;
pub use refusal::Refusal;

use crate::app::observe::{Command, Request, Snapshot};

/// 一次判定的结论。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Decision {
    Refuse { refusal: Refusal },
    Plan { plan: Plan },
}

/// 判定入口。
pub fn decide(request: &Request, snapshot: &Snapshot) -> Decision {
    let outcome = match request.command() {
        Command::Info => info::plan(),
        Command::Inspect => inspect::plan(),
        Command::Check { partition } => check::plan(snapshot, *partition),
        Command::RecordsList => records::list(),
        Command::RecordsVerify { token } => records::verify(snapshot, token.clone()),
        Command::RecordsScan => records::scan(),
        Command::New { table, overwrite } => new::plan(snapshot, *table, *overwrite),
        Command::Add(request) => add::plan(snapshot, request),
        Command::Delete { partition } => delete::plan(snapshot, *partition),
        Command::Copy(request) => copy::plan(snapshot, request),
        Command::Set { partition, action } => set::plan(snapshot, *partition, action),
        Command::Mkfs {
            partition,
            filesystem,
            label,
        } => mkfs::plan(snapshot, *partition, *filesystem, label.clone()),
        Command::Resize(request) => resize::plan(snapshot, request),
        Command::Resizefs { partition, bytes } => {
            let mount_point = request.target().unwrap_or_else(|| std::path::Path::new(""));
            resizefs::plan(snapshot, *partition, *bytes, mount_point)
        }
        Command::Move(request) => move_partition::plan(snapshot, request),
        Command::Recover(mode) => recover::plan(snapshot, *mode),
    };
    match outcome {
        Ok(plan) => Decision::Plan { plan },
        Err(refusal) => Decision::Refuse { refusal },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::observe::disk::DiskFacts;
    use crate::app::observe::snapshot::Topology;
    use crate::app::observe::{Command, TableFormat, TargetSpec};
    use crate::platform::{Occupancy, TargetKind};
    use diskedit_core::evidence::{Evidence, Generation, Observation, Source};
    use diskedit_core::fingerprint::Fingerprint;
    use diskedit_core::table::GptEvidence;

    fn known<T>(value: T) -> Observation<T> {
        Observation::known(value, Source::new("test"), Generation::FIRST)
    }

    fn snapshot() -> Snapshot {
        Snapshot {
            generation: Generation::FIRST,
            identity: known(Fingerprint::strong([7u8; 32])),
            disk: DiskFacts {
                kind: known(TargetKind::File),
                byte_len: known(1 << 30),
                sector_size: known(512),
            },
            topology: known(Topology {
                partitions: Vec::new(),
            }),
            occupancy: known(Occupancy::default()),
            mount: Observation::not_applicable("test snapshot"),
            table: crate::app::observe::table::TableFacts {
                gpt: known(GptEvidence::Absent),
                mbr: known(Evidence::Absent),
            },
            tools: Vec::new(),
        }
    }

    fn request(command: Command) -> Request {
        Request::new(TargetSpec::new("/tmp/image", None), command)
    }

    #[test]
    fn unknown_occupancy_refuses_instead_of_defaulting_to_free() {
        let mut snapshot = snapshot();
        snapshot.occupancy = Observation::unknown("占用探测失败");
        let decision = decide(
            &request(Command::New {
                table: TableFormat::Gpt,
                overwrite: true,
            }),
            &snapshot,
        );
        assert!(matches!(
            decision,
            Decision::Refuse {
                refusal: Refusal::OccupancyUnknown { .. }
            }
        ));
    }

    #[test]
    fn unknown_topology_does_not_become_empty_partition_list() {
        let mut snapshot = snapshot();
        snapshot.topology = Observation::unknown("拓扑读取失败");
        let decision = decide(
            &request(Command::Delete {
                partition: crate::app::observe::PartitionSelector::Number(1),
            }),
            &snapshot,
        );
        assert!(matches!(
            decision,
            Decision::Refuse {
                refusal: Refusal::FactUnknown { .. }
            }
        ));
    }

    #[test]
    fn unusable_identity_cannot_mint_a_write_plan() {
        let mut snapshot = snapshot();
        snapshot.identity = known(Fingerprint::unusable([7u8; 32]));
        let decision = decide(
            &request(Command::New {
                table: TableFormat::Gpt,
                overwrite: true,
            }),
            &snapshot,
        );
        assert!(matches!(
            decision,
            Decision::Refuse {
                refusal: Refusal::IdentityUnprovable { .. }
            }
        ));
    }

    #[test]
    fn known_blank_target_plans_new_gpt_with_identity() {
        let decision = decide(
            &request(Command::New {
                table: TableFormat::Gpt,
                overwrite: true,
            }),
            &snapshot(),
        );
        let Decision::Plan { plan } = decision else {
            panic!("空盘应能建表");
        };
        assert!(plan.writes_target());
        assert!(plan.identity().is_some());
    }

    #[test]
    fn read_only_plan_has_no_target_identity() {
        let Decision::Plan { plan } = decide(&request(Command::Info), &snapshot()) else {
            panic!("只读命令应产出计划");
        };
        assert!(!plan.writes_target());
        assert!(plan.identity().is_none());
    }

    #[test]
    fn confirm_requires_flag_and_matching_identity() {
        let Decision::Plan { plan } = decide(
            &request(Command::New {
                table: TableFormat::Gpt,
                overwrite: true,
            }),
            &snapshot(),
        ) else {
            panic!("空盘应能建表");
        };
        let current = known(Fingerprint::strong([7u8; 32]));
        assert_eq!(
            confirmed::confirm(plan.clone(), false, &current),
            Err(Refusal::ConfirmationRequired)
        );
        assert!(confirmed::confirm(plan.clone(), true, &current).is_ok());
        let changed = known(Fingerprint::strong([8u8; 32]));
        assert_eq!(
            confirmed::confirm(plan, true, &changed),
            Err(Refusal::IdentityChanged)
        );
    }

    #[test]
    fn every_refusal_has_fixed_next_step_wording() {
        let refusals = [
            Refusal::ConfirmationRequired,
            Refusal::OverwriteNotConfirmed,
            Refusal::IdentityUnprovable {
                why: "x".to_owned(),
            },
            Refusal::IdentityChanged,
            Refusal::PlanDigestMismatch,
            Refusal::OccupancyUnknown {
                why: "x".to_owned(),
            },
            Refusal::Occupied {
                evidence: "x".to_owned(),
            },
            Refusal::FactUnknown {
                what: "x".to_owned(),
            },
            Refusal::GeometryUnknown {
                why: "x".to_owned(),
            },
            Refusal::TableMissing,
            Refusal::MountNotMounted {
                path: "x".to_owned(),
            },
            Refusal::MountAmbiguous {
                path: "x".to_owned(),
            },
            Refusal::MountNotPartition {
                device: "x".to_owned(),
            },
            Refusal::MountNotBlockDevice {
                source: "x".to_owned(),
            },
            Refusal::MountUnmapped {
                reason: "x".to_owned(),
            },
            Refusal::OnlineShrink {
                why: "x".to_owned(),
            },
            Refusal::TableDamaged {
                why: "x".to_owned(),
            },
            Refusal::PartitionMissing { number: 1 },
            Refusal::PartitionOverlap { number: 1 },
            Refusal::CapacityInsufficient {
                needed: 1,
                available: 0,
            },
            Refusal::FilesystemUnknown { number: 1 },
            Refusal::ContentUnsupported {
                what: "x".to_owned(),
                why: "y".to_owned(),
            },
            Refusal::MinimumSizeUnknown {
                number: 1,
                why: "x".to_owned(),
            },
            Refusal::ToolMissing {
                program: diskedit_core::fs::Program::E2fsck,
            },
            Refusal::PlatformCannotRunTools {
                why: "x".to_owned(),
            },
            Refusal::ArgumentOutOfRange {
                what: "x".to_owned(),
            },
        ];
        for refusal in refusals {
            assert!(!refusal.next_step().is_empty(), "{refusal:?}");
        }
    }
}
