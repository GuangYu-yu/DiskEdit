//! 表类型决策：GPT/MBR 的锁下目标长度与 PV 事前判据。

use super::*;
#[cfg(target_os = "linux")]
use crate::cli::SizeSpec;
#[cfg(any(target_os = "linux", test))]
use gptman::GPTPartitionEntry;

/// PV / --grow-lv 的事前判据：与表类型无关，故 GPT 与 MBR 两条 resize 路径共用一份。
/// 各条判据都必须落在任何写盘之前。PV 缩容按反向链执行：先 pvresize 把 PV 元数据缩到
/// 新末端（要求新末端之后没有已分配 extent，见 `pv_shrink_precheck`），再提交分区表；
/// 不做 pvmove/lvreduce。`--grow-lv` 只属于扩容方向，与缩容同时出现即请求自相矛盾
pub(crate) fn check_pv_intent(
    part: u32,
    fstype: Option<fsid::ContentKind>,
    is_pv: bool,
    shrinking: bool,
    grow_lv: bool,
    no_fs: bool,
) -> Result<(), crate::outcome::RunOutcome> {
    // PV 链（pvresize/lvextend -r）没有可跳过的"分区文件系统步"：旗标在此整条链
    // 都无消费点，静默照跑即命令做了旗标明确排除的事却报成功
    if is_pv && no_fs {
        return Err(crate::outcome::RunOutcome::refused(
            "--no-fs has no effect on an LVM PV — the partition holds no filesystem of its own; drop --no-fs".to_string(),
        ));
    }
    if is_pv {
        if shrinking && grow_lv {
            return Err(crate::outcome::RunOutcome::refused(
                "--grow-lv extends an LV by the space a partition gains — it cannot combine with shrinking this PV; drop --grow-lv".to_string(),
            ));
        }
    } else if grow_lv {
        return Err(crate::outcome::RunOutcome::refused(format!(
            "partition {part} is not an LVM PV (identified as {}) — --grow-lv needs a PV",
            fsid::content_name(fstype.unwrap_or(fsid::ContentKind::Unknown))
        )));
    }
    Ok(())
}

/// 身份闸纯函数：锁前快照与锁下条目不符即按 Refused 拒绝。只接收条目值、不持有盘
/// 句柄，拒绝分支不伴随任何写入；闭包在长度计算、写表与外部工具调用之前先过这道闸。
#[cfg(any(target_os = "linux", test))]
pub(crate) fn gpt_identity_or_refused(
    expected: &GPTPartitionEntry,
    actual: &GPTPartitionEntry,
    part: u32,
) -> Result<(), RunOutcome> {
    if crate::gpt_policy::gpt_identity_matches(expected, actual) {
        Ok(())
    } else {
        Err(RunOutcome::refused(format!(
            "partition {part} now identifies as a different entry than the pre-lock snapshot — refusing to act on the partition under that slot; re-resolve the target and retry"
        )))
    }
}

/// MBR 同口径身份闸：无 GUID，按起始 LBA 与容量核对，不符即 Refused。
#[cfg(any(target_os = "linux", test))]
pub(crate) fn mbr_identity_or_refused(
    expected: &table::MbrPartition,
    actual: &table::MbrPartition,
    part: u32,
) -> Result<(), RunOutcome> {
    if crate::gpt_policy::mbr_identity_matches(expected, actual) {
        Ok(())
    } else {
        Err(RunOutcome::refused(format!(
            "partition {part} now identifies as a different entry than the pre-lock snapshot — refusing to act on the partition under that slot; re-resolve the target and retry"
        )))
    }
}

/// 锁下解析在线目标长度（GPT）：几何解析与右侧空闲都取自**只读打开的整盘**，锚定值
/// 因此来自锁下现状。
/// `part` 是**锁前由选择器解析出的分区号**：在线模块正是按它（PV 的 pno）或它的挂载点
/// 定位目标，锁下重解析 `:last` 会指向另一个分区、把某个分区的尺寸算到另一个分区头上。
/// `expected` 是锁前读到的那条目快照：取锁后重读表、按号命中的条目必须与它身份一致
/// （GUID + 起止 LBA）；同号槽在窗口内被另一存活条目占用时即拒绝，动作不落到新条目上。
/// 条目已消失/清空由 `live_entry` 拒绝。
#[cfg(target_os = "linux")]
pub(crate) fn gpt_decider(
    size: SizeSpec,
    part: u32,
    is_pv: bool,
    expected: GPTPartitionEntry,
) -> impl FnOnce(&FileSource) -> Result<Option<u64>, RunOutcome> {
    online_size(size, is_pv, move |disk| {
        let (g, _repair) = crate::gpt_policy::require_gpt_geometry(disk, "resize")?;
        let e = crate::gpt_policy::live_entry(&g, part)?;
        gpt_identity_or_refused(&expected, e, part)?;
        let cur = lba_range_bytes(e.starting_lba, e.ending_lba, g.sector_size());
        // 可用区是闭区间 [first_usable, last_usable]：本分区最大可占的扇区数即
        // 起点到 last_usable 的跨度；几何构造已保证起点落在可用区内，故不会下溢
        let max_len = g
            .last_usable_lba()
            .checked_sub(e.starting_lba)
            .and_then(|n| n.checked_add(1))
            .ok_or_else(|| RunOutcome::no_mutation("usable-range arithmetic overflow".to_string()))?;
        Ok((g.sector_size(), cur, free_right_gpt(&g, part), Some(max_len)))
    })
}

/// 锁下解析在线目标长度（MBR）：几何与右侧空闲同样取自锁下的整盘；MBR 无搬移能力，
/// 空闲上界由 32 位 LBA 上限与后继条目界定（见 `free_right_msdos`）。
/// `part` 同 `gpt_decider`：锁前解析出的分区号，锁下不重解释选择器。
/// `expected` 是锁前 MBR 条目快照：锁下命中条目按起点 LBA 与容量核对，不符即拒绝。
#[cfg(target_os = "linux")]
pub(crate) fn mbr_decider(
    size: SizeSpec,
    part: u32,
    is_pv: bool,
    expected: table::MbrPartition,
) -> impl FnOnce(&FileSource) -> Result<Option<u64>, RunOutcome> {
    online_size(size, is_pv, move |disk| {
        let mbr = table::parse_mbr(disk)
            .map_err(|e| RunOutcome::refused(crate::support::parse_failed(&e)))?
            .ok_or_else(|| RunOutcome::refused("no MBR on target".to_string()))?;
        let p = mbr.iter().find(|p| p.num == part)
            .ok_or_else(|| RunOutcome::refused(format!("partition {part} not found (MBR resize covers primary partitions 1..4 only)")))?;
        mbr_identity_or_refused(&expected, p, part)?;
        let ss = disk.sector_size;
        Ok((ss, lba_bytes(p.size_lba as u64, ss), free_right_msdos(&mbr, p, disk.size / ss), None))
    })
}
