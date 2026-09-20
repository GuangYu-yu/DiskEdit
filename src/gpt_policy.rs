//! GPT 修复策略层：消费 table 解析出的事实（GptState / PmbrSize / 几何），
//! 产出可执行的 RepairAction，并用 table 的写入原语落盘。
//!
//! 分层：table.rs = 事实与解析（"是什么"）/ 本模块 = 决定做什么（"怎么办"）/
//! movepart.rs = 搬移与提交（"怎么写"）。
//! "是否需要修复、修哪一类"只在这里判一次，main 与 movepart 共用同一个动作类型。

use crate::dev::FileSource;
use crate::geometry::{self, ValidatedGeometry};
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
    // 跨度取自唯一的几何构造点，不在此另算一遍 条目数 × 条目大小 ÷ 扇区
    let geom = geometry::EntryArrayGeometry::new(
        g.ss,
        g.header.size_of_partition_entry,
        g.header.number_of_partition_entries,
    )
    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
    let span = geom.lba_span();
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

/// 执行修复（写入路径唯一出口）：按动作重写双头 / 保护 MBR → 重读校验必须收敛。
/// 相位判据在函数体内显式落点——
/// - 首次目标写盘之前（重读表）：失败按 Infra 报，此时一个字节都没写，"盘可能已改变"不成立
/// - 从首次写盘起（commit_gpt / ensure_protective_mbr 及其后）：失败按 Failed 报，
///   写入量已无法断定
///
/// `None` 不写盘（调用方已在决策期确认无需修复）
pub(crate) fn apply_repair(src: &mut FileSource, action: &RepairAction) -> Result<(), Fail> {
    match action {
        RepairAction::None => return Ok(()),
        // RepairProtectiveMbr：无需写 GPT（由函数尾部的 ensure_protective_mbr 统一重写，即首次写盘）
        RepairAction::RepairProtectiveMbr => {}
        RepairAction::RelocateBackup { new_backup_lba, new_last_usable }
        | RepairAction::RelocateAndRepair { new_backup_lba, new_last_usable } => {
            let mut g = table::load_gpt(src)
                .map_err(|e| Fail::infra_io(table::into_io_error(e)))?
                .ok_or_else(|| Fail::infra("GPT vanished before repair"))?;
            g.header.last_usable_lba = *new_last_usable;
            table::commit_gpt(src, &g, *new_backup_lba)?;
        }
    }
    table::ensure_protective_mbr(src)?;
    let g2 = table::load_gpt(src).map_err(|e| Fail::failed(table::into_io_error(e).to_string()))?.ok_or_else(|| Fail::failed("re-read after repair failed"))?;
    if g2.state != GptState::Valid || g2.pmbr != PmbrSize::Normal {
        return Err(Fail::failed("repair did not converge — refusing"));
    }
    Ok(())
}

