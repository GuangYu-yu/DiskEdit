//! `undo` 的判定：把纯恢复器的结论落到一条回滚命令决策上。
//!
//! 判定只读快照，不碰目标字节，也不取锁。它负责三件事：按占用闸与
//! 身份等级拒绝不合格的目标；产出一条外部状态或写目标计划；
//! 把记录分成"可回滚""只能回放表区""只能放弃"
//! 三类，其余一律拒绝并保留原记录。真正的锁内复核与写入由执行层完成。
//! `replay` 复用这里的回滚通路（形态只放宽数据区步骤的拒绝），`release`
//! 复用这里的闸门。

use diskedit_core::fingerprint::{Fingerprint, Strength};
use diskedit_core::record::{Frame, RecoveryExpectation, RecoveryPlan, recover};
use diskedit_core::region::Region;

use crate::app::observe::{Command, RecoverMode, RecoverSnapshot, Request};
use crate::platform::Occupancy;

use super::step::{Step, StepKind};
use super::{Decision, Plan, Refusal};

/// 判定 `undo` 的一次调用。
pub fn decide(snapshot: &RecoverSnapshot, request: &Request) -> Decision {
    let Command::Undo = request.command() else {
        return Decision::Refuse(Refusal::RecoveryRefused {
            why: "undo reached with a different command".to_owned(),
        });
    };
    rollback_entry(snapshot, RecoverMode::Undo, "undo")
}

/// 三个恢复入口共用的闸门：分区节点、身份等级与占用。
/// 成功时返回可用于计划绑定的 Strong 身份。
pub(crate) fn gates(snapshot: &RecoverSnapshot, command: &str) -> Result<Fingerprint, Refusal> {
    if snapshot.is_partition_node {
        return Err(Refusal::PartitionNodeTarget {
            command: command.to_owned(),
            target: snapshot.target.display.clone(),
        });
    }
    let identity = match snapshot.target.identity {
        Some(identity) if identity.strength() == Strength::Strong => identity,
        _ => {
            return Err(Refusal::IdentityUnprovable {
                why: "the target gives no comparable identity".to_owned(),
            });
        }
    };
    // 回滚、表区回放与放弃都不豁免占用闸。
    match &snapshot.occupancy {
        Occupancy::Idle => {}
        Occupancy::Used { evidence } => {
            return Err(Refusal::Occupied {
                evidence: evidence.clone(),
            });
        }
        Occupancy::Unknown { why } => {
            return Err(Refusal::OccupancyUnknown { why: why.clone() });
        }
    }
    Ok(identity)
}

/// 读记录字节并走回滚分类；缺记录按拒绝报告。
/// `replay` 与 `undo` 共用这条通路：形态只放宽数据区步骤的拒绝、并改变缺记录文案。
pub(crate) fn rollback_entry(
    snapshot: &RecoverSnapshot,
    mode: RecoverMode,
    command: &'static str,
) -> Decision {
    let identity = match gates(snapshot, command) {
        Ok(identity) => identity,
        Err(refusal) => return Decision::Refuse(refusal),
    };
    let path = record_path(snapshot);
    let Some(bytes) = snapshot.record.bytes.as_deref() else {
        // 记录位置已知而读取失败时不能当成"没有记录"：只有确认不存在才继续。
        return match snapshot.record.unavailable.as_deref() {
            Some(_) => Decision::Refuse(Refusal::RecordUnreadable { path }),
            None => Decision::Refuse(Refusal::RecoveryRefused {
                why: format!(
                    "no recovery record on {} — nothing to {command}",
                    snapshot.target.display
                ),
            }),
        };
    };
    let plan = recover(
        bytes,
        RecoveryExpectation::new(identity, snapshot.target.bytes),
    );
    rollback(plan, mode, identity, &path, snapshot.target.bytes, command)
}

/// 记录文件路径的展示值；缺文件时退回目标名。
pub(crate) fn record_path(snapshot: &RecoverSnapshot) -> String {
    snapshot
        .record
        .file
        .as_ref()
        .map(|file| file.display().to_string())
        .unwrap_or_else(|| snapshot.target.display.clone())
}

