//! `new` 的判定：容量、覆盖门、保护 MBR 三态与完整表写入计划。
//!
//! 这里只做纯计算：字节从哪里来（观察层）、写到哪（步骤区间）、什么情况下
//! 不写（拒绝成因）。磁盘 GUID 已由观察层从宿主随机源取好。

use diskedit_core::table::create::{self, PmbrSize, TableProbe};

use crate::app::observe::new::{NewSnapshot, PriorState};
use crate::app::observe::table::TableView;
use crate::app::observe::{Command, Request, TableFormat};
use crate::platform::Occupancy;

use super::digest::request_digest;
use super::step::{Step, StepKind, TableSegment};
use super::{Decision, Plan, Refusal};

/// 判定 `new`。
pub fn decide(snapshot: &NewSnapshot, request: &Request) -> Decision {
    let Command::New {
        table, overwrite, ..
    } = request.command()
    else {
        return Decision::Refuse(Refusal::ArgumentOutOfRange {
            what: "new decision reached with a different command".to_owned(),
        });
    };
    let format = *table;
    let overwrite = *overwrite;
    if snapshot.is_partition_node {
        return Decision::Refuse(Refusal::PartitionNodeTarget {
            command: "new".to_owned(),
            target: snapshot.target.display.clone(),
        });
    }
    let Some(identity) = snapshot.target.identity else {
        return Decision::Refuse(Refusal::IdentityUnprovable {
            why: "the target gives no comparable identity".to_owned(),
        });
    };

    // 未收尾的记录：同一请求可以续跑，别的请求必须先把记录处理掉。
    let request_digest = request_digest(
        Some(identity),
        format,
        overwrite,
        snapshot.target.sector_size,
    );
    let resume = match &snapshot.prior {
        PriorState::Absent => false,
        PriorState::Unreadable => {
            return Decision::Refuse(Refusal::RecordUnreadable {
                path: record_path(snapshot),
            });
        }
        PriorState::Present(prior) if prior.pending => {
            if prior.plan_digest != request_digest {
                return Decision::Refuse(Refusal::RecordConflict {
                    path: record_path(snapshot),
                });
            }
            true
        }
        PriorState::Present(_) => false,
    };

    let capacity = match format {
        TableFormat::Gpt => create::Format::Gpt,
        TableFormat::Mbr => create::Format::Mbr,
    };
    if let Err(error) =
        create::check_capacity(capacity, snapshot.target.bytes, snapshot.target.sector_size)
    {
        return Decision::Refuse(Refusal::TargetTooSmall {
            why: error.describe(),
        });
    }

    let recognized = matches!(
        snapshot.table,
        TableView::Gpt { .. } | TableView::Mbr { .. } | TableView::Damaged { .. }
    );
    let blank = create::regions_blank(
        &snapshot
            .captured
            .iter()
            .map(|region| (region.region, region.bytes.clone()))
            .collect::<Vec<_>>(),
    );
    let probe = TableProbe::classify(recognized, blank);
    if !resume && probe != TableProbe::Empty && !overwrite {
        return Decision::Refuse(Refusal::TableOccupied {
            probe: probe.as_str().to_owned(),
            regions: describe_regions(snapshot),
        });
    }

    // 占用闸：目标上任一分区在用（镜像：任一绑定到它的 loop 在用）即拒绝。
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

    let total_lba = snapshot.target.bytes / snapshot.target.sector_size;
    let segments = match build_segments(snapshot, format, total_lba) {
        Ok(segments) => segments,
        Err(refusal) => return Decision::Refuse(refusal),
    };
    let regions = segments.iter().map(|segment| segment.region).collect();
    let step = Step::new(
        StepKind::WriteTable {
            format,
            segment_size: snapshot.target.sector_size,
            stamp: snapshot.stamp,
            segments,
        },
        regions,
    );
    match Plan::write(vec![step], identity) {
        Ok(plan) => Decision::Execute {
            plan: plan.with_request_digest(request_digest),
            note: format!(
                "created {} table (verify with: diskedit info {})",
                match format {
                    TableFormat::Gpt => "gpt",
                    TableFormat::Mbr => "mbr",
                },
                snapshot.target.display
            ),
        },
        Err(refusal) => Decision::Refuse(refusal),
    }
}

fn build_segments(
    snapshot: &NewSnapshot,
    format: TableFormat,
    total_lba: u64,
) -> Result<Vec<TableSegment>, Refusal> {
    match format {
        TableFormat::Gpt => {
            let content = create::gpt_content(
                snapshot.target.bytes,
                snapshot.target.sector_size,
                snapshot.disk_guid,
            )
            .map_err(|error| Refusal::GeometryUnknown {
                why: error.to_string(),
            })?;
            let lba0 = lba0_bytes(snapshot);
            if let Some(lba0) = &lba0
                && let PmbrSize::Inconsistent = create::pmbr_size_state(
                    lba0,
                    snapshot.target.bytes,
                    snapshot.target.sector_size,
                )
            {
                return Err(Refusal::PmbrInconsistent {
                    why: "the protective MBR size exceeds both sector-size interpretations"
                        .to_owned(),
                });
            }
            let content = match &lba0 {
                Some(lba0) => content.with_protective(lba0, total_lba),
                None => content,
            };
            Ok(content
                .segments
                .into_iter()
                .map(|segment| TableSegment {
                    region: segment.region,
                    bytes: segment.bytes,
                })
                .collect())
        }
        TableFormat::Mbr => {
            let mut segments = Vec::with_capacity(snapshot.captured.len());
            for captured in &snapshot.captured {
                let bytes = if captured.region.start() == 0 {
                    create::new_mbr_lba0(&captured.bytes, snapshot.target.sector_size)
                } else {
                    vec![0u8; captured.bytes.len()]
                };
                segments.push(TableSegment {
                    region: captured.region,
                    bytes,
                });
            }
            Ok(segments)
        }
    }
}

fn lba0_bytes(snapshot: &NewSnapshot) -> Option<Vec<u8>> {
    snapshot
        .captured
        .iter()
        .find(|region| region.region.start() == 0)
        .map(|region| region.bytes.clone())
}

fn describe_regions(snapshot: &NewSnapshot) -> String {
    let mut parts = Vec::new();
    for captured in &snapshot.captured {
        parts.push(format!(
            "{} [{}, {})",
            captured.label,
            captured.region.start(),
            captured.region.end()
        ));
    }
    parts.join(", ")
}

fn record_path(snapshot: &NewSnapshot) -> String {
    snapshot
        .record
        .file
        .as_ref()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| "(record path unresolved)".to_owned())
}
