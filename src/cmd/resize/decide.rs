//! 表类型决策：GPT/MBR 的目标分区与设备路径。

use super::*;

/// PV / --grow-lv 的事前判据：与表类型无关，故 GPT 与 MBR 两条 resize 路径共用一份。
/// 两条判据都必须落在任何写盘之前——PV 缩容链恰好与扩容反向：先 lvreduce -r 缩 LV
/// 与 FS，再 pvresize --setphysicalvolumesize 缩 PV 元数据，最后才改分区表。本工具
/// 只管最后一步，前两步留给用户，否则先改表会留下"分区已缩、PV 元数据未动"的不一致
pub(crate) fn check_pv_intent(
    part: u32,
    fstype: Option<fsid::FsKind>,
    is_pv: bool,
    shrinking: bool,
    grow_lv: bool,
    no_fs: bool,
) -> Result<(), crate::outcome::Fail> {
    // PV 链（pvresize/lvextend -r）没有可跳过的"分区文件系统步"：旗标在此整条链
    // 都无消费点，静默照跑即命令做了旗标明确排除的事却报成功
    if is_pv && no_fs {
        return Err(crate::outcome::Fail::refused(
            "--no-fs has no effect on an LVM PV — the partition holds no filesystem of its own; drop --no-fs".to_string(),
        ));
    }
    if is_pv {
        if shrinking {
            return Err(crate::outcome::Fail::refused(
                "shrinking an LVM PV needs the lvreduce/pvresize chain — do it manually (see pvresize(8))",
            ));
        }
    } else if grow_lv {
        return Err(crate::outcome::Fail::refused(format!(
            "partition {part} is not an LVM PV (identified as {}) — --grow-lv needs a PV",
            fsid::fs_name(fstype)
        )));
    }
    Ok(())
}

/// 锁下解析在线目标长度（GPT）：几何解析与右侧空闲都取自**只读打开的整盘**，锚定值
/// 因此来自锁下现状。
/// `part` 是**锁前由选择器解析出的分区号**：在线模块正是按它（PV 的 pno）或它的挂载点
/// 定位目标，锁下重解析 `:last` 会指向另一个分区、把某个分区的尺寸算到另一个分区头上；
/// 该分区在锁下已消失/清空则由 `live_entry` 拒绝
#[cfg(target_os = "linux")]
pub(crate) fn gpt_decider<'a>(
    req: &'a SizeRequest,
    part: u32,
    is_pv: bool,
) -> impl FnOnce(&FileSource) -> Result<Option<u64>, Fail> + 'a {
    online_size(req, is_pv, move |disk| {
        let (g, _repair) = crate::gpt_policy::require_gpt_geometry(disk, "resize")?;
        let e = crate::gpt_policy::live_entry(&g, part)?;
        let cur = lba_range_bytes(e.starting_lba, e.ending_lba, g.sector_size());
        // 可用区是闭区间 [first_usable, last_usable]：本分区最大可占的扇区数即
        // 起点到 last_usable 的跨度；几何构造已保证起点落在可用区内，故不会下溢
        let max_len = g
            .last_usable_lba()
            .checked_sub(e.starting_lba)
            .and_then(|n| n.checked_add(1))
            .ok_or_else(|| Fail::infra("usable-range arithmetic overflow".to_string()))?;
        Ok((g.sector_size(), cur, free_right_gpt(&g, part), Some(max_len)))
    })
}

/// 锁下解析在线目标长度（MBR）：几何与右侧空闲同样取自锁下的整盘；MBR 无搬移能力，
/// 空闲上界由 32 位 LBA 上限与后继条目界定（见 `free_right_msdos`）。
/// `part` 同 `gpt_decider`：锁前解析出的分区号，锁下不再重解释选择器
#[cfg(target_os = "linux")]
pub(crate) fn mbr_decider<'a>(
    req: &'a SizeRequest,
    part: u32,
    is_pv: bool,
) -> impl FnOnce(&FileSource) -> Result<Option<u64>, Fail> + 'a {
    online_size(req, is_pv, move |disk| {
        let mbr = table::parse_mbr(disk)
            .map_err(|e| Fail::infra(format!("parse failed: {e}")))?
            .ok_or_else(|| Fail::refused("no MBR on target".to_string()))?;
        let p = mbr.iter().find(|p| p.num == part)
            .ok_or_else(|| Fail::refused(format!("partition {part} not found (MBR resize covers primary partitions 1..4 only)")))?;
        let ss = disk.sector_size;
        Ok((ss, lba_bytes(p.size_lba as u64, ss), free_right_msdos(&mbr, p, disk.size / ss), None))
    })
}
