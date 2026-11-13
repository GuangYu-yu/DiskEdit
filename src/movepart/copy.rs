//! 同表分区复制：新条目的数据拷贝与提交。

use super::*;

/// 事前判定的产物：自有数据 + 源条目的类型 GUID 与副本的 unique GUID
pub(crate) struct PreparedCopy {
    pub(crate) part: u32,
    pub(crate) src_start: u64,
    pub(crate) new_start: u64,
    pub(crate) new_end: u64,
    pub(crate) len: u64,
    pub(crate) type_guid: [u8; 16],
    pub(crate) unique_guid: [u8; 16],
    pub(crate) repair: RepairAction,
}

/// 事前判定（只读）：源分区存在性、目标范围与重叠。返回 `Err` 一律在任何写盘之前。
/// 几何与修复动作由调用方解析并下传，本函数纯只读不触盘。
pub(crate) fn prepare_copy(
    g: &ValidatedGeometry,
    repair: RepairAction,
    part: u32,
    new_start: u64,
    keep_guid: bool,
) -> Result<PreparedCopy, RunOutcome> {
    let e = gpt_policy::live_entry(g, part)?;
    let len = e.ending_lba - e.starting_lba + 1;
    let src_start = e.starting_lba;
    // checked：new_start 来自 CLI 原始输入（--align none 时无上界），回绕会骗过下方边界校验
    let new_end = new_start.checked_add(len - 1)
        .ok_or_else(|| RunOutcome::refused("copy target overflows address space"))?;
    if new_start < g.first_usable_lba() || new_end > g.last_usable_lba() {
        return Err(RunOutcome::refused("copy target outside usable range"));
    }
    for (i, other) in g.entries().iter().enumerate() {
        if other.ending_lba == 0 {
            continue;
        }
        if !(new_end < other.starting_lba || new_start > other.ending_lba) {
            return Err(RunOutcome::refused(format!("copy target overlaps partition #{}", i + 1)));
        }
    }
    Ok(PreparedCopy {
        part, src_start, new_start, new_end, len,
        type_guid: e.partition_type_guid,
        // 默认给副本一个新的 PARTUUID：两份相同的 GUID 会让按 PARTUUID 的挂载
        // 无法区分源与副本；--keep-partuuid 显式要求复制身份时才沿用源值
        unique_guid: if keep_guid { e.unique_partition_guid } else { new_partuuid()? },
        repair,
    })
}

/// 新的 PARTUUID：16 字节取自系统随机源，按 RFC 4122 v4 置版本位与 variant 位。
///
/// GPT 条目的 EFI_GUID 前三字段按小端落盘，规范文本的第 7 字节（版本位）对应
/// 磁盘字节 `raw[7]`，variant 位对应 `raw[8]`（与 `table::derive_guid` 同一位点）。
/// 随机源不可用时返回 `NoMutation`：此时尚未写盘，不回退到时间戳/进程号这类弱随机
pub(crate) fn new_partuuid() -> Result<[u8; 16], RunOutcome> {
    let mut guid = [0u8; 16];
    getrandom::fill(&mut guid)
        .map_err(|e| RunOutcome::no_mutation(format!("cannot read the system random source for a new PARTUUID: {e}")))?;
    guid[7] = (guid[7] & 0x0F) | 0x40;
    guid[8] = (guid[8] & 0x3F) | 0x80;
    Ok(guid)
}
