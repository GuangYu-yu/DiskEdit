//! 分区表证据：GPT 与 MBR 各自独立观测。
//!
//! 两套表分开记，读不到的 GPT 不抹掉已读到的 MBR；任一侧失败只让那一侧
//! 变成 `Unknown`。解析与损坏分类全部复用纯核。

use diskedit_core::evidence::{Evidence, Generation, Observation, Source};
use diskedit_core::table::gpt::parse::classify;
use diskedit_core::table::mbr::primary::{self, Mbr};
use diskedit_core::table::{GptEvidence, TableError};

use super::disk::DiskFacts;
use crate::platform::TargetHandle;

/// 分区表证据的产生点。
pub const SOURCE: Source = Source::new("target.table");

/// MBR 所在的首个逻辑块长度；MBR 固定落在 LBA0 的前 512 字节内。
const MBR_BYTES: usize = 512;
/// 一份 GPT 条目数组的字节数：128 条 × 128 字节。
const ENTRY_ARRAY_BYTES: usize = 128 * 128;

/// 目标上的两套分区表证据。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TableFacts {
    pub gpt: Observation<GptEvidence>,
    pub mbr: Observation<Evidence<Mbr>>,
}

/// 读出两套分区表证据。
pub fn observe(
    handle: Option<&dyn TargetHandle>,
    disk: &DiskFacts,
    generation: Generation,
) -> TableFacts {
    TableFacts {
        gpt: observe_gpt(handle, disk, generation),
        mbr: observe_mbr(handle, generation),
    }
}

fn observe_gpt(
    handle: Option<&dyn TargetHandle>,
    disk: &DiskFacts,
    generation: Generation,
) -> Observation<GptEvidence> {
    let Some(handle) = handle else {
        return Observation::unknown("cannot open target; GPT not read");
    };
    match read_gpt(handle, disk) {
        Ok(evidence) => Observation::known(evidence, SOURCE, generation),
        Err(why) => Observation::unknown(why),
    }
}

fn read_gpt(handle: &dyn TargetHandle, disk: &DiskFacts) -> Result<GptEvidence, String> {
    let sector = disk
        .sector_size
        .value()
        .copied()
        .ok_or_else(|| "logical block size unknown; cannot locate GPT header".to_owned())?;
    let length = disk
        .byte_len
        .value()
        .copied()
        .ok_or_else(|| "target length unknown; cannot locate backup GPT".to_owned())?;
    if sector < 512 {
        return Err(format!(
            "logical block size {sector} is below 512 bytes; cannot hold GPT"
        ));
    }
    let last_lba = length
        .checked_div(sector)
        .and_then(|blocks| blocks.checked_sub(1))
        .ok_or_else(|| "target is shorter than one logical block".to_owned())?;
    if last_lba < 2 {
        return Err("target too small to hold GPT header".to_owned());
    }
    let sector_usize =
        usize::try_from(sector).map_err(|_| "logical block size exceeds addressable range")?;
    let primary_header = super::read_exact_at(handle, sector, sector_usize)?;
    let primary_array = super::read_exact_at(handle, 2 * sector, ENTRY_ARRAY_BYTES)?;
    let backup_span = u64::try_from(ENTRY_ARRAY_BYTES)
        .map_err(|_| "entry array length exceeds u64 range")?
        .div_ceil(sector)
        .max(1);
    let backup_array_lba = last_lba
        .checked_sub(backup_span)
        .ok_or_else(|| "target too small to hold backup entry array".to_owned())?;
    let backup_offset = last_lba
        .checked_mul(sector)
        .ok_or_else(|| "backup header offset exceeds addressable bound".to_owned())?;
    let backup_array_offset = backup_array_lba
        .checked_mul(sector)
        .ok_or_else(|| "backup entry array offset exceeds addressable bound".to_owned())?;
    let backup_header = super::read_exact_at(handle, backup_offset, sector_usize)?;
    let backup_array = super::read_exact_at(handle, backup_array_offset, ENTRY_ARRAY_BYTES)?;
    Ok(classify(
        &primary_header,
        &primary_array,
        &backup_header,
        &backup_array,
        last_lba,
        sector,
    ))
}

fn observe_mbr(
    handle: Option<&dyn TargetHandle>,
    generation: Generation,
) -> Observation<Evidence<Mbr>> {
    let Some(handle) = handle else {
        return Observation::unknown("cannot open target; MBR not read");
    };
    let sector = match super::read_exact_at(handle, 0, MBR_BYTES) {
        Ok(bytes) => bytes,
        Err(why) => return Observation::unknown(why),
    };
    let evidence = match primary::parse(&sector) {
        Ok(mbr) => Evidence::Present(mbr),
        Err(TableError::MbrSignatureMissing) => Evidence::Absent,
        Err(error) => Evidence::UnknownFormat {
            why: format!("failed to parse MBR: {error}"),
        },
    };
    Observation::known(evidence, SOURCE, generation)
}
