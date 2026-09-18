//! GPT 修复策略层：消费 table 解析出的事实（GptState / PmbrSize / 几何），
//! 产出可执行的 RepairAction，并用 table 的写入原语落盘。
//!
//! 分层：table.rs = 事实与解析（"是什么"）/ 本模块 = 决定做什么（"怎么办"）/
//! movepart.rs = 搬移与提交（"怎么写"）。
//! "是否需要修复、修哪一类"只在这里判一次，main 与 movepart 共用同一个动作类型。

use crate::dev::FileSource;
use crate::outcome::Fail;
use crate::table::{self, GptState, PmbrSize, RawGpt};
use std::io;

/// 修复动作（互斥、穷举）。不用 bool 旗标：动作本身就是状态，
/// 需要重算的 last_usable 在决策期算好后随动作携带（plan 自持，执行期不重新推导）
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RepairAction {
    /// 无需修复
    None,
    /// 备份头停在旧末端：重写双头并把备份头搬到设备最后 LBA
    RelocateBackup {
        new_backup_lba: u64,
        new_last_usable: u64,
    },
    /// 备份头已在末端，仅保护 MBR 的 SizeInLBA 过期
    RepairProtectiveMbr,
    /// 两者都需处理
    RelocateAndRepair {
        new_backup_lba: u64,
        new_last_usable: u64,
    },
}

impl RepairAction {
    /// 动作的人类可读描述（供 plan 输出与 apply 日志共用一处措辞）；None 动作无描述。
    /// 保护 MBR 的措辞不写成因：动作只表达"做什么"，成因（stale / 512 口径）由
    /// PmbrSize 这一事实轴承载，需要细看时用 info 读
    pub fn describe(&self) -> Option<String> {
        match self {
            Self::None => None,
            Self::RelocateBackup { new_backup_lba, new_last_usable } => Some(format!(
                "relocate backup GPT → LBA {new_backup_lba} (last_usable_lba → {new_last_usable})"
            )),
            Self::RepairProtectiveMbr => Some("rewrite protective MBR (SizeInLBA not the logical-block value for this container)".to_string()),
            Self::RelocateAndRepair { new_backup_lba, new_last_usable } => Some(format!(
                "relocate backup GPT → LBA {new_backup_lba} (last_usable_lba → {new_last_usable}) + rewrite protective MBR (SizeInLBA not the logical-block value for this container)"
            )),
        }
    }

    /// 搬迁后的 last_usable_lba（仅含搬迁的动作有值）
    pub fn new_last_usable(&self) -> Option<u64> {
        match self {
            Self::RelocateBackup { new_last_usable, .. } | Self::RelocateAndRepair { new_last_usable, .. } => {
                Some(*new_last_usable)
            }
            _ => None,
        }
    }
}

/// 修复后的 last_usable_lba = 新末端 − 数组跨度 − 1（备份数组位于备份头之前）。
/// 容器容不下最小跨度、或现有分区越出新区间 → 拒绝（不写盘，纯计算）
pub fn repaired_last_usable(g: &RawGpt, file_last_lba: u64) -> io::Result<u64> {
    let span = table::array_span_sectors(g.header.number_of_partition_entries, g.header.size_of_partition_entry, g.ss);
    let new_last_usable = file_last_lba
        .checked_sub(span)
        .and_then(|v| v.checked_sub(1))
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "container too small for GPT backup header — refusing"))?;
    let max_end = g.entries.iter().filter(|e| e.ending_lba != 0).map(|e| e.ending_lba).max().unwrap_or(0);
    if max_end > new_last_usable {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "partitions exceed new usable range after enlarge"));
    }
    Ok(new_last_usable)
}

