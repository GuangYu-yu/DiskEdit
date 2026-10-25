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
use crate::args::{parse_size_delta, Args};
use crate::dev::{FileSource, PartSelector};
use crate::{dev, fsid, fsops, movepart, table};
#[cfg(target_os = "linux")]
use crate::lvm::LvmError;

pub(crate) const HELP: &str = r#"diskedit resize <TARGET>:N <SIZE> [OPTIONS]

  Resize partition and its filesystem. Online/offline method is chosen
  automatically. GPT and MBR primary partitions; superfloppy (whole-disk FS)
  omits :N — grow only, never shrinks.

  SIZE:
    10G       set size to 10 GiB (units b/k/m/g/t, 1024 base)
    +2G       grow by 2 GiB
    -500M     shrink by 500 MiB
    +10%      grow by 10% of current size (rounded down to 1MiB)
    -10%      shrink by 10%
    grow      grow into the contiguous free space to the right

  Options:
    --allow-move    allow moving other partitions (plan requires --yes)
    --size BYTES    absolute target size (same units as SIZE)
    --grow-to-end   grow into the contiguous free space to the right
    --chunk-size N  relocation copy granularity in MiB (default 4)
    --sector-size N logical sector size override for raw images
                    (power of two, 512..=65536)
    --grow-lv       also grow the associated LV (--lv NAME, or the only LV)
    --yes           skip confirmation

  --start/--end are rejected here: resizing does not relocate partitions —
  use `move` or `resize-part --start/--end`.

  Automatically:
    detects partition / filesystem / LVM PV, chooses online or offline,
    resizes partition -> PV -> LV -> filesystem as required; long operations
    are checkpointed and resumed by re-running the same command (an
    interrupted single-partition resize job is resumed by re-running
    `resize-part` or `move`, not `resize`).

  Notes: PV shrink is refused (use the lvreduce/pvresize chain). --no-fs skips
  the filesystem step of the grown partition, so it cannot shrink: the FS has
  to be shrunk first. It is also required whenever the filesystem inside
  cannot be grown (unidentified, or not wired to a tool) — otherwise the
  request is refused (10). On an LVM PV --no-fs is refused: the PV chain has
  no filesystem step of its own. An online resize writes through the tools
  (sfdisk / the filesystem's own resize) instead of this tool's journal, so
  `undo` cannot roll it back — it also leaves no checkpoint to resume."#;

const CONFIRM_RELOCATION: &str = "this resizes by relocating the partitions listed above — review and re-run with --yes";

/// resize 的一键入口。SIZE：`10G`=绝对值、`+2G`/`-500M`=增量、`grow`=吃满右侧可用区
/// （挡路分区须 --allow-move，搬移计划须 --yes 确认）。LVM PV 扩容自动 pvresize，
/// --grow-lv 再把新增传给目标 LV 并扩 FS；PV 缩容一律拒绝（走 lvreduce/pvresize 链）
pub(crate) fn cmd_resize(a: &Args) -> crate::outcome::Outcome {
    // 请求的形状在入口定型一次，随锁前/锁下两次解析共用：位置参数 SIZE / --size / --grow-to-end
    let req = SizeRequest { pos: a.pos.get(1).cloned(), abs: a.size, grow_to_end: a.grow_to_end };
    if req.pos.is_some() && (a.size.is_some() || a.grow_to_end) {
        bail_fail(Fail::refused("SIZE and --size/--grow-to-end are mutually exclusive".to_string()));
    }
    // --size 与 --grow-to-end 是两种给终点的方式：都给时 grow_to_end 分支根本不读
    // size 的值，静默丢参数比报错更害人（用户以为扩到了 10G）
    if a.size.is_some() && a.grow_to_end {
        bail_fail(Fail::refused("--size and --grow-to-end are mutually exclusive".to_string()));
    }
    if a.lv.is_some() && !a.grow_lv {
        bail_fail(Fail::refused("--lv only works together with --grow-lv".to_string()));
    }
    // resize 只改大小、不移动。--start/--end 属 move/resize-part 的语义（挪位），
    // 静默忽略会让用户误以为分区被移动过 —— 显式拒绝。判据必须覆盖三个入口：
    // 两者各自单给、以及 `--start end` 那种尾部打包的写法
    if a.start.is_some() || a.end.is_some() || a.start_end {
        bail_fail(Fail::refused("resize does not relocate partitions — use `move` or `resize-part --start/--end`".to_string()));
    }
    // 载体必须是整盘：分区节点首扇区里的 FAT/NTFS 引导字节带 0x55AA 时会被 parse_mbr_raw
    // 认成一张 MBR，MBR 分支的写路径会改写该分区自己的 LBA0。判据与 new/add/del/set 同源
    refuse_partition_node_target("resize", &a.target);
    let src = open_target_ro(a).unwrap_or_else(|f| bail_fail(f));
    match table::table_label(&src) {
        Ok(table::TableLabel::Gpt) => {}
        Ok(table::TableLabel::Mbr) => {
            let Some(pref) = a.part else {
                bail_fail(Fail::refused("`resize` needs :N (the partition to resize)".to_string()));
            };
            return cmd_resize_msdos(a, pref, &req, &src);
        }
        // superfloppy：无分区表，FS 即整盘，无表可写——纯 FS grow
        Ok(table::TableLabel::None) => return cmd_resize_superfloppy(a, &req),
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
    let (target, grow_to_end) = resolve_size_request(&req, cur_bytes).unwrap_or_else(|f| bail_fail(f));
    let shrinking = target.is_some_and(|t| t < cur_bytes);

    check_pv_intent(part, fstype, is_pv, shrinking, a.grow_lv, a.no_fs).unwrap_or_else(|f| bail_fail(f));

    // 块设备在线路径（表类型无关，随表类型变的只有右侧空闲数）；镜像或未挂载的非 PV 落到离线路径。
    // 目标长度不在锁前算：`+N` 锚定当前尺寸、`grow` 锚定右侧空闲，都由 gpt_decider 在
    // 在线自己的锁下、按重解析的盘上几何重新解析（见其注释）
    #[cfg(target_os = "linux")]
    if let Some(o) = resize_online(a, &src, part, is_pv, gpt_decider(&req, part, is_pv)) {
        return o;
    }

    // 离线路径
    let is_block = src.is_block;
    // 一次打开完成分类：有没有自己分区的未收尾搬移在**锁下**判定（右侧"已空"可能正是
    // 搬了一半的结果），而不是先只读判一次再开第二次——判据与开目标之间不许留窗口。
    // 返回的 resumed 决定 grow 分支：续跑时"右侧已空"可能是搬移的中间态
    let (mut src, resuming) =
        open_target_resumable(a, pref).unwrap_or_else(|f| bail_fail(f));
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
    let (target, _) = resolve_size_request(&req, cur_bytes).unwrap_or_else(|f| bail_fail(f));
    let shrinking = target.is_some_and(|t| t < cur_bytes);
    // FS 类型与 PV 判据同样按锁下现状重取（与几何同理）：识别与取锁之间分区可被
    // 重新格式化（mkfs 不守本工具的锁），过期的 is_pv 会让 check_pv_intent 对着
    // 已不是 PV 的分区说 PV 的话
    let fstype = fsid::identify(&src, lba_bytes(start, ss), cur_bytes)
        .unwrap_or_else(|e| bail_fail(Fail::infra(format!("identify failed: {e}"))));
    let is_pv = fstype == Some(fsid::FsKind::Lvm2Pv);
    check_pv_intent(part, fstype, is_pv, shrinking, a.grow_lv, a.no_fs).unwrap_or_else(|f| bail_fail(f));
    // 占用闸在 movepart 的 prepare 层（本分支的每条路径都经 prepare_resize/prepare_apply），
    // 不在命令层重复判一次
    if grow_to_end {
        let free = free_right_gpt(&g, part);
        // 右侧有空闲且没有未收尾的搬移作业 → 纯扩容。若作业未收尾，则"右侧已空"很可能
        // 正是搬了一半的结果，走普通 resize_part 会跳过剩余搬移与 swap 重建等收尾
        if free > 0 && !resuming {
            // free 的上界由 free_right_gpt 的构造保证（≤ last_usable - end）；checked
            // 把这层非局部依赖显式化，回绕值进不了计划
            let Some(new_end) = end.checked_add(free) else {
                bail_fail(Fail::infra("free-space arithmetic overflow (geometry inconsistent)"));
            };
            let (chunk, mut logger) = chunk_logger(a, &src, g.disk_guid());
            let o = settle_layout(movepart::resize_part(&mut src, &g, repair, part, start, new_end, chunk, a.no_fs, &mut |m| logger.log(m)), &src);
            return finish_resize(a, o, Some(&mut src), part, is_pv, is_block, cur_bytes);
        }
        // 已顶到 last_usable 的分区不是"被挡"，是无可再扩：occupied 文案会把用户引进
        // --allow-move 的空搬移（空 moves 的 plan 什么都没做却报成功）
        if !resuming && end == last_usable {
            bail_fail(Fail::refused(format!(
                "partition already ends at last_usable_lba {last_usable} — nothing to grow"
            )));
        }
        // 右侧被挡：自动搬移挡路分区（plan 打印 → --allow-move 放行 → --yes 确认）。
        // --allow-move 只对**新**搬移授权：续跑是"接着做用户已确认过的那件事"，
        // 被卡的挡路分区正是那份作业的一部分——再要一次授权只会把承诺"重跑原命令
        // 即续跑"变成谎话。要求豁免的两态由此分开：未收尾作业放行，真空间不足才拒绝
        if !a.allow_move && !resuming {
            bail_fail(Fail::refused("right side is occupied — pass --allow-move to relocate the blocking partitions (plan will be printed; --yes confirms)".to_string()));
        }
        let plan = match movepart::make_plan_resuming(&mut src, &g, repair, part) {
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
        let o = settle_layout(movepart::apply(&mut src, &g, &plan, chunk, a.no_fs, &mut |m| logger.log(m)), &src);
        finish_resize(a, o, Some(&mut src), part, is_pv, is_block, cur_bytes)
    } else {
        // SIZE：字节 → 扇区（下取整）；扩须右侧空闲足够，缩由 resize_part 内部 FS 先缩 + 守卫链
        let Some(bytes) = target else { crate::args::usage() };
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
        // 未收尾的搬移作业 ⇒ 必须走 resume 路径（即使几何上 free_right 已足够——swap 等
        // 收尾步骤可能尚未执行，普通扩容会跳过它们）
        if resuming || matches!(intent, movepart::ShiftIntent::Grow(s) if s > free_right_gpt(&g, part)) {
            // --allow-move 只对**新**搬移授权，续跑放行——两态判据与 grow 分支
            // 同一份理由（见上），真空间不足才拒绝；确认流也同一份：
            // plan 打印 → --yes 确认
            if !a.allow_move && !resuming {
                bail_fail(Fail::refused("not enough contiguous free space to the right — pass --allow-move to relocate the blocking partitions (plan will be printed; --yes confirms)".to_string()));
            }
            let plan = match movepart::make_plan_shift_resuming(&mut src, &g, repair, part, intent) {
                Ok(p) => p,
                Err(f) => bail_fail(f),
            };
            crate::cmd::plan::print_plan(&plan, start).unwrap_or_else(|e| bail_fail(Fail::infra(format!("plan failed: {e}"))));
            if !a.yes {
                bail_fail(Fail::refused(CONFIRM_RELOCATION));
            }
            let (chunk, mut logger) = chunk_logger(a, &src, g.disk_guid());
            let o = settle_layout(movepart::apply(&mut src, &g, &plan, chunk, a.no_fs, &mut |m| logger.log(m)), &src);
            return finish_resize(a, o, Some(&mut src), part, is_pv, is_block, cur_bytes);
        }
        let (chunk, mut logger) = chunk_logger(a, &src, g.disk_guid());
        let o = settle_layout(movepart::resize_part(&mut src, &g, repair, part, start, new_end, chunk, a.no_fs, &mut |m| logger.log(m)), &src);
        finish_resize(a, o, Some(&mut src), part, is_pv, is_block, cur_bytes)
    }
}
