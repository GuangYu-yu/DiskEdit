//! GPT 修复在判定层的收敛：需要修复的表直接并入本次表写，不另起一个事务。
//!
//! 修复动作与目标命令的表改动写在同一批字节里：主/备头由 [`GptTable::segments`] 按
//! 规范位置重写，保护 MBR 需要规范化时追加一段 LBA0。这样"先修复再命令"之间的
//! 中间态不存在，写事务的原像与回滚覆盖整张新表。

use diskedit_core::policy::gpt::repair_action;
use diskedit_core::region::Region;
use diskedit_core::table::create::{PmbrSize, Segment, protective_mbr_content};
use diskedit_core::table::edit::GptTable;
use diskedit_core::table::gpt::parse::GptState;

use super::Refusal;

/// 修复判定的结果：需要落地的保护 MBR 段（已合规时为 `None`）。
pub struct Repaired {
    pub protective_mbr: Option<Vec<u8>>,
}

/// 判定修复动作并把重算后的 `last_usable_lba` 落进表头。
///
/// `PmbrSize::Inconsistent` 与"容器容不下备份元数据""分区越出新上界"都归
/// `TableDamaged` 拒绝：这些是盘上的表不符合规范，改请求参数也无解。
pub fn adopt(
    table: &mut GptTable,
    state: GptState,
    pmbr: PmbrSize,
    lba0: &[u8],
) -> Result<Repaired, Refusal> {
    let last_lba = table.last_lba();
    let action = repair_action(
        &table.header,
        &table.entries,
        state,
        pmbr,
        last_lba,
        table.sector_size,
    )
    .map_err(|error| Refusal::TableDamaged {
        why: error.to_string(),
    })?;
    if let Some(new_last_usable) = action.new_last_usable() {
        table.header.last_usable_lba = new_last_usable;
    }
    let protective_mbr = if action.rewrites_protective_mbr() {
        // `protective_mbr_content` 的第二个参数是容器总扇区数，不是末 LBA。
        protective_mbr_content(lba0, last_lba + 1)
    } else {
        None
    };
    Ok(Repaired { protective_mbr })
}

/// 把保护 MBR 段追加到表段之后；无需重写时保持原样。
pub fn append_protective(segments: &mut Vec<Segment>, protective: Option<Vec<u8>>) {
    let Some(bytes) = protective else {
        return;
    };
    let Ok(region) = Region::sized(0, bytes.len() as u64) else {
        return;
    };
    segments.push(Segment { region, bytes });
}
