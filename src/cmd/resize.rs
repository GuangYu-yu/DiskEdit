//! resize：分区 + 文件系统的一键扩缩（GPT / MBR / superfloppy，在线与离线自动选择）。

mod size;
mod decide;
mod online;
mod lvm;
mod superfloppy;
mod msdos;
mod finish;

#[cfg(test)]
mod tests;

pub(crate) use size::*;
pub(crate) use decide::*;
#[cfg(target_os = "linux")]
pub(crate) use online::*;
#[cfg(target_os = "linux")]
pub(crate) use lvm::*;
pub(crate) use superfloppy::*;
pub(crate) use msdos::*;
pub(crate) use finish::*;

use crate::support::*;
use crate::args::{parse_size_delta, Args, SizeArg};
use crate::dev::{FileSource, PartSelector, WriteSource};
use crate::{dev, fsid, fsops, movepart, table};
#[cfg(target_os = "linux")]
use crate::lvm::LvmError;

pub(crate) const HELP: &str = r#"diskedit resize <TARGET>[:N] <SIZE> [OPTIONS]

  Resize partition and its filesystem, partition -> PV -> LV -> filesystem as
  required. The online/offline method is chosen automatically. GPT and MBR
  primary partitions; superfloppy (whole-disk FS) omits :N — grow only, never
  shrinks.

  Examples:
    diskedit resize /dev/sdb1 +2G
    diskedit resize disk.img 10G --allow-move --yes

  SIZE:
    10G       set size to 10 GiB (units k/m/g/t, 1024 base; plain bytes
              when no unit)
    +2G       grow by 2 GiB
    -500M     shrink by 500 MiB
    +10%      grow by 10% of current size (rounded down to 1MiB)
    -10%      shrink by 10%
    grow      grow into the contiguous free space to the right
    Sizes may carry a fraction: 1.5G = 1610612736 bytes, +1.5% likewise. A
    size that does not land on a sector boundary is truncated, and the
    truncation is reported

  Options:
    --allow-move    permit relocating the partitions in the way: the plan is
                    printed, and --yes is needed to carry it out. Without it a
                    grow or size that needs relocation is refused (10)
    --chunk-size N  recovery granularity in MiB (default 4): a crash redoes
                    at most 256 MiB of already copied data, or one chunk if
                    the chunk is larger. Re-running an interrupted job must
                    use the same value
    --grow-lv       also grow the LV: the one named by --lv, else the only LV
    --lv NAME       which LV to grow (implies --grow-lv)
    --no-fs         skip the filesystem step of the grown partition; required
                    when the filesystem inside cannot be grown here. Limits
                    are under Constraints
    --sector-size N logical sector size override for raw images
                    (power of two, 512..=65536)
    --yes           acknowledge the action; the top-level help lists which
                    commands ask

  Constraints:
    --no-fs   refused on an LVM PV (the PV chain has no filesystem step of
              its own) and together with a shrink (shrink the FS first). A
              swap partition relocated as a blocker is rebuilt regardless:
              that step belongs to the relocation, not to the filesystem
    PV        shrinking is refused — use the lvreduce/pvresize chain
    MBR       tables carry no checkpoint: an interrupted MBR grow cannot be
              resumed. Inspect the table and FS size before re-running; a
              re-run reads the on-disk geometry, it does not replay the old
              request
    online    writes go through the tools (sfdisk / the FS's own resize),
              not this tool's journal: no checkpoint to resume, and `undo`
              cannot roll it back

  Related commands: this is the single entry point — it plans and applies in
  one run. To inspect the plan first, use `plan` then `apply`; `resize-part`
  and `move` are the low-level single-partition commands, and they resume an
  interrupted job of their own. A resize interrupted after its partition step
  (the LVM finish chain still pending) is resumed by re-running `resize`.

  --start/--end are rejected here: resizing does not relocate partitions —
  use `move` or `resize-part --start/--end`."#;

const CONFIRM_RELOCATION: &str = "this resizes by relocating the partitions listed above — review and re-run with --yes";

/// resize 的一键入口。SIZE：`10G`=绝对值、`+2G`/`-500M`=增量、`grow`=吃满右侧可用区
/// （挡路分区须 --allow-move，搬移计划须 --yes 确认）。LVM PV 扩容自动 pvresize，
/// --grow-lv 再把新增传给目标 LV 并扩 FS；PV 缩容一律拒绝（走 lvreduce/pvresize 链）
pub(crate) fn cmd_resize(a: &Args) -> crate::outcome::Outcome {
    // SIZE 只有一种给法：位置参数。原始字符串随锁前/锁下两次解析共用
    let size = a.pos.get(1).map(String::as_str);
    // resize 只改大小、不移动。--start/--end 属 move/resize-part 的语义（挪位），
    // 静默忽略会让用户误以为分区被移动过 —— 显式拒绝。起点只有一个字段，
    // 数值与 `--start end` 都在 `a.start` 里，一处判据即可覆盖
    if a.start.is_some() || a.end.is_some() {
        bail_fail(Fail::refused("resize does not relocate partitions — use `move` or `resize-part --start/--end`".to_string()));
    }
    // 载体必须是整盘：分区节点首扇区里的 FAT/NTFS 引导字节带 0x55AA 时会被 parse_mbr_raw
    // 认成一张 MBR，MBR 分支的写路径会改写该分区自己的 LBA0。判据与 new/add/delete/set 同源
    refuse_partition_node_target("resize", &a.target);
    let src = open_target_ro(a).unwrap_or_else(|f| bail_fail(f));
    match table::table_label(&src) {
        Ok(table::TableLabel::Gpt) => {}
        Ok(table::TableLabel::Mbr) => {
            let Some(pref) = a.part else {
                bail_fail(Fail::refused("`resize` needs :N (the partition to resize)".to_string()));
            };
            return cmd_resize_msdos(a, pref, size, &src);
        }
        // superfloppy：无分区表，FS 即整盘，无表可写——纯 FS grow
        Ok(table::TableLabel::None) => return cmd_resize_superfloppy(a, size),
        Ok(other) => bail_fail(Fail::refused(format!("resize requires a GPT or MBR target (label: {other})"))),
        Err(e) => bail_fail(Fail::infra(format!("parse failed: {e}"))),
    }
    let Some(pref) = a.part else {
        bail_fail(Fail::refused("`resize` needs :N (the partition to resize)".to_string()));
    };
    // 可操作几何（构造点即拒绝条目重叠）：entries / ss / 修复后的 last_usable 全部取自它，
    // 命令层不自己算一次有效上界
    let (g, _repair) = crate::gpt_policy::require_gpt_geometry(&src, "resize").unwrap_or_else(|f| bail_fail(f));
    // 只读快照
    let part = crate::gpt_policy::resolve_part_in(crate::gpt_policy::TableEntries::Gpt(g.entries()), pref)
        .unwrap_or_else(|f| bail_fail(f));
    let e = crate::gpt_policy::live_entry(&g, part).unwrap_or_else(|f| bail_fail(f));
    let (start, end, ss) = (e.starting_lba, e.ending_lba, g.sector_size());
    let cur_bytes = lba_range_bytes(start, end, ss);
    let fstype = fsid::identify(&src, lba_bytes(start, ss), cur_bytes)
        .unwrap_or_else(|e| bail_fail(Fail::infra(format!("identify failed: {e}"))));
    let is_pv = fstype == Some(fsid::FsKind::Lvm2Pv);

    // SIZE → 绝对目标字节数 / grow 标记（锁前快照，仅服务参数早失败与在线路径的选择）
    let (target, grow_to_end) = resolve_size_request(size, cur_bytes).unwrap_or_else(|f| bail_fail(f));
    let shrinking = target.is_some_and(|t| t < cur_bytes);

    check_pv_intent(part, fstype, is_pv, shrinking, a.grow_lv, a.no_fs).unwrap_or_else(|f| bail_fail(f));

    // PV 链要扩哪个 LV 是只读判定：--lv 写错、PV 上没有 LV、LV 不唯一都必须在此之前
    // 定下来，写表之后再发现就只剩"分区已扩、PV 链尚未做"的现场。在线与离线两条路径
    // 共用这一次预检（收尾链在提交之后用锁下的分区节点再查一次）
    // 选定结果随行到执行半程，由它导出 checkpoint 里的 LVM 收尾事实
    #[cfg(target_os = "linux")]
    let mut lv_sel: Option<movepart::LvmSelection> = None;
    #[cfg(not(target_os = "linux"))]
    let lv_sel: Option<movepart::LvmSelection> = None;
    #[cfg(target_os = "linux")]
    if is_pv && a.grow_lv {
        let start_bytes = start
            .checked_mul(ss)
            .unwrap_or_else(|| bail_fail(Fail::infra("partition start overflows byte offset")));
        lv_sel = Some(
            preflight_lv_selection(&src, part, src.is_block, start_bytes, a.lv.as_deref())
                .unwrap_or_else(|e| bail_fail(lvm_preflight_fail(e))),
        );
    }

    // 目标长度不在锁前算：`+N` 锚定当前尺寸、`grow` 锚定右侧空闲，都由 gpt_decider 在
    // 在线自己的锁下、按重解析的盘上几何重新解析（见其注释）
    #[cfg(target_os = "linux")]
    if let Some(o) = resize_online(a, &src, part, is_pv, gpt_decider(size, part, is_pv)) {
        return o;
    }

    // 离线路径
    let is_block = src.is_block;
    // 一次打开完成分类：目标上的未收尾作业在**锁下**判定（右侧"已空"可能正是搬了一半
    // 的结果），而不是先只读判一次再开第二次——判据与开目标之间不许留窗口。
    // 两族作业的进入语义不同：`reloc` 是本分区的搬移作业（"右侧已空"可能是搬移中间态，
    // 且挡路分区已是用户确认过的那份作业）；`resuming` 是任一未收尾作业（分区层可能已
    // 提交，只等 FS/PV 收尾），FS 无变化的短路不能把后者当成"无事可做"
    let (mut src, resumed) =
        open_target_resize_resumable(a, pref).unwrap_or_else(|f| bail_fail(f));
    let reloc = resumed == crate::transaction::ResumeKind::Relocation;
    let resuming = resumed != crate::transaction::ResumeKind::Fresh;
    // 锁下重取权威几何：写路径的每个 LBA（start/end/free/last_usable）都来自它。
    // 锁前那份只服务参数早失败与在线路径的事实快照；只读阶段与取得独占权之间
    // 盘可以被别人改写，用锁前的值驱动写分支就是把过期决定写进盘
    let (g, repair) = crate::gpt_policy::require_gpt_geometry(&src, "resize").unwrap_or_else(|f| bail_fail(f));
    // 锁下快照
    let part = crate::gpt_policy::resolve_part_in(crate::gpt_policy::TableEntries::Gpt(g.entries()), pref)
        .unwrap_or_else(|f| bail_fail(f));
    let e = crate::gpt_policy::live_entry(&g, part).unwrap_or_else(|f| bail_fail(f));
    let (start, end, ss) = (e.starting_lba, e.ending_lba, g.sector_size());
    let last_usable = g.last_usable_lba();
    // SIZE 锚点与 PV 判据按锁下的新几何重算：+N/+N% 锚定"当前尺寸"，窗口内分区被
    // 改写过，锁前锚点就已作废——拿旧锚点的绝对值对照新几何会把"扩"判成"缩"
    // （先缩 FS！），方向与请求相反。grow 标记是请求的形状（与盘无关），锁前锁后同值
    let cur_bytes = lba_range_bytes(start, end, ss);
    let (target, _) = resolve_size_request(size, cur_bytes).unwrap_or_else(|f| bail_fail(f));
    let shrinking = target.is_some_and(|t| t < cur_bytes);
    // FS 类型与 PV 判据同样按锁下现状重取（与几何同理）：识别与取锁之间分区可被
    // 重新格式化（mkfs 不守本工具的锁），过期的 is_pv 会让 check_pv_intent 对着
    // 已不是 PV 的分区说 PV 的话
    let fstype = fsid::identify(&src, lba_bytes(start, ss), cur_bytes)
        .unwrap_or_else(|e| bail_fail(Fail::infra(format!("identify failed: {e}"))));
    let is_pv = fstype == Some(fsid::FsKind::Lvm2Pv);
    check_pv_intent(part, fstype, is_pv, shrinking, a.grow_lv, a.no_fs).unwrap_or_else(|f| bail_fail(f));
    // PV 链的工具必须在写表之前齐备（pvresize/pvs 及 grow-lv 的 lvs/vgs/lvextend；
    // 镜像目标另加 losetup——它的分区链要临时映射）：它们在表提交之后才启动，
    // 缺工具那时才发现就只剩 PARTIAL 加手工
    #[cfg(target_os = "linux")]
    if is_pv {
        preflight_lvm_tools(a.grow_lv, !is_block).unwrap_or_else(|e| bail_fail(Fail::from(e)));
        // 锁下的 PV 判据可能已与锁前不同（mkfs 会换掉分区内容）：要扩的 LV 按锁下的
        // 分区节点与起始字节再确认一次，仍在写表之前
        if a.grow_lv {
            let start_bytes = start
                .checked_mul(ss)
                .unwrap_or_else(|| bail_fail(Fail::infra("partition start overflows byte offset")));
            lv_sel = Some(
                preflight_lv_selection(&src, part, is_block, start_bytes, a.lv.as_deref())
                    .unwrap_or_else(|e| bail_fail(lvm_preflight_fail(e))),
            );
        }
    } else {
        // 锁下已不是 PV：锁前那份选择对本次写盘没有意义（mkfs 换掉了分区内容）
        lv_sel = None;
    }
    // 占用闸在 movepart 的 prepare 层（本分支的每条路径都经 prepare_resize/prepare_apply），
    // 不在命令层重复判一次
    if grow_to_end {
        let free = free_right_gpt(&g, part);
        // 右侧有空闲且没有未收尾的搬移作业 → 纯扩容。若作业未收尾，则"右侧已空"很可能
        // 正是搬了一半的结果，走普通 resize_part 会跳过剩余搬移与 swap 重建等收尾
        if free > 0 && !reloc {
            // free 的上界由 free_right_gpt 的构造保证（≤ last_usable - end）；checked
            // 把这层非局部依赖显式化，回绕值进不了计划
            let Some(new_end) = end.checked_add(free) else {
                bail_fail(Fail::infra("free-space arithmetic overflow (geometry inconsistent)"));
            };
            let (chunk, mut logger) = chunk_logger(a, &src, g.disk_guid());
            let o = settle_layout(movepart::resize_part(&mut src, &g, repair, part, start, new_end, chunk, a.no_fs, is_pv, lv_sel.clone(), &mut |m| logger.log(m)), &src);
            return finish_resize(a, o, Some(&mut src), part, is_pv, is_block, cur_bytes);
        }
        // 已顶到 last_usable 的分区不是"被挡"，是无可再扩：occupied 文案会把用户引进
        // --allow-move 的空搬移（空 moves 的 plan 什么都没做却报成功）。
        // PV 例外：分区层没有可扩的扇区，但 pvresize/lvextend 仍是本次操作的后置条件
        // （上一次运行可能正停在提交之后），交给收尾链续做
        if !reloc && end == last_usable {
            if is_pv {
                let (chunk, mut logger) = chunk_logger(a, &src, g.disk_guid());
                let o = settle_layout(movepart::resize_part(&mut src, &g, repair, part, start, end, chunk, a.no_fs, true, lv_sel.clone(), &mut |m| logger.log(m)), &src);
                return finish_resize(a, o, Some(&mut src), part, is_pv, is_block, cur_bytes);
            }
            bail_fail(Fail::refused(format!(
                "partition already ends at last_usable_lba {last_usable} — nothing to grow"
            )));
        }
        // 右侧被挡：自动搬移挡路分区（plan 打印 → --allow-move 放行 → --yes 确认）。
        // --allow-move 只对**新**搬移授权：续跑是"接着做用户已确认过的那件事"，
        // 被卡的挡路分区正是那份作业的一部分——再要一次授权只会把承诺"重跑原命令
        // 即续跑"变成谎话。要求豁免的两态由此分开：未收尾作业放行，真空间不足才拒绝
        if !a.allow_move && !reloc {
            bail_fail(Fail::refused("right side is occupied — pass --allow-move to relocate the blocking partitions (plan will be printed; --yes confirms)".to_string()));
        }
        let plan = match movepart::make_plan_resuming(&src, &g, repair, part) {
            Ok(p) => p,
            Err(f) => bail_fail(f),
        };
        // 续跑的收尾仍须 --yes：盘上状态已与上次请求时不同，写盘前再确认一次；
        // --yes 一并覆盖"未确认的续跑"与"新的搬移"两种进入方式
        crate::cmd::plan::print_plan(&plan, start).unwrap_or_else(|e| bail_fail(Fail::infra(format!("plan failed: {e}"))));
        if !a.yes {
            bail_fail(Fail::refused(CONFIRM_RELOCATION));
        }
        let (chunk, mut logger) = chunk_logger(a, &src, g.disk_guid());
        let o = settle_layout(movepart::apply(&mut src, &g, &plan, chunk, a.no_fs, is_pv, lv_sel.clone(), &mut |m| logger.log(m)), &src);
        finish_resize(a, o, Some(&mut src), part, is_pv, is_block, cur_bytes)
    } else {
        // SIZE：字节 → 扇区（下取整）；扩须右侧空闲足够，缩由 resize_part 内部 FS 先缩 + 守卫链
        let Some(bytes) = target else {
            bail_fail(Fail::refused("`resize` needs a size: diskedit resize <TARGET>[:N] <SIZE> (see diskedit help resize)".to_string()));
        };
        if bytes < ss {
            bail_fail(Fail::refused(format!("size {bytes} < one sector ({ss})")));
        }
        // SIZE 是外部输入：换算与相加全程 checked，回绕会把越过 last_usable 的荒谬值送进比较
        let Some(new_end) = start.checked_add(bytes / ss).and_then(|e| e.checked_sub(1)) else {
            bail_fail(Fail::refused(format!("size {bytes} overflows the LBA range")));
        };
        if new_end > last_usable {
            bail_fail(Fail::refused(format!("size {bytes} exceeds usable range (partition would end past last_usable_lba {last_usable})")));
        }
        // 请求已通过越界判定，note 不会再跟在一条拒绝后面
        crate::support::note_sector_truncation("resizing to", bytes, ss);
        // 请求的形状三选一：缩容与"尺寸未变"都算不出正位移，但只有后者可能是原样
        // 重跑当初的扩容命令。搬移量本身只在扩的时候有定义
        let intent = if new_end > end {
            movepart::ShiftIntent::Grow(new_end - end)
        } else if new_end == end {
            movepart::ShiftIntent::NoOp
        } else {
            movepart::ShiftIntent::Shrink
        };
        // 请求的终点就是现状：表与 FS 都不动，不进入 resize_part。与 MBR 路径同一判据——
        // 请求"什么都不改"时出口是 0 且文案明确，而不是先打印 resized。
        // PV 例外：分区层无变化不意味着本次操作没有后置条件，收尾链（pvresize/lvextend）
        // 仍要跑，故落到下面的直连路径
        if !resuming && !is_pv && matches!(intent, movepart::ShiftIntent::NoOp) {
            println!("partition {part} is already {cur_bytes} bytes — nothing to do");
            return crate::outcome::Outcome::applied_with(Vec::new());
        }
        // 未收尾的搬移作业 ⇒ 必须走 resume 路径（即使几何上 free_right 已足够——swap 等
        // 收尾步骤可能尚未执行，普通扩容会跳过它们）
        if reloc || matches!(intent, movepart::ShiftIntent::Grow(s) if s > free_right_gpt(&g, part)) {
            // --allow-move 只对**新**搬移授权，续跑放行——两态判据与 grow 分支
            // 同一份理由（见上），真空间不足才拒绝；确认流也同一份：
            // plan 打印 → --yes 确认
            if !a.allow_move && !reloc {
                bail_fail(Fail::refused("not enough contiguous free space to the right — pass --allow-move to relocate the blocking partitions (plan will be printed; --yes confirms)".to_string()));
            }
            let plan = match movepart::make_plan_shift_resuming(&src, &g, repair, part, intent) {
                Ok(p) => p,
                Err(f) => bail_fail(f),
            };
            crate::cmd::plan::print_plan(&plan, start).unwrap_or_else(|e| bail_fail(Fail::infra(format!("plan failed: {e}"))));
            if !a.yes {
                bail_fail(Fail::refused(CONFIRM_RELOCATION));
            }
            let (chunk, mut logger) = chunk_logger(a, &src, g.disk_guid());
            let o = settle_layout(movepart::apply(&mut src, &g, &plan, chunk, a.no_fs, is_pv, lv_sel.clone(), &mut |m| logger.log(m)), &src);
            return finish_resize(a, o, Some(&mut src), part, is_pv, is_block, cur_bytes);
        }
        let (chunk, mut logger) = chunk_logger(a, &src, g.disk_guid());
        let o = settle_layout(movepart::resize_part(&mut src, &g, repair, part, start, new_end, chunk, a.no_fs, is_pv, lv_sel.clone(), &mut |m| logger.log(m)), &src);
        finish_resize(a, o, Some(&mut src), part, is_pv, is_block, cur_bytes)
    }
}
