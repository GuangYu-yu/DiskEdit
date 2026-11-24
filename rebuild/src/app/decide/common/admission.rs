//! 写命令的准入：身份、几何、占用、拓扑与外部程序。

use diskedit_core::evidence::{Evidence, Observation};
use diskedit_core::fingerprint::{Fingerprint, Strength};
use diskedit_core::fs::Program;
use diskedit_core::fs::catalog::{self, Support};
use diskedit_core::fs::profile::ContentKind;
use diskedit_core::table::gpt::entries::PartitionEntry;
use diskedit_core::table::gpt::parse::GptCopy;
use diskedit_core::table::mbr::primary::MbrEntry;
use diskedit_core::table::{GptEvidence, Mbr};

use crate::app::decide::Refusal;
use crate::app::observe::snapshot::PartitionFacts;
use crate::app::observe::{Snapshot, TableFormat};

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
    let partitions = snapshot.contents().ok_or_else(|| Refusal::FactUnknown {
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
            why: "fingerprint strength is insufficient to identify the target".to_owned(),
        }),
        Observation::Unknown { why } => Err(Refusal::IdentityUnprovable { why: why.clone() }),
        Observation::NotApplicable { why } => Err(Refusal::IdentityUnprovable { why: why.clone() }),
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

/// 目标的逻辑块大小。
pub fn sector(snapshot: &Snapshot) -> Result<u64, Refusal> {
    known(&snapshot.disk.sector_size, "target logical block size")
}

/// 把分区选择器解析成具体分区号。
///
/// `:last` 取拓扑中的最大分区号；拓扑未知或没有分区时按未知拒绝，不猜一个数。
pub fn select(
    snapshot: &Snapshot,
    selector: crate::app::observe::PartitionSelector,
) -> Result<u32, Refusal> {
    use crate::app::observe::PartitionSelector;
    match selector {
        PartitionSelector::Number(number) => {
            if number == 0 {
                Err(Refusal::ArgumentOutOfRange {
                    what: "partition number cannot be 0".to_owned(),
                })
            } else {
                Ok(number)
            }
        }
        PartitionSelector::Last => snapshot
            .contents()
            .ok_or_else(|| Refusal::FactUnknown {
                what: "partition topology".to_owned(),
            })?
            .iter()
            .map(|facts| facts.partition.number)
            .max()
            .ok_or_else(|| Refusal::FactUnknown {
                what: "target has no partitions; cannot resolve :last".to_owned(),
            }),
    }
}

/// 指定分区，缺失即拒绝。
pub fn partition(snapshot: &Snapshot, number: u32) -> Result<&PartitionFacts, Refusal> {
    snapshot
        .partition(number)
        .ok_or(Refusal::PartitionMissing { number })
}

/// 分区的内容类别。
pub fn content(facts: &PartitionFacts) -> Result<ContentKind, Refusal> {
    match &facts.content {
        Observation::Known { value, .. } => Ok(*value),
        Observation::Unknown { why } => Err(Refusal::FactUnknown {
            what: format!("content of partition {}: {why}", facts.partition.number),
        }),
        Observation::NotApplicable { why } => Err(Refusal::FactUnknown {
            what: format!("content of partition {}: {why}", facts.partition.number),
        }),
    }
}

/// 取某项能力需要的程序；不支持时给出内容类别与原因。
pub fn programs(
    kind: ContentKind,
    verb: catalog::Verb,
) -> Result<Option<&'static [Program]>, Refusal> {
    match catalog::profile(kind).support(verb) {
        Support::Tools(tools) => Ok(Some(tools)),
        Support::NotApplicable => Ok(None),
        Support::Unsupported(why) => Err(Refusal::ContentUnsupported {
            what: format!("{} {verb:?}", kind.as_str()),
            why: why.to_owned(),
        }),
    }
}

/// GPT 中已使用的条目及其分区号。
pub fn gpt_used(copy: &GptCopy) -> impl Iterator<Item = (u32, &PartitionEntry)> {
    copy.entries
        .iter()
        .enumerate()
        .filter(|(_, entry)| !entry.partition_type_guid.is_zero())
        .map(|(index, entry)| (index as u32 + 1, entry))
}

/// GPT 条目数组里的第一个空槽。
pub fn gpt_free_slot(copy: &GptCopy) -> Option<u32> {
    copy.entries
        .iter()
        .position(|entry| entry.partition_type_guid.is_zero())
        .map(|index| index as u32 + 1)
}

/// MBR 主分区中已使用的条目。
pub fn mbr_used(mbr: &Mbr) -> impl Iterator<Item = (u32, MbrEntry)> + '_ {
    mbr.entries
        .iter()
        .enumerate()
        .filter(|(_, entry)| !entry.is_empty())
        .map(|(index, entry)| (index as u32 + 1, *entry))
}

/// MBR 主分区里的第一个空槽。
pub fn mbr_free_slot(mbr: &Mbr) -> Option<u32> {
    mbr.entries
        .iter()
        .position(|entry| entry.is_empty())
        .map(|index| index as u32 + 1)
}

/// 两个半开区间是否重叠。
pub fn overlaps(a_start: u64, a_len: u64, b_start: u64, b_len: u64) -> bool {
    let a_end = a_start.saturating_add(a_len);
    let b_end = b_start.saturating_add(b_len);
    a_start < b_end && b_start < a_end
}

fn summarize(occupancy: &crate::platform::Occupancy) -> String {
    format!(
        "{} mounts, {} swaps, {} holders, array member {}, {} loops",
        occupancy.mounts.len(),
        occupancy.swaps.len(),
        occupancy.holders.len(),
        if occupancy.array_member { "yes" } else { "no" },
        occupancy.loops.len()
    )
}