/// 已经提交或没有可回写区间的记录只清理；有原像的按记录回写。
fn rollback(
    plan: RecoveryPlan,
    mode: RecoverMode,
    identity: Fingerprint,
    path: &str,
    target_bytes: u64,
    command: &str,
) -> Decision {
    match plan {
        RecoveryPlan::Absent => Decision::Refuse(Refusal::RecoveryRefused {
            why: format!("no recovery record at {path} — nothing to {command}"),
        }),
        RecoveryPlan::Committed => external_cleanup(
            mode,
            identity,
            "the recorded operation had already committed — the recovery record was removed",
        ),
        RecoveryPlan::Foreign { magic } => Decision::Refuse(Refusal::RecoveryRefused {
            why: format!(
                "the record at {path} was not written by this implementation (magic {:?}); only `diskedit release` may abandon it",
                String::from_utf8_lossy(&magic)
            ),
        }),
        RecoveryPlan::NeedsManual { why } => Decision::Refuse(Refusal::RecoveryRefused {
            why: format!("the record at {path} is not replayable: {why}"),
        }),
        RecoveryPlan::RollBack { frames } | RecoveryPlan::Replay { frames } => {
            if mode == RecoverMode::Undo && data_may_have_moved(&frames) {
                return Decision::Refuse(Refusal::RecoveryRefused {
                    why: "the record holds data-region steps that may already have moved; \
                          use `diskedit replay` to restore only the partition table"
                        .to_owned(),
                });
            }
            let regions = match collect_regions(&frames) {
                Ok(regions) => regions,
                Err(why) => return Decision::Refuse(why),
            };
            if regions.is_empty() {
                return external_cleanup(
                    mode,
                    identity,
                    "the record held no written ranges — the recovery record was removed",
                );
            }
            // 记录里登记的区间必须仍落在当前目标长度内；越界是几何
            // 漂移，按拒绝处理，不拿旧偏移去写。
            for region in &regions {
                if region.end() > target_bytes {
                    return Decision::Refuse(Refusal::RecoveryRefused {
                        why: format!(
                            "recorded preimage range [{}, {}) lies past the target's current length {} — \
                             the geometry changed after the record was written",
                            region.start(),
                            region.end(),
                            target_bytes
                        ),
                    });
                }
            }
            let step = Step::new(StepKind::RecoverRecords { mode }, regions);
            match Plan::write(vec![step], identity) {
                Ok(plan) => Decision::Execute {
                    plan,
                    note: "rolled back the recorded write and removed the recovery record"
                        .to_owned(),
                },
                Err(refusal) => Decision::Refuse(refusal),
            }
        }
    }
}

fn external_cleanup(mode: RecoverMode, identity: Fingerprint, note: &str) -> Decision {
    let step = Step::new(StepKind::RecoverRecords { mode }, Vec::new());
    match Plan::external(vec![step], identity) {
        Ok(plan) => Decision::Execute {
            plan,
            note: note.to_owned(),
        },
        Err(refusal) => Decision::Refuse(refusal),
    }
}

/// 记录里是否已有不可回滚的步骤：只要出现不可逆帧（无论是否已完成），
/// `undo` 都不能把目标当成可完整撤销。
fn data_may_have_moved(frames: &[Frame]) -> bool {
    frames.iter().any(|frame| frame.irreversible)
}

/// 从记录帧里取出可回写的表区区间。
fn collect_regions(frames: &[Frame]) -> Result<Vec<Region>, Refusal> {
    let mut regions = Vec::new();
    for frame in frames {
        for preimage in &frame.regions {
            if preimage.data.is_empty() {
                continue;
            }
            if preimage.data.len() as u64 != preimage.len {
                return Err(Refusal::RecoveryRefused {
                    why: "the recorded preimage length and region length disagree".to_owned(),
                });
            }
            let region = Region::sized(preimage.offset, preimage.len).map_err(|_| {
                Refusal::RecoveryRefused {
                    why: "the recorded preimage region is inverted or out of range".to_owned(),
                }
            })?;
            regions.push(region);
        }
    }
    Ok(regions)
}

#[cfg(test)]
mod tests {
    use diskedit_core::fingerprint::Fingerprint;
    use diskedit_core::record::{FrameKind, Preimage, RecordBody, encode};

    use super::*;
    use crate::app::decide::MIB;
    use crate::app::observe::TargetFacts;
    use crate::app::observe::records::RecordState;
    use crate::platform::{Occupancy, TargetKind};

    const TARGET: Fingerprint = Fingerprint::strong([0x42; 32]);

    fn snapshot(bytes: Option<Vec<u8>>, occupancy: Occupancy) -> RecoverSnapshot {
        RecoverSnapshot {
            target: TargetFacts {
                display: "disk.img".to_owned(),
                path: "disk.img".into(),
                bytes: MIB,
                kind: TargetKind::File,
                sector_size: 512,
                identity: Some(TARGET),
            },
            record: RecordState {
                directory: Some("state/images/x".into()),
                directory_exists: true,
                file: Some("state/images/x/record".into()),
                bytes,
                unavailable: None,
            },
            occupancy,
            is_partition_node: false,
        }
    }

