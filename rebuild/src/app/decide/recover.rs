//! `recover` 的判定：把纯恢复器的结论落到一条命令决策上。
//!
//! 判定只读快照，不碰目标字节，也不取锁。它负责三件事：按 B22 的占用闸与
//! 身份等级拒绝不合格的目标；按 B9 的次序产出一条外部状态或写目标计划；
//! 按 B7 / B8 / B23 的口径把记录分成"可回滚""只能回放表区""只能放弃"
//! 三类，其余一律拒绝并保留原记录。真正的锁内复核与写入由执行层完成。

use diskedit_core::fingerprint::Strength;
use diskedit_core::record::{Frame, RecoveryPlan, recover};
use diskedit_core::region::Region;

use crate::app::observe::{Command, RecoverMode, RecoverSnapshot, Request};
use crate::platform::Occupancy;

use super::step::{Step, StepKind};
use super::{Decision, Plan, Refusal};

/// 判定 `recover` 的一次调用。
pub fn decide(snapshot: &RecoverSnapshot, request: &Request) -> Decision {
    let Command::Recover(mode) = request.command() else {
        return Decision::Refuse(Refusal::RecoveryRefused {
            why: "recover reached with a different command".to_owned(),
        });
    };
    let mode = *mode;
    if snapshot.is_partition_node {
        return Decision::Refuse(Refusal::PartitionNodeTarget {
            command: "recover".to_owned(),
            target: snapshot.target.display.clone(),
        });
    }
    let identity = match snapshot.target.identity {
        Some(identity) if identity.strength() == Strength::Strong => identity,
        _ => {
            return Decision::Refuse(Refusal::IdentityUnprovable {
                why: "the target gives no comparable identity".to_owned(),
            });
        }
    };
    // B22：回滚、表区回放与放弃都不豁免占用闸。
    match &snapshot.occupancy {
        Occupancy::Idle => {}
        Occupancy::Used { evidence } => {
            return Decision::Refuse(Refusal::Occupied {
                evidence: evidence.clone(),
            });
        }
        Occupancy::Unknown { why } => {
            return Decision::Refuse(Refusal::OccupancyUnknown { why: why.clone() });
        }
    }
    let path = snapshot
        .record
        .file
        .as_ref()
        .map(|file| file.display().to_string())
        .unwrap_or_else(|| snapshot.target.display.clone());
    let Some(bytes) = snapshot.record.bytes.as_deref() else {
        return match mode {
            // 放弃对没有记录的目标是成功空操作。
            RecoverMode::Release { .. } => Decision::Unchanged {
                note: format!(
                    "no recovery record on {} — nothing to release",
                    snapshot.target.display
                ),
            },
            // 与基线一致：默认与表区回放缺记录时按拒绝报告。
            RecoverMode::Default | RecoverMode::ReplayTableOnly => {
                Decision::Refuse(Refusal::RecoveryRefused {
                    why: format!(
                        "no recovery record on {} — nothing to recover",
                        snapshot.target.display
                    ),
                })
            }
        };
    };
    let plan = recover(bytes, identity);
    match mode {
        RecoverMode::Release { discard_replay } => release(plan, discard_replay, identity, &path),
        mode @ (RecoverMode::Default | RecoverMode::ReplayTableOnly) => {
            rollback(plan, mode, identity, &path, snapshot.target.bytes)
        }
    }
}

