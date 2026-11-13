//! resize：分区 + 文件系统的一键扩容（GPT / MBR / superfloppy，在线与离线自动选择）。

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

use crate::cli::ResizeRequest;
use crate::support::*;
use crate::dev::{FileSource, PartSelector, WriteSource};
use crate::{dev, fsid, fsops, movepart, table};
#[cfg(target_os = "linux")]
use crate::lvm::LvmError;

/// 一次搬移事务 = 事前判定（只读）+ 执行（写盘）。
///
/// 判定的返回类型里只有 `Refused` / `NoMutation`（此刻确定未写盘）；执行半程交给
/// [`crate::movepart::execute_apply`]（内部走 [`crate::execution::execute_plan`]），
/// 它的错误分类按"首写之前 / 之后"给出；本函数的出口按本次写事实归责，把执行器
/// 视野之外的目标写入（repair）算进结论。几何由调用方解析（一条命令解析一次，
/// 往下传），本函数与执行半程都不读表
#[allow(clippy::too_many_arguments)]
pub(crate) fn apply_move(
    src: &mut WriteSource,
    g: &crate::geometry::ValidatedGeometry,
    plan: &mut crate::movepart::Plan,
    chunk_len: u64,
    no_fs: bool,
    defer_ckpt: bool,
    lvm: Option<crate::movepart::LvmSelection>,
    log: &mut dyn FnMut(&str),
) -> RunOutcome {
    // 出口归责：执行器的分类只看计划内的写入，repair 的位置不在它的视野里。
    // 函数体内各早退原样返回，由这一处统一过判定点
    apply_move_unreconciled(src, g, plan, chunk_len, (no_fs, defer_ckpt), lvm, log)
        .settled()
}

/// [`apply_move`] 的主体：各出口只陈述本步结论，归责留给外层。
/// `switches` = (`no_fs`, `defer_ckpt`)
fn apply_move_unreconciled(
    src: &mut WriteSource,
    g: &crate::geometry::ValidatedGeometry,
    plan: &mut crate::movepart::Plan,
    chunk_len: u64,
    switches: (bool, bool),
    lvm: Option<crate::movepart::LvmSelection>,
    log: &mut dyn FnMut(&str),
) -> RunOutcome {
    let (no_fs, defer_ckpt) = switches;
    // 事务入口的注入点：此刻 abort 必须与"从未运行过"不可区分
    movepart::fault_before_any_write();
    // 记录里的搬移状态是续跑的唯一依据；读两份并持有期结束后再进入判定
    let (existing_relocation, existing_lvm) = match src.record_journal() {
        Some(j) => (j.relocation().cloned(), j.lvm_post()),
        None => (None, None),
    };
    let mut d = match movepart::prepare_apply(
        src,
        g,
        plan,
        chunk_len,
        no_fs,
        defer_ckpt,
        lvm,
        existing_relocation.as_ref(),
        existing_lvm,
        log,
    ) {
        Ok(d) => d,
        Err(f) => return f,
    };
    // 修复写盘之前先把现场落盘：repair 已是目标写入，它与记录绑定之间不留无记录的
    // 窗口——那里 abort 会留下"盘已改、无从回滚"的现场，违背"写盘后 abort 必须留下
    // 可续传现场"的边界配对。执行半程的同值绑定是幂等的
    if let Err(e) = movepart::bind_relocation(src, &d.relocation, d.lvm.as_ref()) {
        return RunOutcome::no_mutation(e.bind_failure());
    }
    movepart::fault_after_relocation_bound();
    // 修复虽是写盘，但先于任何目标写入执行；放在 RunOutcome 域是为了保住分类：
    // 其读表阶段尚未写一个字节（NoMutation），一旦进入 io 域就只剩 Partial 一条路
    if let Some(what) = plan.repair.describe() {
        if let Err(f) = crate::gpt_policy::apply_repair(src, &plan.repair) {
            return f;
        }
        log(&format!("[repair] {what}"));
        // 修复是目标写入：此后装配步骤的表内容、首写前比对与扩容区间都按修复后的表读。
        // 判定期那份读取在修复前，不能继续当基线。`old_range` 描述的是提交前区间，
        // 修复不改条目位置，保持原值
        d.g = match crate::gpt_policy::resolve_geometry(src) {
            Ok(Some((fresh, _))) => fresh,
            Ok(None) => {
                return RunOutcome::refused("the GPT disappeared after the repair — refusing");
            }
            Err(f) => return f,
        };
    }
    movepart::fault_after_repair();
    // 越界之后只剩两条出口：写入完成，或"盘可能已改变"的执行失败
    let mut post: Vec<crate::outcome::Pending> = Vec::new();
    match movepart::execute_apply(src, plan, d, log, &mut post) {
        Ok(()) => RunOutcome::applied_with(post),
        // 分类由执行器给：首写之前的拒绝仍是 3/4，首写之后才是 5。失败自带的待办与
        // 本进程已攒下的待办合并，一个都不丢
        Err(RunOutcome::Partial { cause, pending, origin }) => RunOutcome::Partial {
            cause,
            pending: pending.into_iter().chain(post).collect(),
            origin,
        },
        // 待办只由写盘之后的步骤产生：非空即已越过首写，"确定未写盘"的变体在这里不适用
        Err(other) if !post.is_empty() => RunOutcome::partial_with(other.to_string(), post),
        Err(other) => other,
    }
}

