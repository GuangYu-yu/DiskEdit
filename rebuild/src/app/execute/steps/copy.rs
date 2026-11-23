//! 分区复制：按块把源分区内容搬到指定目标区间。
//!
//! 只写目标区间；源分区只读。判定层已保证目标区间不与任何既有分区（含源）重叠，
//! 因此边读边写不会覆盖尚未读取的源字节。

use diskedit_core::region::Region;

use super::super::token::Recorded;
use super::super::{ExecError, WriteChannel};
use super::Context;
use super::table;

/// 把源分区的前 `sectors` 个扇区复制到 `start_lba` 起的目标区间。
pub fn apply(
    source: u32,
    start_lba: u64,
    sectors: u64,
    chunk: u64,
    channel: &mut WriteChannel<'_>,
    token: &Recorded,
    ctx: &Context<'_>,
) -> Result<(), ExecError> {
    let (source_lba, source_sectors) = table::partition_span(channel, ctx.sector, source)?;
    if sectors > source_sectors {
        return Err(ExecError::InvalidPlan(format!(
            "复制长度 {sectors} 超过源分区 {source} 的 {source_sectors}"
        )));
    }
    let chunk = chunk.max(1);
    let mut remaining = sectors * ctx.sector;
    let mut offset = 0u64;
    while remaining > 0 {
        let take = remaining.min(chunk);
        let bytes = channel.read_exact(source_lba * ctx.sector + offset, take as usize)?;
        let region = Region::sized(start_lba * ctx.sector + offset, take)
            .map_err(|error| ExecError::InvalidPlan(format!("空区间: {error:?}")))?;
        channel.write(token, region, &bytes)?;
        offset += take;
        remaining -= take;
    }
    Ok(())
}