/// 已经提交或没有可回写区间的记录只清理；有原像的按记录回写。
fn rollback(
    plan: RecoveryPlan,
    mode: RecoverMode,
    identity: diskedit_core::fingerprint::Fingerprint,
    path: &str,
    target_bytes: u64,
) -> Decision {
    match plan {
        RecoveryPlan::Absent => Decision::Refuse(Refusal::RecoveryRefused {
            why: format!("no recovery record at {path} — nothing to recover"),
        }),
        RecoveryPlan::Committed => external_cleanup(
            mode,
            identity,
            "the recorded operation had already committed — the recovery record was removed",
        ),
        RecoveryPlan::Foreign { magic } => Decision::Refuse(Refusal::RecoveryRefused {
            why: format!(
                "the record at {path} was not written by this implementation (magic {:?}); only `recover --release` may abandon it",
                String::from_utf8_lossy(&magic)
            ),
        }),
        RecoveryPlan::NeedsManual { why } => Decision::Refuse(Refusal::RecoveryRefused {
            why: format!("the record at {path} is not replayable: {why}"),
        }),
        RecoveryPlan::RollBack { frames } | RecoveryPlan::Replay { frames } => {
            if mode == RecoverMode::Default && data_may_have_moved(&frames) {
                return Decision::Refuse(Refusal::RecoveryRefused {
                    why: "the record holds data-region steps that may already have moved; \
                          use `recover --replay-table-only` to restore only the partition table"
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
            // B24 / A1-17：记录里登记的区间必须仍落在当前目标长度内；越界是几何
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

/// 放弃回滚：只建外部状态计划，目标字节不变。
fn release(
    plan: RecoveryPlan,
    discard_replay: bool,
    identity: diskedit_core::fingerprint::Fingerprint,
    path: &str,
) -> Decision {
    match &plan {
        RecoveryPlan::Absent => Decision::Unchanged {
            note: "no recovery record on the target — nothing to release".to_owned(),
        },
        RecoveryPlan::NeedsManual { why } if why.contains("different device") => {
            Decision::Refuse(Refusal::RecoveryRefused {
                why: format!("the record at {path} belongs to a different target: {why}"),
            })
        }
        // 外来格式或损坏载荷仍可放弃：B8 只看对象与归属，不看载荷完好；
        // D2 把旧版在途记录的处理权交给人的显式放弃。
        RecoveryPlan::Foreign { .. } | RecoveryPlan::NeedsManual { .. } => {
            external_release(identity, RecoverMode::Release { discard_replay })
        }
        RecoveryPlan::RollBack { frames } | RecoveryPlan::Replay { frames } => {
            let replayable = replayable_ranges(frames);
            if replayable > 0 && !discard_replay {
                return Decision::Refuse(Refusal::RecoveryRefused {
                    why: format!(
                        "the record still holds {replayable} replayable range(s); pass \
                         --discard-replay to give them up"
                    ),
                });
            }
            external_release(identity, RecoverMode::Release { discard_replay })
        }
        RecoveryPlan::Committed => {
            external_release(identity, RecoverMode::Release { discard_replay })
        }
    }
}

fn external_cleanup(
    mode: RecoverMode,
    identity: diskedit_core::fingerprint::Fingerprint,
    note: &str,
) -> Decision {
    let step = Step::new(StepKind::RecoverRecords { mode }, Vec::new());
    match Plan::external(vec![step], identity) {
        Ok(plan) => Decision::Execute {
            plan,
            note: note.to_owned(),
        },
        Err(refusal) => Decision::Refuse(refusal),
    }
}

fn external_release(
    identity: diskedit_core::fingerprint::Fingerprint,
    mode: RecoverMode,
) -> Decision {
    let step = Step::new(StepKind::RecoverRecords { mode }, Vec::new());
    match Plan::external(vec![step], identity) {
        Ok(plan) => Decision::Execute {
            plan,
            note: "released the recovery record without touching the target".to_owned(),
        },
        Err(refusal) => Decision::Refuse(refusal),
    }
}

/// 记录里是否已有完成落盘、不可回滚的数据区步骤。
fn data_may_have_moved(frames: &[Frame]) -> bool {
    frames
        .iter()
        .any(|frame| frame.completed && frame.irreversible)
}

/// 记录里携带原像的区间数；只统计放弃会真丢掉的可回放区间。
fn replayable_ranges(frames: &[Frame]) -> usize {
    frames
        .iter()
        .flat_map(|frame| &frame.regions)
        .filter(|preimage| !preimage.data.is_empty())
        .count()
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
    use crate::app::observe::TargetFacts;
    use crate::app::observe::records::RecordState;
    use crate::platform::{Occupancy, TargetKind};

    const TARGET: Fingerprint = Fingerprint::strong([0x42; 32]);

    fn snapshot(bytes: Option<Vec<u8>>, occupancy: Occupancy) -> RecoverSnapshot {
        RecoverSnapshot {
            target: TargetFacts {
                display: "disk.img".to_owned(),
                path: "disk.img".into(),
                bytes: 1 << 20,
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

    fn request(mode: RecoverMode) -> Request {
        Request::new(
            crate::app::observe::TargetSpec::new("disk.img", None),
            Command::Recover(mode),
        )
        .with_confirmation(true)
    }

    fn body() -> RecordBody {
        RecordBody {
            target: TARGET,
            geometry_generation: 1,
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

    // covers: B22
    #[test]
    fn a_used_target_is_refused_before_any_recovery() {
        let plan = decide(
            &snapshot(
                Some(vec![1]),
                Occupancy::Used {
                    evidence: "mounted at /mnt".to_owned(),
                },
            ),
            &request(RecoverMode::Default),
        );
        assert!(matches!(plan, Decision::Refuse(Refusal::Occupied { .. })));
    }

    // covers: D6
    #[test]
    fn an_undecidable_occupancy_is_not_treated_as_idle() {
        let plan = decide(
            &snapshot(
                Some(Vec::new()),
                Occupancy::Unknown {
                    why: "holders unreadable".to_owned(),
                },
            ),
            &request(RecoverMode::Default),
        );
        assert!(matches!(
            plan,
            Decision::Refuse(Refusal::OccupancyUnknown { .. })
        ));
    }

    // covers: D2
    #[test]
    fn a_foreign_record_is_refused_and_never_rolled_back() {
        let mut bytes = encode(&body(), &[frame(0, FrameKind::Intent, false, false, &[])]).unwrap();
        bytes[0] = b'Z';
        let plan = decide(
            &snapshot(Some(bytes), Occupancy::Idle),
            &request(RecoverMode::Default),
        );
        assert!(matches!(
            plan,
            Decision::Refuse(Refusal::RecoveryRefused { .. })
        ));
    }

    // covers: B7, B9
    #[test]
    fn data_region_steps_block_the_default_and_allow_table_only() {
        let frames = [
            frame(0, FrameKind::Intent, false, false, &[]),
            frame(1, FrameKind::Preimage, true, false, &[0xAA; 4]),
            frame(2, FrameKind::Progress, true, true, &[]),
        ];
        let bytes = encode(&body(), &frames).unwrap();
        let blocked = decide(
            &snapshot(Some(bytes.clone()), Occupancy::Idle),
            &request(RecoverMode::Default),
        );
        assert!(matches!(
            blocked,
            Decision::Refuse(Refusal::RecoveryRefused { .. })
        ));

        let table_only = decide(
            &snapshot(Some(bytes), Occupancy::Idle),
            &request(RecoverMode::ReplayTableOnly),
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

    // covers: B8
    #[test]
    fn releasing_a_replayable_record_needs_discard() {
        let frames = [
            frame(0, FrameKind::Intent, false, false, &[]),
            frame(1, FrameKind::Preimage, true, false, &[0xAA; 4]),
        ];
        let bytes = encode(&body(), &frames).unwrap();
        let refused = decide(
            &snapshot(Some(bytes.clone()), Occupancy::Idle),
            &request(RecoverMode::Release {
                discard_replay: false,
            }),
        );
        assert!(matches!(
            refused,
            Decision::Refuse(Refusal::RecoveryRefused { .. })
        ));

        let allowed = decide(
            &snapshot(Some(bytes), Occupancy::Idle),
            &request(RecoverMode::Release {
                discard_replay: true,
            }),
        );
        match allowed {
            Decision::Execute { .. } => {}
            other
            @ (Decision::Report { .. } | Decision::Unchanged { .. } | Decision::Refuse(_)) => {
                panic!("expected a release plan, got {other:?}")
            }
        }
    }

    // covers: B24
    #[test]
    fn a_recorded_range_past_the_target_is_refused() {
        let frames = [
            frame(0, FrameKind::Intent, false, false, &[]),
            frame(1, FrameKind::Preimage, true, false, &[0xAA; 4]),
        ];
        let bytes = encode(&body(), &frames).unwrap();
        let mut snapshot = snapshot(Some(bytes), Occupancy::Idle);
        snapshot.target.bytes = 512; // the recorded range [512, 516) is now past the end
        let decision = decide(&snapshot, &request(RecoverMode::Default));
        assert!(matches!(
            decision,
            Decision::Refuse(Refusal::RecoveryRefused { .. })
        ));
    }
}
