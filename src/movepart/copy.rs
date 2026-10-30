//! 同表分区复制：新条目的数据拷贝与提交。

use super::*;

/// 字节级分区复制：源不动，目标范围逐 chunk 复制后新增条目。
/// 目标范围由 add_entry_at 的重叠校验把关；数据复制先于表项提交。
/// 两半的返回类型就是 durable boundary 的类型表达（见 `prepare_copy` / `execute_copy`）。
/// 几何与修复动作由调用方解析并下传，本函数不读表
#[allow(clippy::too_many_arguments)] // 同 resize_part：参数即最小事实集
pub fn copy_part(
    src: &mut WriteSource,
    g: &ValidatedGeometry,
    repair: RepairAction,
    part: u32,
    new_start: u64,
    name: &str,
    keep_guid: bool,
    chunk_len: u64,
    log: &mut dyn FnMut(&str),
) -> Result<u32, Fail> {
    // 事务入口注入点与 apply / resize_part 同形：durable boundary 之前 abort 必须留下
    // 完全未改动的盘，这条不变量在 copy 上同样要可断言
    fault_before_any_write();
    let p = prepare_copy(g, repair, part, new_start, keep_guid)?;
    // 修复先于任何目标写入；放 Fail 域以保住分类（同 apply）
    if let Some(what) = p.repair.describe() {
        gpt_policy::apply_repair(src, &p.repair, p.ss)?;
        log(&format!("[repair] {what}"));
    }
    // 越界之后的 io 失败一律归 Failed（复制阶段不改表，但数据区已动）
    execute_copy(src, &p, name, chunk_len, log).map_err(Fail::from)
}

/// 事前判定的产物：自有数据 + 源条目的类型 GUID 与副本的 unique GUID
pub(crate) struct PreparedCopy {
    pub(crate) ss: u64,
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
/// 几何与修复动作由调用方解析并下传（本函数纯只读，不触盘）
///
/// 为何不像 prepare_resize / prepare_apply 那样调 ensure_idle_before_write：
/// copy 只读源分区、不搬移其 extent，只写经重叠校验的空闲区，不触发占用闸所防的
/// 两险（搬挂载中分区的 extent、表已写而 FS 步被占）；块设备另有整盘 O_EXCL
/// （见 dev.rs）兜底
pub(crate) fn prepare_copy(
    g: &ValidatedGeometry,
    repair: RepairAction,
    part: u32,
    new_start: u64,
    keep_guid: bool,
) -> Result<PreparedCopy, Fail> {
    let ss = g.sector_size();
    let e = gpt_policy::live_entry(g, part)?;
    let len = e.ending_lba - e.starting_lba + 1;
    let src_start = e.starting_lba;
    // checked：new_start 来自 CLI 原始输入（--align none 时无上界），回绕会骗过下方边界校验
    let new_end = new_start.checked_add(len - 1)
        .ok_or_else(|| Fail::refused("copy target overflows address space"))?;
    if new_start < g.first_usable_lba() || new_end > g.last_usable_lba() {
        return Err(Fail::refused("copy target outside usable range"));
    }
    for (i, other) in g.entries().iter().enumerate() {
        if other.ending_lba == 0 {
            continue;
        }
        if !(new_end < other.starting_lba || new_start > other.ending_lba) {
            return Err(Fail::refused(format!("copy target overlaps partition #{}", i + 1)));
        }
    }
    Ok(PreparedCopy {
        ss, part, src_start, new_start, new_end, len,
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
/// 随机源不可用时返回 `Infra`：此时尚未写盘，不回退到时间戳/进程号这类弱随机
pub(crate) fn new_partuuid() -> Result<[u8; 16], Fail> {
    let mut guid = [0u8; 16];
    getrandom::fill(&mut guid)
        .map_err(|e| Fail::infra(format!("cannot read the system random source for a new PARTUUID: {e}")))?;
    guid[7] = (guid[7] & 0x0F) | 0x40;
    guid[8] = (guid[8] & 0x3F) | 0x80;
    Ok(guid)
}

/// 执行（写盘）。返回 `io::Result`：函数体内构造不出 `Fail::Refused`
pub(crate) fn execute_copy(
    src: &mut WriteSource,
    p: &PreparedCopy,
    name: &str,
    chunk_len: u64,
    log: &mut dyn FnMut(&str),
) -> io::Result<u32> {
    let (ss, part, new_start, new_end, len) = (p.ss, p.part, p.new_start, p.new_end, p.len);
    // 与 execute_apply 的同类换算同口径（见那边的 byte_overflow）：这些 LBA 经 checked
    // 运算才能进地址计算，回绕会让复制落到盘上另一个位置
    let byte_overflow = || io::Error::new(io::ErrorKind::InvalidData, "copy byte offset overflows — refusing");
    let src_off = p.src_start.checked_mul(ss).ok_or_else(byte_overflow)?;
    let dst_off = new_start.checked_mul(ss).ok_or_else(byte_overflow)?;
    let total = len.checked_mul(ss).ok_or_else(byte_overflow)?;
    // 修复已由 copy 的 Fail 域在调用本函数之前完成，后续所有写入都基于修复后的几何
    // 含数据复制：字节不入 journal，留屏障令 undo 拒绝（前向恢复、无回滚）
    src.set_mutation(crate::dev::Mutation::CopyPartition {
        partition: p.part,
        from_lba: p.src_start,
        to_lba: new_start,
    });
    src.mark_non_reversible()?;
    let mut pos = 0u64;
    let total_n = total.div_ceil(chunk_len) as usize;
    while pos < total {
        let len_c = chunk_len.min(total - pos);
        let mut buf = vec![0u8; len_c as usize];
        src.read_at(src_off + pos, &mut buf).map_err(|e| {
            chunk_io_error(e, "reading source", (src_off + pos) / ss, len_c / ss, (pos / chunk_len + 1) as usize, total_n, COPY_HINT)
        })?;
        src.write_data_at(dst_off + pos, &buf).map_err(|e| {
            chunk_io_error(e, "writing target", (dst_off + pos) / ss, len_c / ss, (pos / chunk_len + 1) as usize, total_n, COPY_HINT)
        })?;
        src.sync_data()?;
        pos += len_c;
    }
    fault_copy_before_commit();
    // 副本的 boot sector 原样带来旧 HiddenSectors，按新起始位置修正必须**先于**表项
    // 提交：提交与修正之间中断，重跑会被 prepare_copy 的重叠判据挡在门外，副本的
    // HiddenSectors 永久停在旧值（Windows 会按错的分区偏移认盘）
    if crate::fsid::identify(src, dst_off, total)? == Some(FsKind::Ntfs) {
        fix_ntfs_hidden_sectors(src, new_start, ss, log)?;
    }
    // add_entry_at 自带"写盘前拒绝"语义，但它在本命令的边界之后：数据已经复制过去，
    // 此刻它报什么，对本次调用的结论都只能是"盘可能已改变"
    let num = gpt_policy::add_entry_at(
        src, new_start, new_end, name, p.type_guid, p.unique_guid,
    )
    .map_err(crate::outcome::into_io_error)?;
    log(&format!("partition {part} copied to #{num} at {new_start}..{new_end}"));
    Ok(num)
}