//! 分区复制：按块搬运源分区内容到目标分区。
//!
//! 只写目标分区声明的区间；源分区只读。块大小固定，超长复制不一次性读入内存。

use diskedit_core::region::Region;

use super::super::token::Recorded;
use super::super::{ExecError, WriteChannel};
use super::Context;
use super::table;

/// 单块字节数。
const CHUNK: u64 = 1 << 20;

/// 把源分区内容复制到目标分区；只覆盖源分区长度，不扩张目标。
pub fn apply(
    source: u32,
    target: u32,
    channel: &mut WriteChannel<'_>,
    token: &Recorded,
    ctx: &Context<'_>,
) -> Result<(), ExecError> {
    let (source_lba, source_sectors) = table::partition_span(channel, ctx.sector, source)?;
    let (target_lba, target_sectors) = table::partition_span(channel, ctx.sector, target)?;
    if target_sectors < source_sectors {
        return Err(ExecError::InvalidPlan(format!(
            "目标分区 {target} 小于源分区 {source}"
        )));
    }
    let mut remaining = source_sectors * ctx.sector;
    let mut offset = 0u64;
    while remaining > 0 {
        let take = remaining.min(CHUNK);
        let bytes = channel.read_exact(source_lba * ctx.sector + offset, take as usize)?;
        let region = Region::sized(target_lba * ctx.sector + offset, take)
            .map_err(|error| ExecError::InvalidPlan(format!("空区间: {error:?}")))?;
        channel.write(token, region, &bytes)?;
        offset += take;
        remaining -= take;
    }
    Ok(())
}