/// 判定该表需要哪种修复（不写盘）。两条轴：备份头是否需要搬到新末端（GptState）、
/// 保护 MBR 是否需要规范化（PmbrSize）。只有 PMBR 值大于规范值且非已知口径
/// （Inconsistent，可能是更大盘的截断副本）才拒绝自动修复
pub fn classify_repair(g: &RawGpt, file_last_lba: u64) -> io::Result<RepairAction> {
    if g.pmbr == PmbrSize::Inconsistent {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "protective MBR SizeInLBA exceeds container — refusing auto-repair (use sgdisk/parted)",
        ));
    }
    let relocates = g.state != GptState::Valid;
    // stale 与 512 口径都是"可修复"：动作相同（按本容器重写），差别只在措辞
    let pmbr_needs_repair = matches!(g.pmbr, PmbrSize::NeedsRepair { .. });
    Ok(match (relocates, pmbr_needs_repair) {
        (false, false) => RepairAction::None,
        (false, true) => RepairAction::RepairProtectiveMbr,
        (true, false) => RepairAction::RelocateBackup {
            new_backup_lba: file_last_lba,
            new_last_usable: repaired_last_usable(g, file_last_lba)?,
        },
        (true, true) => RepairAction::RelocateAndRepair {
            new_backup_lba: file_last_lba,
            new_last_usable: repaired_last_usable(g, file_last_lba)?,
        },
    })
}

/// 执行修复（写入路径）：按动作重写双头 / 保护 MBR → 重读校验必须收敛
pub fn perform_repair(src: &mut FileSource, action: &RepairAction) -> io::Result<()> {
    match action {
        // None / RepairProtectiveMbr：无需写 GPT（后者由函数尾部的 ensure_protective_mbr 统一重写）
        RepairAction::None | RepairAction::RepairProtectiveMbr => {}
        RepairAction::RelocateBackup { new_backup_lba, new_last_usable }
        | RepairAction::RelocateAndRepair { new_backup_lba, new_last_usable } => {
            let mut g = table::load_gpt(src)
                .map_err(table::flatten)?
                .ok_or_else(|| io::Error::other("GPT vanished before repair"))?;
            g.header.last_usable_lba = *new_last_usable;
            table::commit_gpt(src, &g, *new_backup_lba)?;
        }
    }
    table::ensure_protective_mbr(src)?;
    let g2 = table::load_gpt(src).map_err(table::flatten)?.ok_or_else(|| io::Error::other("re-read after repair failed"))?;
    if g2.state != GptState::Valid || g2.pmbr != PmbrSize::Normal {
        return Err(io::Error::other("repair did not converge — refusing"));
    }
    Ok(())
}

/// 所有需要做空间算术的命令（add/new/del/rename/flag/resize-part）都先经过这里：
/// 读取 → 判定（classify_repair）→ 需要时就地修复（perform_repair）→ 返回修复后的表。
/// 表自身的几何自洽性（主头位置、可用区、备份头是否越界）由 table 在解析层强制；
/// plan 不走这里（plan 不写盘，修复动作由 apply 执行）。
///
/// "修复后必须两份头都有效"这一条有规范依据：UEFI 2.10 §5.3.2 规定
/// "_Both the primary and backup GPTs must be valid before an attempt is made to grow the size
/// of a physical volume_"，理由同节给出——GPT 的恢复方案依赖备份头位于设备末端，容量变化后
/// 备份头必须随之搬移。本函数被所有空间算术命令共用（含不扩容的 del），对它们而言比规范更严：
/// 规范只规定了扩容场景的下限，并未禁止其他操作也要求双头有效
/// 失败按"写没写盘"分类：解析失败与"修不了"都发生在任何写入之前 → `Infra`（不能提示
/// "盘可能已改变"）；一旦 perform_repair 开始写，后续失败就只能按 `Failed` 报
pub fn ensure_geometry(src: &mut FileSource) -> Result<Option<RawGpt>, Fail> {
    let g = match table::load_gpt(src) {
        Ok(g) => g,
        Err(e) => return Err(Fail::infra(format!("parse failed: {e}"))),
    };
    let Some(g) = g else { return Ok(None) };
    let file_last_lba = src.size / g.ss - 1;
    let action = classify_repair(&g, file_last_lba).map_err(|e| Fail::infra(e.to_string()))?;
    if action != RepairAction::None {
        perform_repair(src, &action)?;
        return match table::load_gpt(src) {
            Ok(Some(g)) => Ok(Some(g)),
            Ok(None) => Err(Fail::failed("GPT vanished while repairing")),
            // 修复已写过盘 → 重读失败只能按"可能已改变"处理
            Err(e) => Err(Fail::failed(format!("re-read after repair: {e}"))),
        };
    }
    Ok(Some(g))
}