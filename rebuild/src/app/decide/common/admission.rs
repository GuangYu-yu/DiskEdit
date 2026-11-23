//! 写命令的准入：身份、几何、占用、拓扑与外部程序。

use diskedit_core::evidence::{Evidence, Observation};
use diskedit_core::fingerprint::{Fingerprint, Strength};
use diskedit_core::fs::Program;
use diskedit_core::table::gpt::parse::GptCopy;
use diskedit_core::table::{GptEvidence, Mbr};

use super::super::TableFormat;
use super::super::refusal::Refusal;
use crate::app::observe::{PartitionFacts, Snapshot};

/// 通过准入的事实视图。
pub struct Admission<'a> {
    pub identity: Fingerprint,
    pub length: u64,
    pub sector: u64,
    pub partitions: &'a [PartitionFacts],
}

/// 检查写命令共有的前置条件。
pub fn admit(snapshot: &Snapshot) -> Result<Admission<'_>, Refusal> {
    let identity = strong_identity(snapshot)?;
    let length = known(&snapshot.disk.byte_len, "target byte length")?;
    let sector = known(&snapshot.disk.sector_size, "target logical block size")?;
    match &snapshot.occupancy {
        Observation::Known { value, .. } => {
            if value.is_occupied() {
                return Err(Refusal::Occupied {
                    evidence: summarize(value),
                });
            }
        }
        Observation::Unknown { why } => {
            return Err(Refusal::OccupancyUnknown { why: why.clone() });
        }
        Observation::NotApplicable { why } => {
            return Err(Refusal::OccupancyUnknown { why: why.clone() });
        }
    }
    let partitions = snapshot
        .contents()
        .ok_or_else(|| Refusal::FactUnknown {
            what: "partition topology".to_owned(),
        })?;
    Ok(Admission {
        identity,
        length,
        sector,
        partitions,
    })
}

/// 取强身份。
pub fn strong_identity(snapshot: &Snapshot) -> Result<Fingerprint, Refusal> {
    match &snapshot.identity {
        Observation::Known { value, .. } if value.strength() == Strength::Strong => Ok(*value),
        Observation::Known { .. } => Err(Refusal::IdentityUnprovable {
            why: "指纹等级不足以识别目标".to_owned(),
        }),
        Observation::Unknown { why } => Err(Refusal::IdentityUnprovable { why: why.clone() }),
        Observation::NotApplicable { why } => {
            Err(Refusal::IdentityUnprovable { why: why.clone() })
        }
    }
}

/// 目标上可用的分区表视图。
#[derive(Clone, Copy, Debug)]
pub enum TableView<'a> {
    Gpt(&'a GptCopy),
    Mbr(&'a Mbr),
}

impl TableView<'_> {
    pub fn format(&self) -> TableFormat {
        match self {
            Self::Gpt(_) => TableFormat::Gpt,
            Self::Mbr(_) => TableFormat::Mbr,
        }
    }
}

/// 读取可用的分区表；GPT 优先，GPT 缺席时回退 MBR。
///
/// 任一侧证据未知时不在缺证据的前提下继续。
pub fn table(snapshot: &Snapshot) -> Result<TableView<'_>, Refusal> {
    match &snapshot.table.gpt {
        Observation::Known {
            value: GptEvidence::Usable(copy),
            ..
        } => Ok(TableView::Gpt(copy)),
        Observation::Known {
            value: GptEvidence::Damaged(error),
            ..
        } => Err(Refusal::TableDamaged {
            why: error.to_string(),
        }),
        Observation::Known {
            value: GptEvidence::Absent,
            ..
        } => mbr_table(snapshot),
        Observation::Unknown { why } => Err(Refusal::FactUnknown {
            what: format!("GPT evidence: {why}"),
        }),
        Observation::NotApplicable { why } => Err(Refusal::FactUnknown {
            what: format!("GPT evidence: {why}"),
        }),
    }
}

fn mbr_table(snapshot: &Snapshot) -> Result<TableView<'_>, Refusal> {
    match &snapshot.table.mbr {
        Observation::Known {
            value: Evidence::Present(mbr),
            ..
        } => Ok(TableView::Mbr(mbr)),
        Observation::Known {
            value: Evidence::Absent,
            ..
        } => Err(Refusal::TableMissing),
        Observation::Known {
            value: Evidence::UnknownFormat { why },
            ..
        } => Err(Refusal::TableDamaged { why: why.clone() }),
        Observation::Known {
            value: Evidence::Unreadable { why },
            ..
        } => Err(Refusal::FactUnknown {
            what: format!("MBR evidence: {why}"),
        }),
        Observation::Unknown { why } => Err(Refusal::FactUnknown {
            what: format!("MBR evidence: {why}"),
        }),
        Observation::NotApplicable { why } => Err(Refusal::FactUnknown {
            what: format!("MBR evidence: {why}"),
        }),
    }
}

/// 所需外部程序全部可用时才放行。
pub fn require_tools(snapshot: &Snapshot, programs: &[Program]) -> Result<(), Refusal> {
    for program in programs {
        match snapshot.tool_state(*program) {
            Some(Observation::Known {
                value: crate::app::observe::tools::ToolState::Available(_),
                ..
            }) => {}
            Some(Observation::Known {
                value: crate::app::observe::tools::ToolState::Missing { .. },
                ..
            }) => return Err(Refusal::ToolMissing { program: *program }),
            Some(Observation::NotApplicable { why }) => {
                return Err(Refusal::PlatformCannotRunTools { why: why.clone() });
            }
            Some(Observation::Unknown { why }) => {
                return Err(Refusal::FactUnknown {
                    what: format!("availability of {program:?}: {why}"),
                });
            }
            None => {
                return Err(Refusal::FactUnknown {
                    what: format!("availability of {program:?}"),
                });
            }
        }
    }
    Ok(())
}

fn known<T: Copy>(fact: &Observation<T>, what: &str) -> Result<T, Refusal> {
    match fact {
        Observation::Known { value, .. } => Ok(*value),
        Observation::Unknown { why } => Err(Refusal::FactUnknown {
            what: format!("{what}: {why}"),
        }),
        Observation::NotApplicable { why } => Err(Refusal::FactUnknown {
            what: format!("{what}: {why}"),
        }),
    }
}

fn summarize(occupancy: &crate::platform::Occupancy) -> String {
    format!(
        "挂载 {} 项、交换区 {} 项、持有者 {} 项、阵列成员 {}、回环 {} 项",
        occupancy.mounts.len(),
        occupancy.swaps.len(),
        occupancy.holders.len(),
        if occupancy.array_member { "是" } else { "否" },
        occupancy.loops.len()
    )
}
