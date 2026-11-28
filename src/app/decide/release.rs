//! `release` 的判定：放弃回滚，只产出外部状态计划，不写目标字节。
//!
//! 闸门与 `undo` 共用。放弃之前还要过四道准入：目标身份与记录声明的
//! 身份一致、布局归属可确认、记录状态允许放弃、记录可完整解码。能确认是外来
//! 布局的记录仍可显式释放；头部或载荷前缀读不出来时归属无从证明，拒绝动它。

use diskedit_core::record::{Frame, ManualReason, RecoveryExpectation, RecoveryPlan};

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
    // 只有确认没有记录时，放弃才是成功空操作。
    let path = record_path(snapshot);
    let Some(bytes) = snapshot.record.bytes.as_deref() else {
        return match snapshot.record.unavailable.as_deref() {
            Some(_) => Decision::Refuse(Refusal::RecordUnreadable { path }),
            None => Decision::Unchanged {
                note: format!(
                    "no recovery record on {} — nothing to release",
                    snapshot.target.display
                ),
            },
        };
    };
    let plan = diskedit_core::record::recover(
        bytes,
        RecoveryExpectation::new(identity, snapshot.target.bytes),
    );
    release(plan, discard_replay, identity, &path)
}

/// 放弃回滚：只建外部状态计划，目标字节不变。
fn release(
    plan: RecoveryPlan,
    discard_replay: bool,
    identity: diskedit_core::fingerprint::Fingerprint,
    path: &str,
) -> Decision {
    match admit(plan, discard_replay, path) {
        Ok(()) => external_release(identity, RecoverMode::Release { discard_replay }),
        Err(refusal) => Decision::Refuse(refusal),
    }
}

/// 释放的四道准入；判定层与执行层共用这一份判据。
///
/// - 身份：记录声明属于别的目标时拒绝，不替它清掉回滚凭据；
/// - 归属：头部或载荷前缀读不出来时无法确认布局归属，拒绝；
/// - 状态：仍能回放的区间、或已无法枚举的区间，需要显式 `--discard-replay`；
/// - 解码：能确认是本实现的布局、只是载荷不完整时，按上述状态闸处理。
pub(crate) fn admit(plan: RecoveryPlan, discard_replay: bool, path: &str) -> Result<(), Refusal> {
    match plan {
        // 记录文件为空：既没有可读的标识，也没有可读的目标身份。
        RecoveryPlan::Absent => Err(Refusal::RecordDamaged {
            why: format!("the record at {path} is empty — its layout and target cannot be read"),
        }),
        RecoveryPlan::Committed => Ok(()),
        RecoveryPlan::RollBack { frames } | RecoveryPlan::Replay { frames } => {
            let replayable = replayable_ranges(&frames);
            if replayable > 0 && !discard_replay {
                return Err(Refusal::RecoveryRefused {
                    why: format!(
                        "the record still holds {replayable} replayable range(s); pass \
                         --discard-replay to give them up"
                    ),
                });
            }
            Ok(())
        }
        // 外来说明本实现不读其载荷；布局由标识确认，显式释放仍然允许。
        RecoveryPlan::Foreign { .. } => Ok(()),
        RecoveryPlan::NeedsManual { why } => match why {
            ManualReason::DifferentTarget => Err(Refusal::RecordForeign {
                why: format!("the record at {path} belongs to a different target: {why}"),
            }),
            ManualReason::HeaderCrc
            | ManualReason::ShortHeader { .. }
            | ManualReason::PayloadTooLarge(_)
            | ManualReason::ExcessBytes(_)
            | ManualReason::BodyCrc => Err(Refusal::RecordDamaged {
                why: format!(
                    "the record at {path} cannot be read as this implementation's layout: {why}"
                ),
            }),
            // 载荷前缀可读、目标身份已核对；能回放的区间数无从枚举。
            ManualReason::PayloadCrc | ManualReason::Frames(_) | ManualReason::LegacyGeneration => {
                if discard_replay {
                    Ok(())
                } else {
                    Err(Refusal::RecoveryRefused {
                        why: format!(
                            "the record at {path} is not fully readable, so the ranges it could \
                             still replay are unknown; pass --discard-replay to give them up"
                        ),
                    })
                }
            }
        },
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
    fn releasing_a_replayable_record_needs_discard() {
        let frames = [
            frame(0, FrameKind::Intent, false, false, &[]),
            frame(1, FrameKind::Preimage, true, false, &[0xAA; 4]),
        ];
        let bytes = encode(&body(MIB), &frames).unwrap();
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

    #[test]
    fn releasing_a_record_for_another_target_is_refused() {
        // 记录自证属于别的载体：放弃也不能把它当作本目标的记录清掉。
        let other = RecordBody {
            target: Fingerprint::strong([0x99; 32]),
            legacy_generation: 0,
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
            Decision::Refuse(Refusal::RecordForeign { why }) => {
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