/// 所有需要做空间算术的命令（add/new/del/rename/flag/resize-part/copy）都先经过这里：
/// 读取 → 判定（classify_repair）→ 产出**可操作几何**（[`ValidatedGeometry`]，其
/// `last_usable_lba` 已是修复后将生效的值）。
///
/// 构造 [`ValidatedGeometry`] 即校验，因此写入路径拿到它之后不必再问"这个几何可用吗"：
/// - 条目数组几何自洽（条目数/单条目大小/自定字节上限，见 `EntryArrayGeometry::new`）
/// - **已定义条目互不重叠**（UEFI 2.10 §5.3.1 GPT overview：Each defined partition must not
///   overlap with any other defined partition）。本工具的空间派生事实（右侧空闲、搬移打包、
///   扩容终点）全部以不重叠为前提，故它必须在构造点被验过——这是拒绝，不是能力缺陷：
///   `info` 仍读 RawGpt，能告诉用户哪两条重叠了
///
/// **绝不写盘**：入参是 `&FileSource`，类型上就没有写入能力。决策与副作用分开是各自的
/// 唯一出口——修复动作由 [`apply_repair`] 执行，调用方须在**所有事前拒绝判定之后**才调用它，
/// 否则那些 `Refused`（承诺"本次未写盘"，退出码 10）就成了假话。
///
/// 表自身的几何自洽性（主头位置、可用区、数组位置、备份头是否越界）由 table 在解析层强制；
/// plan 不走这里（plan 不写盘，修复动作由 apply 执行）。
///
/// "修复后必须两份头都有效"这一条有规范依据：UEFI 2.10 §5.3.2 规定
/// "_Both the primary and backup GPTs must be valid before an attempt is made to grow the size
/// of a physical volume_"，理由同节给出——GPT 的恢复方案依赖备份头位于设备末端，容量变化后
/// 备份头必须随之搬移。本函数被所有空间算术命令共用（含不扩容的 del），对它们而言比规范更严：
/// 规范只规定了扩容场景的下限，并未禁止其他操作也要求双头有效
pub fn resolve_geometry(src: &FileSource) -> Result<Option<(ValidatedGeometry, RepairAction)>, Fail> {
    let g = match table::load_gpt(src) {
        Ok(Some(g)) => g,
        Ok(None) => return Ok(None),
        // 解析失败与"修不了"都发生在任何写入之前 → Infra（不能提示"盘可能已改变"）
        Err(e) => return Err(Fail::infra(format!("parse failed: {e}"))),
    };
    let file_last_lba = src.size / g.ss - 1;
    // 这些拒绝（PMBR SizeInLBA 越出容器、容器装不下备份头跨度、分区越出修复后的可用区）
    // 都是盘/容器自身的异常：改请求参数也无解，故归 infra
    let action = classify_repair(&g, file_last_lba).map_err(|e| Fail::infra(e.to_string()))?;
    // 修复后的可用区上界只从动作取（决策期已算好），不再由调用点各自推导一次
    let vg = ValidatedGeometry::new(&g, file_last_lba, action.new_last_usable())
        .map_err(|e| Fail::infra(e.to_string()))?;
    Ok(Some((vg, action)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::table::{HeaderIssue, PmbrIssue, RawHeader};

    /// 最小表事实：ss=512、128×128B 条目（数组跨度 = 32 扇区）、条目仅含 (start,end) 区间
    fn gpt(state: GptState, pmbr: PmbrSize, ents: &[(u64, u64)]) -> RawGpt {
        RawGpt {
            ss: 512,
            state,
            pmbr,
            header: RawHeader {
                primary_lba: 1,
                backup_lba: 999, // 旧末端：事实以 state 表达，字段值不参与判定
                first_usable_lba: 34,
                last_usable_lba: 999,
                disk_guid: [0; 16],
                partition_entry_lba: 2,
                number_of_partition_entries: 128,
                size_of_partition_entry: 128,
                header_size: 92,
            },
            entries: ents.iter().map(|&(s, e)| gptman::GPTPartitionEntry {
                partition_type_guid: [1; 16],
                unique_partition_guid: [2; 16],
                starting_lba: s,
                ending_lba: e,
                attribute_bits: 0,
                partition_name: "".into(),
            }).collect(),
        }
    }

    /// 判定矩阵：两条轴（state 是否需搬迁 / pmbr 是否需规范化）四组合 + Inconsistent 一票否决
    #[test]
    fn classify_repair_decision_matrix() {
        let valid = GptState::Valid;
        let stale = GptState::NeedsRepair { cause: HeaderIssue::BackupLbaStale { expected: 999, actual: 500 } };
        let pmbr_ok = PmbrSize::Normal;
        let pmbr_stale = PmbrSize::NeedsRepair { cause: PmbrIssue::Stale };

        assert_eq!(classify_repair(&gpt(valid, pmbr_ok, &[]), 2000).unwrap(), RepairAction::None);
        assert_eq!(
            classify_repair(&gpt(valid, pmbr_stale, &[]), 2000).unwrap(),
            RepairAction::RepairProtectiveMbr
        );
        assert_eq!(
            classify_repair(&gpt(stale, pmbr_ok, &[]), 2000).unwrap(),
            RepairAction::RelocateBackup { new_backup_lba: 2000, new_last_usable: 2000 - 32 - 1 }
        );
        assert_eq!(
            classify_repair(&gpt(stale, pmbr_stale, &[]), 2000).unwrap(),
            RepairAction::RelocateAndRepair { new_backup_lba: 2000, new_last_usable: 2000 - 32 - 1 }
        );
        // Inconsistent：可能是更大盘的截断副本，无论 state 如何都拒绝自动修复
        let inc = gpt(GptState::Valid, PmbrSize::Inconsistent, &[]);
        assert!(classify_repair(&inc, 2000).is_err());
    }

    /// 搬迁后的可用区上界：容量不足与既有分区越界都必须拒绝，绝不静默截断分区
    #[test]
    fn repaired_last_usable_bounds() {
        // 正常：new_last_usable = file_last − 跨度 − 1
        let g = gpt(GptState::Valid, PmbrSize::Normal, &[(100, 500)]);
        assert_eq!(repaired_last_usable(&g, 2000).unwrap(), 1967);
        // 分区末端越出搬迁后的可用区 → 拒绝
        assert!(repaired_last_usable(&g, 500).is_err());
        // 容器容不下 备份数组+备份头 的最小跨度 → 拒绝（下溢防护）
        assert!(repaired_last_usable(&g, 32).is_err());
        assert!(repaired_last_usable(&g, 33).is_err());
    }
}