    fn request(command: Command) -> Request {
        Request::new(
            crate::app::observe::TargetSpec::new("disk.img", None),
            command,
        )
        .with_confirmation(true)
    }

    fn body(total_bytes: u64) -> RecordBody {
        RecordBody {
            target: TARGET,
            legacy_generation: diskedit_core::record::geometry_generation(total_bytes),
            plan_digest: [0x11; 32],
        }
    }

    fn frame(seq: u32, kind: FrameKind, completed: bool, irreversible: bool, data: &[u8]) -> Frame {
        Frame {
            seq,
            kind,
            completed,
            irreversible,
            intent: Vec::new(),
            regions: if data.is_empty() {
                Vec::new()
            } else {
                vec![Preimage {
                    offset: 512,
                    len: data.len() as u64,
                    digest: [0x33; 16],
                    data: data.to_vec(),
                    after: Vec::new(),
                }]
            },
        }
    }

    #[test]
    fn a_used_target_is_refused_before_any_recovery() {
        let plan = decide(
            &snapshot(
                Some(vec![1]),
                Occupancy::Used {
                    evidence: "mounted at /mnt".to_owned(),
                },
            ),
            &request(Command::Undo),
        );
        assert!(matches!(plan, Decision::Refuse(Refusal::Occupied { .. })));
    }

    #[test]
    fn an_undecidable_occupancy_is_not_treated_as_idle() {
        let plan = decide(
            &snapshot(
                Some(Vec::new()),
                Occupancy::Unknown {
                    why: "holders unreadable".to_owned(),
                },
            ),
            &request(Command::Undo),
        );
        assert!(matches!(
            plan,
            Decision::Refuse(Refusal::OccupancyUnknown { .. })
        ));
    }

    #[test]
    fn a_foreign_record_is_refused_and_never_rolled_back() {
        let mut bytes = encode(
            &body(MIB),
            &[frame(0, FrameKind::Intent, false, false, &[])],
        )
        .unwrap();
        bytes[0] = b'Z';
        let plan = decide(
            &snapshot(Some(bytes), Occupancy::Idle),
            &request(Command::Undo),
        );
        assert!(matches!(
            plan,
            Decision::Refuse(Refusal::RecoveryRefused { .. })
        ));
    }

    #[test]
    fn undo_refuses_an_irreversible_step_that_did_not_complete() {
        let frames = [frame(0, FrameKind::Intent, false, true, &[])];
        let bytes = encode(&body(MIB), &frames).unwrap();
        let decision = decide(
            &snapshot(Some(bytes), Occupancy::Idle),
            &request(Command::Undo),
        );
        assert!(matches!(
            decision,
            Decision::Refuse(Refusal::RecoveryRefused { .. })
        ));
    }

    #[test]
    fn data_region_steps_block_the_default_and_allow_table_only() {
        let frames = [
            frame(0, FrameKind::Intent, false, false, &[]),
            frame(1, FrameKind::Preimage, true, false, &[0xAA; 4]),
            frame(2, FrameKind::Progress, true, true, &[]),
        ];
        let bytes = encode(&body(MIB), &frames).unwrap();
        let blocked = decide(
            &snapshot(Some(bytes.clone()), Occupancy::Idle),
            &request(Command::Undo),
        );
        assert!(matches!(
            blocked,
            Decision::Refuse(Refusal::RecoveryRefused { .. })
        ));

        let table_only = super::super::replay::decide(
            &snapshot(Some(bytes), Occupancy::Idle),
            &request(Command::Replay),
        );
        match table_only {
            Decision::Execute { plan, .. } => {
                assert!(plan.writes_target());
                assert_eq!(
                    plan.steps()[0].regions,
                    vec![Region::sized(512, 4).unwrap()]
                );
            }
            other
            @ (Decision::Report { .. } | Decision::Unchanged { .. } | Decision::Refuse(_)) => {
                panic!("expected a table-only rollback plan, got {other:?}")
            }
        }
    }

    #[test]
    fn a_recorded_range_past_the_target_is_refused() {
        let frames = [
            frame(0, FrameKind::Intent, false, false, &[]),
            frame(1, FrameKind::Preimage, true, false, &[0xAA; 4]),
        ];
        let bytes = encode(&body(512), &frames).unwrap();
        let mut snapshot = snapshot(Some(bytes), Occupancy::Idle);
        snapshot.target.bytes = 512; // the recorded range [512, 516) is now past the end
        let decision = decide(&snapshot, &request(Command::Undo));
        assert!(matches!(
            decision,
            Decision::Refuse(Refusal::RecoveryRefused { .. })
        ));
    }
}