pub(crate) const DETAILS: &str = r#"  Resize partition and its filesystem, partition -> PV -> LV -> filesystem as
  required. The online/offline method is chosen automatically. GPT and MBR
  primary partitions; superfloppy (whole-disk FS) omits :N — grow only, never
  shrinks.

  Examples:
    diskedit resize /dev/sdb1 +2G --yes
    diskedit resize disk.img 10G --allow-move --chunk 4 --yes

  SIZE:
    10G       set size to 10 GiB (units k/m/g/t, 1024 base; plain bytes
              when no unit)
    +2G       grow by 2 GiB
    +10%      grow by 10% of current size (rounded down to 1MiB)
    grow      grow into the contiguous free space to the right
    Sizes may carry a fraction: 1.5G = 1610612736 bytes, +1.5% likewise. A
    size that does not land on a sector boundary is truncated, and the
    truncation is reported

  Constraints:
    shrink    refused: this command only grows. Shrink with `resize-part`
              --end: it shrinks the filesystem first (offline), then the
              partition, and an interrupted shrink resumes from its record
    --no-fs   refused on an LVM PV (the PV chain has no filesystem step of
              its own). A swap partition relocated as a blocker is rebuilt
              regardless: that step belongs to the relocation, not to the
              filesystem
    MBR       uses the same recovery record as GPT: an interrupted grow is
              resumed by re-running the same command; `undo` rolls the
              recorded table write back instead
    record    a relocation record written before requests were bound to
              their digest carries none: it cannot be tied to this request,
              so re-running `resize --allow-move` is refused. Inspect it with
              `inspect`, then release it with `undo`
    online    writes go through the tools (sfdisk / the FS's own resize),
              not this tool's record: nothing to resume, and `undo`
              cannot roll it back

  Related commands: this is the single entry point — it plans and applies in
  one run. To inspect the plan first, use `plan`; `resize-part` and `move` are
  the low-level single-partition commands, and they resume an interrupted job
  of their own. A resize interrupted after its partition step (the LVM finish
  chain still pending) is resumed by re-running `resize`."#;

/// `resize` 只扩。缩容由 `resize-part` 承担——先离线缩文件系统再收表，进度落在
/// 同一份记录里，中断后同请求重跑续做
fn refuse_shrink() -> RunOutcome {
    RunOutcome::refused(
        "`resize` only grows — shrink with `resize-part` (--end): it shrinks the filesystem \
         first (offline), then the partition, and an interrupted shrink resumes from its record"
            .to_string(),
    )
}

/// resize 的一键入口。SIZE：`10G`=绝对值、`+2G`=增量、`grow`=吃满右侧可用区
/// （挡路分区须 --allow-move，搬移计划须 --yes 确认）。只扩不缩：缩容拆成
/// `resizefs` + `resize-part`。LVM PV 扩容自动 pvresize，
/// --grow-lv 再把新增传给目标 LV 并扩 FS
pub(crate) fn cmd_resize(a: &ResizeRequest) -> crate::outcome::RunOutcome {
    // SIZE 已由命令行层解析成形状 + 数值，锁前/锁下两次锚定共用同一份（请求里的 a.size）
    // 载体必须是整盘：分区节点首扇区里的 FAT/NTFS 引导字节带 0x55AA 时会被 parse_mbr_raw
    // 认成一张 MBR，MBR 分支的写路径会改写该分区自己的 LBA0。判据与 new/add/delete/set 同源
    refuse_partition_node_target("resize", &a.target);
    let src = open_target_ro(a).unwrap_or_else(|f| bail_fail(f));
    match table::table_label(&src) {
        Ok(table::TableLabel::Gpt) => {}
        Ok(table::TableLabel::Mbr) => {
            let Some(pref) = a.part else {
                bail_fail(RunOutcome::refused("`resize` needs :N (the partition to resize)".to_string()));
            };
            return cmd_resize_msdos(a, pref, &src);
        }
        // superfloppy：无分区表，FS 即整盘，无表可写——纯 FS grow
        Ok(table::TableLabel::None) => return cmd_resize_superfloppy(a),
        Ok(other) => bail_fail(RunOutcome::refused(format!("resize requires a GPT or MBR target (label: {other})"))),
        Err(e) => bail_fail(crate::outcome::table_parse_failure(&e, format!("parse failed: {e}"))),
    }
    let Some(pref) = a.part else {
        bail_fail(RunOutcome::refused("`resize` needs :N (the partition to resize)".to_string()));
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
        .unwrap_or_else(|e| bail_fail(RunOutcome::no_mutation(format!("identify failed: {e}"))));
    let is_pv = fstype == fsid::ContentKind::Lvm2Pv;

    // SIZE → 绝对目标字节数 / grow 标记（锁前快照，仅服务参数早失败与在线路径的选择）
    let (target, grow_to_end) = resolve_size_request(a.size, cur_bytes).unwrap_or_else(|f| bail_fail(f));
    let shrinking = target.is_some_and(|t| t < cur_bytes);
    // 缩容不走 `resize`。判据落在在线/离线分岔之前，两条路径同一答案
    if shrinking {
        bail_fail(refuse_shrink());
    }

    check_pv_intent(part, Some(fstype), is_pv, shrinking, a.grow_lv, a.no_fs).unwrap_or_else(|f| bail_fail(f));

    // PV 链要扩哪个 LV 是只读判定：--lv 写错、PV 上没有 LV、LV 不唯一都必须在此之前
    // 定下来，写表之后再发现就只剩"分区已扩、PV 链尚未做"的现场。在线与离线两条路径
    // 共用这一次预检（收尾链在提交之后用锁下的分区节点再查一次）
    // 选定结果随行到执行半程，由它导出记录里的 LVM 收尾事实
    #[cfg(target_os = "linux")]
    let mut lv_sel: Option<movepart::LvmSelection> = None;
    #[cfg(not(target_os = "linux"))]
    let lv_sel: Option<movepart::LvmSelection> = None;
    #[cfg(target_os = "linux")]
    if is_pv && a.grow_lv {
        let start_bytes = start
            .checked_mul(ss)
            .unwrap_or_else(|| bail_fail(RunOutcome::no_mutation("partition start overflows byte offset")));
        lv_sel = Some(
            preflight_lv_selection(&src, part, src.is_block, start_bytes, a.lv.as_deref())
                .unwrap_or_else(|e| bail_fail(lvm_preflight_fail(e))),
        );
    }

    // 目标长度不在锁前算：`+N` 锚定当前尺寸、`grow` 锚定右侧空闲，都由 gpt_decider 在
    // 在线自己的锁下、按重解析的盘上几何重新解析（见其注释）
    #[cfg(target_os = "linux")]
    if let Some(o) = resize_online(a, &src, part, is_pv, gpt_decider(a.size, part, is_pv)) {
        return o;
    }

    // 离线路径
    let is_block = src.is_block;
    // 一次打开完成分类：目标上的未收尾现场在**锁下**判定（右侧"已空"可能正是搬了一半
    // 的结果），而不是先只读判一次再开第二次——判据与开目标之间不许留窗口。
    // 单分区 resize 与搬移作业共用同一份记录：`resumed` 是任一未收尾作业（分区层可能已
    // 提交，只等 FS/PV 收尾），FS 无变化的短路不能把后者当成"无事可做"；本分区的搬移
    // 进度另由记录里的搬移状态给出
    let (mut src, entry) = open_target_resize(a, a.grow_lv).unwrap_or_else(|f| bail_fail(f));
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
    // 记录里的搬移状态是本分区未收尾的搬移作业（"右侧已空"可能是搬移中间态）
    let relocation = src.record_journal().and_then(|j| j.relocation().cloned());
    let reloc = relocation.as_ref().is_some_and(|r| r.grow_part == part);
    let resuming = entry.resumed;
    // SIZE 锚点与 PV 判据按锁下的新几何重算：+N/+N% 锚定"当前尺寸"，窗口内分区被
    // 改写过，锁前锚点就已作废——拿旧锚点的绝对值对照新几何会把"扩"判成"缩"
    // （先缩 FS！），方向与请求相反。grow 标记是请求的形状（与盘无关），锁前锁后同值
    let cur_bytes = lba_range_bytes(start, end, ss);
    let (target, _) = resolve_size_request(a.size, cur_bytes).unwrap_or_else(|f| bail_fail(f));
    let shrinking = target.is_some_and(|t| t < cur_bytes);
    // 锁下的方向可能因窗口内的外部改写而与锁前相反（缩容一律不走这里）
    if shrinking {
        // 缩容撞上未收尾的搬移作业：按"目标被占用"拒绝并指向 abandon。只说"只扩"
        // 会把用户推向 resize-part，在那里再撞一次占用拒绝，两次都不提那份作业
        if reloc {
            bail_fail(RunOutcome::refused(
                "an unfinished operation still owns this target — release it with `diskedit abandon` \
                 (or re-run the original grow request to finish it first)"
                    .to_string(),
            ));
        }
        bail_fail(refuse_shrink());
    }
    // FS 类型与 PV 判据同样按锁下现状重取（与几何同理）：识别与取锁之间分区可被
    // 重新格式化（mkfs 不守本工具的锁），过期的 is_pv 会让 check_pv_intent 对着
    // 已不是 PV 的分区说 PV 的话
    let fstype = fsid::identify(&src, lba_bytes(start, ss), cur_bytes)
        .unwrap_or_else(|e| bail_fail(RunOutcome::no_mutation(format!("identify failed: {e}"))));
    let is_pv = fstype == fsid::ContentKind::Lvm2Pv;
    check_pv_intent(part, Some(fstype), is_pv, shrinking, a.grow_lv, a.no_fs).unwrap_or_else(|f| bail_fail(f));
    // 记录型续跑：记录里有提交前区间、且盘上区间已不是它 ⇒ 分区层提交过，当前区间
    // 就是本次作业的目标（`resize` 不改起点）。已提交的现场上现算右侧空闲会得到 0，
    // 照它分流会把"只差 FS/PV 收尾"拒成 nothing to grow / 右侧被挡。
    // 没有这段区间的记录（首次写盘之前中断，或不含提交前区间的记录）不适用：盘上没有任何
    // 提交，按未提交续做
    let record_committed = entry.resumed
        && src.record_journal()
            .and_then(|j| j.old_range())
            .is_some_and(|old| old != (start, end));
    // PV 链的工具必须在写表之前齐备（pvresize/pvs 及 grow-lv 的 lvs/vgs/lvextend；
    // 镜像目标另加 losetup——它的分区链要临时映射）：它们在表提交之后才启动，
    // 缺工具那时才发现就只剩 PARTIAL 加手工
    #[cfg(target_os = "linux")]
    if is_pv {
        preflight_lvm_tools(a.grow_lv, !is_block).unwrap_or_else(|e| bail_fail(RunOutcome::from(e)));
        // 锁下的 PV 判据可能已与锁前不同（mkfs 会换掉分区内容）：要扩的 LV 按锁下的
        // 分区节点与起始字节再确认一次，仍在写表之前
        if a.grow_lv {
            let start_bytes = start
                .checked_mul(ss)
                .unwrap_or_else(|| bail_fail(RunOutcome::no_mutation("partition start overflows byte offset")));
            lv_sel = Some(
                preflight_lv_selection(&src, part, is_block, start_bytes, a.lv.as_deref())
                    .unwrap_or_else(|e| bail_fail(lvm_preflight_fail(e))),
            );
        }
    } else {
        // 锁下已不是 PV：锁前那份选择对本次写盘没有意义（mkfs 换掉了分区内容）
        lv_sel = None;
    }
    // 记录里带着 LVM 收尾基线（上次请求过 --grow-lv），本次却没给：记录不存 LV 路径，
    // 收尾只能按本次请求重选。静默退成只跑 pvresize 等于把承诺过的 LV 扩容丢掉
    if lv_sel.is_none() && src.record_journal().is_some_and(|j| j.lvm_post().is_some()) {
        bail_fail(RunOutcome::refused(
            "an unfinished resize recorded an LV growth — re-run with --grow-lv (the record does not \
             store the LV path)"
                .to_string(),
        ));
    }
    // 盘上记录没有 LV 收尾基线，本次却给了 --grow-lv：记录是那次作业的完整意图，
    // 在已提交作业上追加一步没有记录承载的动作，失败后没有载体能解释它从哪来。
    // 首次运行（`entry.resumed` 为假）的记录是本次新建的，不在此列
    if entry.resumed && a.grow_lv && src.record_journal().is_some_and(|j| j.lvm_post().is_none()) {
        bail_fail(RunOutcome::refused(
            "an unfinished resize record carries no LV growth step — adding one to a job already recorded \
             leaves nothing behind to explain it if it fails; re-run without `--grow-lv`"
                .to_string(),
        ));
    }
    // 占用闸在 movepart 的 prepare 层（本分支的每条路径都经 prepare_resize/prepare_apply），
    // 不在命令层重复判一次
    if grow_to_end {
        // 记录型续跑且分区层已提交：目标区间就是盘上现状（`resize` 不改起点），交给
        // resize_part 的 committed 分支只收尾 FS/PV。这里改用现算的右侧空闲分流，
        // 会在"表已提交、FS 收尾未做"的现场上把续跑拒成 nothing to grow
        //（`grow` 的目标就是右侧空闲的末端，提交后右侧必无空闲）
        if record_committed {
            let (chunk, mut logger) = chunk_logger(a, &src, g.disk_guid());
            let o = crate::cmd::layout::resize_part_via_plan_with(&mut src, &g, repair, part, start, end, chunk, a.no_fs, lv_sel.clone(), is_pv, &mut |m| logger.log(m));
            let record = src.record_journal().map(|j| j.record());
            let o = settle_layout(o, &src, record);
            return finish_resize(a, o, Some(&mut src), part, is_pv, is_block, lv_sel.as_ref().map(|s| s.path.as_str()), cur_bytes);
        }
        let free = free_right_gpt(&g, part);
        // 右侧有空闲且没有未收尾的搬移作业 → 纯扩容。若作业未收尾，则"右侧已空"很可能
        // 正是搬了一半的结果，走普通 resize_part 会跳过剩余搬移与 swap 重建等收尾
        if free > 0 && !reloc {
            // free 的上界由 free_right_gpt 的构造保证（≤ last_usable - end）；checked
            // 把这层非局部依赖显式化，回绕值进不了计划
            let Some(new_end) = end.checked_add(free) else {
                bail_fail(RunOutcome::no_mutation("free-space arithmetic overflow (geometry inconsistent)"));
            };
            let (chunk, mut logger) = chunk_logger(a, &src, g.disk_guid());
            let o = crate::cmd::layout::resize_part_via_plan_with(&mut src, &g, repair, part, start, new_end, chunk, a.no_fs, lv_sel.clone(), is_pv, &mut |m| logger.log(m));
            let record = src.record_journal().map(|j| j.record());
            let o = settle_layout(o, &src, record);
            return finish_resize(a, o, Some(&mut src), part, is_pv, is_block, lv_sel.as_ref().map(|s| s.path.as_str()), cur_bytes);
        }
        // 已顶到 last_usable 的分区不是"被挡"，是无可再扩：occupied 文案会把用户引进
        // --allow-move 的空搬移（空 moves 的 plan 什么都没做却报成功）。
        // PV 例外：分区层没有可扩的扇区，但 pvresize/lvextend 仍是本次操作的后置条件
        // （上一次运行可能正停在提交之后），交给收尾链续做
        if !reloc && end == last_usable {
            if is_pv {
                let (chunk, mut logger) = chunk_logger(a, &src, g.disk_guid());
                let o = crate::cmd::layout::resize_part_via_plan_with(&mut src, &g, repair, part, start, end, chunk, a.no_fs, lv_sel.clone(), true, &mut |m| logger.log(m));
                let record = src.record_journal().map(|j| j.record());
                let o = settle_layout(o, &src, record);
                return finish_resize(a, o, Some(&mut src), part, is_pv, is_block, lv_sel.as_ref().map(|s| s.path.as_str()), cur_bytes);
            }
            bail_fail(RunOutcome::refused(format!(
                "partition already ends at last_usable_lba {last_usable} — nothing to grow"
            )));
        }
        // 右侧被挡：自动搬移挡路分区（plan 打印 → --allow-move 放行 → --yes 确认）。
        // --allow-move 只对**新**搬移授权：续跑是"接着做用户已确认过的那件事"，
        // 被卡的挡路分区正是那份作业的一部分——再要一次授权只会把承诺"重跑原命令
        // 即续跑"变成谎话。要求豁免的两态由此分开：未收尾作业放行，真空间不足才拒绝
        if !a.allow_move && !reloc {
            bail_fail(RunOutcome::refused("right side is occupied — pass --allow-move to relocate the blocking partitions (plan will be printed; --yes confirms)".to_string()));
        }
        // 记录型现场直接承载搬移计划：没有第二族现场需要让位
        let mut plan = match movepart::make_plan_resuming(&src, relocation.as_ref(), &g, repair, part) {
            Ok(p) => p,
            Err(f) => bail_fail(f),
        };
        // 续跑的收尾仍须 --yes：盘上状态已与上次请求时不同，写盘前再确认一次；
        // --yes 一并覆盖"未确认的续跑"与"新的搬移"两种进入方式
        crate::cmd::plan::print_plan(&plan, start).unwrap_or_else(|e| bail_fail(RunOutcome::no_mutation(format!("plan failed: {e}"))));
        let (chunk, mut logger) = chunk_logger(a, &src, g.disk_guid());
        // 槽位在进入搬移前绑定本请求：记录属于另一份 resize 时在这里拒绝，不让搬移
        // 按别人的游标继续。终点取本作业落定的新末端（`grow_end_for`，与执行半程同一
        // 算式）：提交后的重跑走 `record_committed` 分支，那里按盘上末端绑摘要，两者
        // 必须同值，否则"表已提交、FS 未收尾"的合法续跑会被自己拒掉。
        // 这条稳定性依赖**续跑时 plan 由记录重建**（`resume_outcome`）：改成"从盘上
        // 现算 plan 再比对"后，逐条位置检查会先拒，摘要随盘面漂移。`grow` 的摘要因此
        // 不对"磁盘变大导致 grow 终点变化"敏感——那由 `verify_relocation_resume` 的
        // `disk_size` 与条目位置检查兜住
        let grow_end = movepart::grow_end_for(&plan, start)
            .unwrap_or_else(|e| bail_fail(RunOutcome::no_mutation(e.to_string())));
        let digest = crate::recovery::PlanDigest::of_request(
            crate::recovery::OpKind::Resize,
            part,
            start,
            grow_end,
            g.sector_size(),
            src.size,
        );
        if let Some(journal) = src.record_journal_mut()
            && let Err(why) = journal.bind_plan(digest)
        {
            bail_fail(RunOutcome::refused(why));
        }
        let o = apply_move(&mut src, &g, &mut plan, chunk, a.no_fs, is_pv, lv_sel.clone(), &mut |m| logger.log(m));
        let record = src.record_journal().map(|j| j.record());
        let o = settle_layout(o, &src, record);
        finish_resize(a, o, Some(&mut src), part, is_pv, is_block, lv_sel.as_ref().map(|s| s.path.as_str()), cur_bytes)
    } else {
        // SIZE：字节 → 扇区（下取整）；扩须右侧空闲足够，缩由 resize_part 内部 FS 先缩 + 守卫链
        let Some(bytes) = target else {
            bail_fail(RunOutcome::refused("`resize` needs a size: diskedit resize <TARGET>[:N] <SIZE> (see diskedit help resize)".to_string()));
        };
        if bytes < ss {
            bail_fail(RunOutcome::refused(format!("size {bytes} < one sector ({ss})")));
        }
        // SIZE 是外部输入：换算与相加全程 checked，回绕会把越过 last_usable 的荒谬值送进比较
        let Some(new_end) = start.checked_add(bytes / ss).and_then(|e| e.checked_sub(1)) else {
            bail_fail(RunOutcome::refused(format!("size {bytes} overflows the LBA range")));
        };
        if new_end > last_usable {
            bail_fail(RunOutcome::refused(format!("size {bytes} exceeds usable range (partition would end past last_usable_lba {last_usable})")));
        }
        // 请求已通过越界判定，note 不会再跟在一条拒绝后面
        crate::support::note_sector_truncation("resizing to", bytes, ss);
        // 请求的形状三选一：尺寸未变算不出正位移，它可能是原样重跑同一条扩容命令；
        // 缩容已在入口拒绝，这里只剩扩与不变两种。搬移量只在扩时有定义
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
            return crate::outcome::RunOutcome::applied_with(Vec::new());
        }
        // 未收尾的搬移作业 ⇒ 必须走 resume 路径（即使几何上 free_right 已足够——swap 等
        // 收尾步骤可能尚未执行，普通扩容会跳过它们）
        if reloc || matches!(intent, movepart::ShiftIntent::Grow(s) if s > free_right_gpt(&g, part)) {
            // --allow-move 只对**新**搬移授权，续跑放行——两态判据与 grow 分支
            // 同一份理由，真空间不足才拒绝；确认流也同一份：
            // plan 打印 → --yes 确认
            if !a.allow_move && !reloc {
                bail_fail(RunOutcome::refused("not enough contiguous free space to the right — pass --allow-move to relocate the blocking partitions (plan will be printed; --yes confirms)".to_string()));
            }
            // 记录型现场直接承载搬移计划：没有第二族现场需要让位
            let mut plan = match movepart::make_plan_shift_resuming(&src, relocation.as_ref(), &g, repair, part, intent) {
                Ok(p) => p,
                Err(f) => bail_fail(f),
            };
            crate::cmd::plan::print_plan(&plan, start).unwrap_or_else(|e| bail_fail(RunOutcome::no_mutation(format!("plan failed: {e}"))));
            let (chunk, mut logger) = chunk_logger(a, &src, g.disk_guid());
            // 与 `grow` 分支同一处绑定：请求终点是 SIZE 换算出的新末端，改名/改大小
            // 重跑都会在进入搬移前被拒；`--chunk` 不在摘要里，由搬移校验单独核对
            let digest = crate::recovery::PlanDigest::of_request(
                crate::recovery::OpKind::Resize,
                part,
                start,
                new_end,
                g.sector_size(),
                src.size,
            );
            if let Some(journal) = src.record_journal_mut()
                && let Err(why) = journal.bind_plan(digest)
            {
                bail_fail(RunOutcome::refused(why));
            }
            let o = apply_move(&mut src, &g, &mut plan, chunk, a.no_fs, is_pv, lv_sel.clone(), &mut |m| logger.log(m));
            let record = src.record_journal().map(|j| j.record());
            let o = settle_layout(o, &src, record);
            return finish_resize(a, o, Some(&mut src), part, is_pv, is_block, lv_sel.as_ref().map(|s| s.path.as_str()), cur_bytes);
        }
        let (chunk, mut logger) = chunk_logger(a, &src, g.disk_guid());
        let o = crate::cmd::layout::resize_part_via_plan_with(&mut src, &g, repair, part, start, new_end, chunk, a.no_fs, lv_sel.clone(), is_pv, &mut |m| logger.log(m));
        let record = src.record_journal().map(|j| j.record());
        let o = settle_layout(o, &src, record);
        finish_resize(a, o, Some(&mut src), part, is_pv, is_block, lv_sel.as_ref().map(|s| s.path.as_str()), cur_bytes)
    }
}
