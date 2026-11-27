//! `release` 的判定：放弃回滚，只产出外部状态计划，不写目标字节。
//!
//! 闸门与 `undo` 共用（B22）；记录分类按 B8 / D2 的口径：外来格式或损坏载荷
//! 仍可放弃，但放弃可回放区间需要 `--discard-replay` 明说。

use diskedit_core::record::{Frame, RecoveryPlan};

use crate::app::observe::{Command, RecoverMode, RecoverSnapshot, Request};

use super::step::{Step, StepKind};
use super::undo::{gates, record_path};
use super::{Decision, Plan, Refusal};

/// 判定 `release` 的一次调用。
pub fn decide(snapshot: &RecoverSnapshot, request: &Request) -> Decision {
    let Command::Release { discard_replay } = request.command() else {
        return Decision::Refuse(Refusal::RecoveryRefused {
            why: "release reached with a different command".to_owned(),
        });
    };
    let discard_replay = *discard_replay;
    let identity = match gates(snapshot, "release") {
        Ok(identity) => identity,
        Err(refusal) => return Decision::Refuse(refusal),
    };
    // 放弃对没有记录的目标是成功空操作。
    let Some(bytes) = snapshot.record.bytes.as_deref() else {
        return Decision::Unchanged {
            note: format!(
                "no recovery record on {} — nothing to release",
                snapshot.target.display
            ),
        };
    };
    let plan = diskedit_core::record::recover(bytes, identity);
    release(plan, discard_replay, identity, &record_path(snapshot))
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

/// 记录里携带原像的区间数；只统计放弃会真丢掉的可回放区间。
fn replayable_ranges(frames: &[Frame]) -> usize {
    frames
        .iter()
        .flat_map(|frame| &frame.regions)
        .filter(|preimage| !preimage.data.is_empty())
        .count()
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

    fn request(command: Command) -> Request {
        Request::new(
            crate::app::observe::TargetSpec::new("disk.img", None),
            command,
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
            &request(Command::Release {
                discard_replay: false,
            }),
        );
        assert!(matches!(
            refused,
            Decision::Refuse(Refusal::RecoveryRefused { .. })
        ));

        let allowed = decide(
            &snapshot(Some(bytes), Occupancy::Idle),
            &request(Command::Release {
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

    // covers: I16
    #[test]
    fn releasing_a_record_for_another_target_is_refused() {
        // 记录自证属于别的载体：放弃也不能把它当作本目标的记录清掉。
        let other = RecordBody {
            target: Fingerprint::strong([0x99; 32]),
            geometry_generation: 1,
            plan_digest: [0x22; 32],
        };
        let frames = [frame(0, FrameKind::Intent, false, false, &[])];
        let bytes = encode(&other, &frames).unwrap();
        let decision = decide(
            &snapshot(Some(bytes), Occupancy::Idle),
            &request(Command::Release {
                discard_replay: true,
            }),
        );
        match decision {
            Decision::Refuse(Refusal::RecoveryRefused { why }) => {
                assert!(why.contains("different target"), "{why}");
            }
            other @ (Decision::Report { .. }
            | Decision::Execute { .. }
            | Decision::Unchanged { .. }
            | Decision::Refuse(_)) => {
                panic!("expected a refusal for a foreign target, got {other:?}")
            }
        }
    }
}
