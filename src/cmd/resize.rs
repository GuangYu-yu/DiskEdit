//! resize：分区 + 文件系统的一键扩缩（GPT / MBR / superfloppy，在线与离线自动选择）。

use crate::support::*;
use crate::args::{parse_size_delta, Args};
use crate::dev::FileSource;
use crate::{dev, fsid, fsops, movepart, table};

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
    --grow-lv       also grow the associated LV (--lv NAME, or the only LV)
    --yes           skip confirmation

  Automatically:
    detects partition / filesystem / LVM PV, chooses online or offline,
    resizes partition -> PV -> LV -> filesystem as required; long operations
    are checkpointed and resumed by re-running the same command (an
    interrupted single-partition resize job is resumed by re-running
    `resize-part` or `move`, not `resize`).

  Notes: PV shrink is refused (use the lvreduce/pvresize chain). --no-fs skips
  the filesystem steps, so it cannot shrink: the FS has to be shrunk first."#;

/// SIZE 参数 →（绝对目标字节数, grow 标记），GPT/MBR resize 共用。
/// 绝对值/扩/缩都锚定当前分区字节数；两者皆缺 = 未指定 SIZE
fn resolve_size_request(a: &Args, size_arg: Option<&str>, cur_bytes: u64) -> (Option<u64>, bool) {
    let mut grow_to_end = a.grow_to_end;
    let mut target: Option<u64> = a.size;
    if let Some(s) = size_arg {
        if s == "grow" {
            grow_to_end = true;
        } else {
            let (v, kind, pct) = parse_size_delta(s)
                .unwrap_or_else(|| bail_fail(Fail::refused(format!("bad SIZE {s:?} (use 10G | +2G | -500M | +10% | grow; see diskedit help resize)"))));
            if pct {
                // 百分比增量：锚定当前分区字节数，先乘后除（u128 防溢出）避免丢余数，
                // 再向下取整到 1MiB，保证结果落在扇区/对齐界内
                let raw = (cur_bytes as u128).checked_mul(v as u128)
                    .and_then(|x| x.checked_div(100))
                    .filter(|x| *x <= u64::MAX as u128)
                    .unwrap_or_else(|| bail_fail(Fail::refused(format!("{s} overflows partition size")))) as u64;
                let delta = raw / (1024 * 1024) * (1024 * 1024);
                target = Some(match kind {
                    1 => cur_bytes.checked_add(delta).unwrap_or_else(|| bail_fail(Fail::refused(format!("{s} overflows partition size")))),
                    _ => cur_bytes.checked_sub(delta).unwrap_or_else(|| bail_fail(Fail::refused(format!("{s} exceeds current size {cur_bytes}")))),
                });
            } else {
                target = Some(match kind {
                    0 => v,
                    1 => cur_bytes.checked_add(v).unwrap_or_else(|| bail_fail(Fail::refused(format!("{s} overflows partition size")))),
                    _ => cur_bytes.checked_sub(v).unwrap_or_else(|| bail_fail(Fail::refused(format!("{s} exceeds current size {cur_bytes}")))),
                });
            }
        }
    }
    if target.is_none() && !grow_to_end {
        bail_fail(Fail::refused("specify a SIZE (e.g. +20G, -500M, 10G, grow; see diskedit help resize)".to_string()));
    }
    (target, grow_to_end)
}

/// PV / --grow-lv 的事前判据：与表类型无关，故 GPT 与 MBR 两条 resize 路径共用一份。
/// 两条判据都必须落在任何写盘之前——PV 缩容要经 lvreduce/pvresize 链（本工具不做），
/// 一旦先改了分区表就留下"分区已缩、PV 元数据未动"的不一致
fn check_pv_intent(
    part: u32,
    fstype: &str,
    is_pv: bool,
    shrinking: bool,
    grow_lv: bool,
) -> Result<(), crate::outcome::Fail> {
    if is_pv {
        if shrinking {
            return Err(crate::outcome::Fail::refused(
                "shrinking an LVM PV needs the lvreduce/pvresize chain — do it manually (see pvresize(8))",
            ));
        }
    } else if grow_lv {
        return Err(crate::outcome::Fail::refused(format!(
            "partition {part} is not an LVM PV (identified as {fstype}) — --grow-lv needs a PV"
        )));
    }
    Ok(())
}

/// 在线路径前置守卫：活动 swap 拒绝（run swapoff 后重试）
#[cfg(target_os = "linux")]
fn refuse_swap_active(dn: &str, part: u32) {
    if crate::online::swap_active(dn, part) {
        bail_fail(Fail::refused(format!("partition {part} is active swap — run swapoff first")));
    }
}

/// resize 请求里**与表类型无关**的部分：写盘前的事实快照 + 目标尺寸。
/// 表类型特有的差异收敛成一件事实——右侧连续空闲（GPT 按修复后的 last_usable，
/// MBR 按 32 位上限与后继条目），故由各自的几何函数算好 `free_right_lba` 填入。
/// 这样在线路径与 `check_pv_intent` 只写一份，不会各自演化。
///
/// `free_right_lba` / `cur_bytes` 是**锁前**快照：在线路径的锁在 `online` 模块内部取得
///（身份取自解析结果，此处还拿不到它）。写表前 `online` 会在锁下重取 sysfs 快照，
/// 由 `check_new_range` 复核容量与邻接——过扩/重叠有锁下防线；残余是 free 的**数额**
/// 不在锁下重导出（在线路径刻意不解析分区表），窗口内布局变化可能少扩或被拒，不会越界
#[cfg(target_os = "linux")]
struct ResizeTarget {
    part: u32,
    ss: u64,
    cur_bytes: u64,
    is_pv: bool,
    free_right_lba: u64,
    target: Option<u64>,
    grow_to_end: bool,
}

/// 块设备在线路径：**表类型无关**（写表经 sfdisk / BLKPG），随表类型变化的只有
/// `free_right_lba` 一项，故由调用方算好传入。返回 Some(退出码) = 已按在线路径处理完毕；
/// None = 不适用（镜像，或未挂载的非 PV）→ 调用方继续离线路径。
/// 派生不出盘名即拒绝而非继续：此时无法探测挂载状态，退到离线路径可能在 FS 挂载中写表
#[cfg(target_os = "linux")]
fn resize_online(a: &Args, src: &FileSource, t: &ResizeTarget) -> Option<u8> {
    if !src.is_block {
        return None;
    }
    let dn = std::path::Path::new(&a.target)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| bail_fail(Fail::refused(format!("cannot derive disk name from {}", a.target))));
    refuse_swap_active(&dn, t.part);

    // grow-to-end 的目标长度 = 现长 + 右侧空闲字节数。乘加全程 checked：数值来自
    // 表/sysfs、受盘容量约束本不可能溢出，一旦溢出即事实已损坏——报明确错误，
    // 而不是回绕成小值骗过下游的容量校验
    let grow_full_len = |t: &ResizeTarget| -> u64 {
        t.free_right_lba
            .checked_mul(t.ss)
            .and_then(|f| t.cur_bytes.checked_add(f))
            .unwrap_or_else(|| {
                bail_fail(Fail::infra(format!(
                    "grown size overflows ({} + {} sectors × {} B)",
                    t.cur_bytes, t.free_right_lba, t.ss
                )))
            })
    };

    // PV：分区层必须先按新尺寸出现在内核里，pvresize 才能吸收；活跃 LV 经 dm 持有分区使
    // BLKRRPART 返回 EBUSY，故走 sfdisk+partx 同步路径
    if t.is_pv {
        let new_len = if t.grow_to_end {
            if t.free_right_lba == 0 {
                bail_fail(Fail::refused("no free space to the right — a PV cannot relocate blocking partitions while LVs may be active".to_string()));
            }
            grow_full_len(t)
        } else {
            t.target.unwrap_or(t.cur_bytes) / t.ss * t.ss // 扇区下取整，与离线路径同规则
        };
        if new_len != t.cur_bytes {
            let o = crate::online::resize_pv_online(&dn, t.part, new_len);
            if !o.is_complete() {
                o.report();
                return Some(o.exit_code());
            }
        }
        return Some(resize_done(a, true, true, t.cur_bytes));
    }

    // 非 PV：仅挂载中的分区能在线扩（在线不能搬移，只吃连续空闲）
    let mnt = crate::online::find_mountpoint(&dn, t.part)?;
    // 无右侧空闲 = 分区已吃满 → None 让 FS 工具扩满现分区
    let size = if t.grow_to_end {
        let grown = grow_full_len(t);
        (grown != t.cur_bytes).then_some(grown)
    } else {
        t.target
    };
    let o = crate::online::resize_online(&mnt, size);
    if o.is_complete() {
        println!("resized online (verify with: diskedit info {})", a.target);
    } else {
        o.report();
    }
    Some(o.exit_code())
}

/// 块设备的分区节点路径：命名规则唯一实现在 `dev::part_node_name`
#[cfg(target_os = "linux")]
fn part_dev_path(target: &str, part: u32) -> String {
    crate::dev::part_node_name(target.trim_end_matches('/'), part)
}

/// 分区扩容成功后的 LVM 链（仅块设备）：pvresize 吸收全部新增空间；--grow-lv 把本次新增
/// 传给目标 LV（--lv 指定或该 PV 上唯一顶层 LV），向下取整到 VG extent，不消费原有空闲。
/// lvextend 按容量扩，分配源由 LVM 决定，不限于本 PV
#[cfg(target_os = "linux")]
fn lvm_grow_chain(part_dev: &str, delta_bytes: u64, grow_lv: bool, want_lv: Option<&str>) -> Result<(), String> {
    crate::lvm::pv_resize(part_dev)?;
    println!("pvresize {part_dev} done");
    if !grow_lv {
        return Ok(());
    }
    let vg = match crate::lvm::vg_of(part_dev) {
        Ok(Some(vg)) => vg,
        Ok(None) => return Err(format!("pvresize done but {part_dev} is a PV outside any VG — nothing to extend")),
        Err(e) => return Err(e),
    };
    let lvs = crate::lvm::lvs_on_pv(&vg, part_dev)?;
    let (name, path) = match want_lv {
        // 匹配裸 LV 名或 /dev/<vg>/<name> 路径（lv_path 精确比较，不做后缀模糊匹配）
        Some(sel) => match lvs.iter().find(|(n, p)| n == sel || p == &format!("/dev/{sel}")) {
            Some(x) => x.clone(),
            None => {
                let names = lvs.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>().join(", ");
                return Err(format!("LV {sel:?} not found on {part_dev} in VG {vg} (top-level LVs: {names})"));
            }
        },
        None => match lvs.len() {
            1 => lvs[0].clone(),
            0 => return Err(format!("VG {vg}: no top-level LV uses {part_dev} — nothing to extend")),
            _ => {
                let names = lvs.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>().join(", ");
                return Err(format!("VG {vg}: multiple LVs on {part_dev} — pick one with --lv NAME (candidates: {names})"));
            }
        },
    };
    let ext = crate::lvm::vg_extent_size(&vg)?;
    let n = delta_bytes / ext;
    if n == 0 {
        return Err(format!("added {delta_bytes} bytes < one VG extent ({ext} bytes) — LV left unchanged"));
    }
    crate::lvm::lv_extend(&path, n)?;
    println!("lvextend -l +{n} -r {path} done (lv {name})");
    Ok(())
}

/// resize 的一键入口。SIZE：`10G`=绝对值、`+2G`/`-500M`=增量、`grow`=吃满右侧可用区
/// （挡路分区须 --allow-move，搬移计划须 --yes 确认）。LVM PV 扩容自动 pvresize，
/// --grow-lv 再把新增传给目标 LV 并扩 FS；PV 缩容一律拒绝（走 lvreduce/pvresize 链）
pub(crate) fn cmd_resize(a: &Args) -> u8 {
    let size_arg = a.pos.get(1).cloned();
    if size_arg.is_some() && (a.size.is_some() || a.grow_to_end) {
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
    let src = open_target_ro(a).unwrap_or_else(|f| bail_fail(f));
    match table::table_label(&src) {
        Ok(table::TableLabel::Gpt) => {}
        Ok(table::TableLabel::Mbr) => {
            let Some(part) = a.part else { crate::args::usage() };
            return cmd_resize_msdos(a, part, size_arg.as_deref(), &src);
        }
        // superfloppy：无分区表，FS 即整盘，无表可写——纯 FS grow
        Ok(table::TableLabel::None) => return cmd_resize_superfloppy(a, size_arg.as_deref(), &src),
        Ok(other) => bail_fail(Fail::refused(format!("resize requires a GPT or MBR target (label: {other})"))),
        Err(e) => bail_fail(Fail::infra(format!("parse failed: {e}"))),
    }
    let Some(part) = a.part else { crate::args::usage() };
    // 可操作几何（构造点即拒绝条目重叠）：entries / ss / 修复后的 last_usable 全部取自它，
    // 命令层不再自己算一次有效上界（历史实现见 support::effective_last_usable）
    let (g, _repair) = match crate::gpt_policy::resolve_geometry(&src) {
        Ok(Some(v)) => v,
        Ok(None) => bail_fail(Fail::refused("resize requires a GPT target".to_string())),
        Err(f) => bail_fail(f),
    };
    let e = crate::gpt_policy::live_entry(&g, part).unwrap_or_else(|f| bail_fail(f));
    let (start, end, ss) = (e.starting_lba, e.ending_lba, g.ss);
    let cur_bytes = (end - start + 1) * ss;
    let fstype = fsid::identify(&src, start * ss, (end - start + 1) * ss).unwrap_or_else(|e| bail_fail(Fail::infra(format!("identify failed: {e}"))));
    let is_pv = fstype == "lvm2_pv";

    // SIZE → 绝对目标字节数 / grow 标记
    let (target, grow_to_end) = resolve_size_request(a, size_arg.as_deref(), cur_bytes);
    let shrinking = target.is_some_and(|t| t < cur_bytes);

    check_pv_intent(part, fstype, is_pv, shrinking, a.grow_lv).unwrap_or_else(|f| bail_fail(f));

    // 块设备在线路径（表类型无关，随表类型变的只有右侧空闲数）；镜像或未挂载的非 PV 落到离线路径
    #[cfg(target_os = "linux")]
    if let Some(code) = resize_online(a, &src, &ResizeTarget {
        part,
        ss,
        cur_bytes,
        is_pv,
        free_right_lba: free_right_gpt(&g, part),
        target,
        grow_to_end,
    }) {
        return code;
    }

    // 离线路径
    let is_block = src.is_block;
    // 一次打开完成分类：有没有自己分区的未收尾搬移在**锁下**判定（右侧"已空"可能正是
    // 搬了一半的结果），而不是先只读判一次再开第二次——判据与开目标之间不许留窗口。
    // 返回的 resumed 决定 grow 分支：续跑时"右侧已空"可能是搬移的中间态
    let (mut src, resuming) =
        open_target_resumable(a, part).unwrap_or_else(|f| bail_fail(f));
    // 锁下重取权威几何：写路径的每个 LBA（start/end/free/last_usable）都来自它。
    // 锁前那份只服务参数早失败与在线路径的事实快照；只读阶段与取得独占权之间
    // 盘可以被别人改写，用锁前的值驱动写分支就是把过期决定写进盘
    let (g, repair) = match crate::gpt_policy::resolve_geometry(&src) {
        Ok(Some(v)) => v,
        Ok(None) => bail_fail(Fail::refused("resize requires a GPT target".to_string())),
        Err(f) => bail_fail(f),
    };
    let e = crate::gpt_policy::live_entry(&g, part).unwrap_or_else(|f| bail_fail(f));
    let (start, end, ss) = (e.starting_lba, e.ending_lba, g.ss);
    let last_usable = g.last_usable_lba();
    // SIZE 锚点与 PV 判据按锁下的新几何重算：+N/+N% 锚定"当前尺寸"，窗口内分区被
    // 改写过，锁前锚点就已作废——拿旧锚点的绝对值对照新几何会把"扩"判成"缩"
    // （先缩 FS！），方向与请求相反。grow 标记是请求的形状（与盘无关），锁前锁后同值
    let cur_bytes = (end - start + 1) * ss;
    let (target, _) = resolve_size_request(a, size_arg.as_deref(), cur_bytes);
    let shrinking = target.is_some_and(|t| t < cur_bytes);
    check_pv_intent(part, fstype, is_pv, shrinking, a.grow_lv).unwrap_or_else(|f| bail_fail(f));
    if grow_to_end {
        let free = free_right_gpt(&g, part);
        // 右侧有空闲且没有未收尾的搬移作业 → 纯扩容。若作业未收尾，则"右侧已空"很可能
        // 正是搬了一半的结果，走普通 resize_part 会跳过剩余搬移与 swap 重建等收尾
        if free > 0 && !resuming {
            let (chunk, mut logger) = chunk_logger(a, &src, g.header.disk_guid);
            let o = settle_layout(movepart::resize_part(&mut src, &g, repair, part, start, end + free, chunk, a.no_fs, &mut |m| logger.log(m)), &src);
            return finish_resize(a, o, is_pv, is_block, cur_bytes);
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
        // 槽上未收尾的作业若是精确 SIZE 的最小位移 plan，其终点停在旧请求的末端：
        // 当作 grow 执行会把请求静默缩水。重跑当初那条 SIZE 命令即续跑
        if plan.kind == movepart::PlanKind::Shift {
            bail_fail(Fail::refused("an unfinished `resize SIZE` relocation job owns this target — re-run that command to resume it, or `diskedit abandon` to release it".to_string()));
        }
        // 续跑的收尾仍须 --yes：盘上状态已与上次请求时不同，写盘前再确认一次；
        // --yes 一并覆盖"未确认的续跑"与"新的搬移"两种进入方式
        crate::cmd::plan::print_plan(&plan, start).unwrap_or_else(|e| bail_fail(Fail::refused(format!("plan failed: {e}"))));
        if !a.yes {
            bail_fail(Fail::refused("this resizes by relocating the partitions listed above — review and re-run with --yes"));
        }
        let (chunk, mut logger) = chunk_logger(a, &src, g.header.disk_guid);
        let o = settle_layout(movepart::apply(&mut src, &g, &plan, chunk, a.no_fs, &mut |m| logger.log(m)), &src);
        finish_resize(a, o, is_pv, is_block, cur_bytes)
    } else {
        // SIZE：字节 → 扇区（下取整）；扩须右侧空闲足够，缩由 resize_part 内部 FS 先缩 + 守卫
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
        // 扩容需要的位移量只在扩的时候有定义：缩容的新末端更靠左，右侧只会更空，
        // 不存在搬移需求（此时 new_end < end，直接相减会回绕）
        let shift = (new_end > end).then(|| new_end - end);
        // 未收尾的搬移作业 ⇒ 必须走 resume 路径（即使几何上 free_right 已足够——swap 等
        // 收尾步骤可能尚未执行，普通扩容会跳过它们）
        if resuming || shift.is_some_and(|s| s > free_right_gpt(&g, part)) {
            // --allow-move 只对**新**搬移授权，续跑放行——两态判据与 grow 分支
            // 同一份理由（见上），真空间不足才拒绝；确认流也同一份：
            // plan 打印 → --yes 确认
            if !a.allow_move && !resuming {
                bail_fail(Fail::refused("not enough contiguous free space to the right — pass --allow-move to relocate the blocking partitions (plan will be printed; --yes confirms)".to_string()));
            }
            let plan = match movepart::make_plan_shift_resuming(&mut src, &g, repair, part, shift) {
                Ok(p) => p,
                Err(f) => bail_fail(f),
            };
            // 反向同理：槽上是 grow 的尾打包 plan，终点是"吃满右侧"而非请求的 SIZE，
            // 静默执行会把请求放大到全部空闲（缩容请求甚至会变成扩容）。
            // 重跑当初那条 grow 命令即续跑
            if plan.kind == movepart::PlanKind::TailPacked {
                bail_fail(Fail::refused("an unfinished `resize grow` job owns this target — re-run that command to resume it, or `diskedit abandon` to release it".to_string()));
            }
            crate::cmd::plan::print_plan(&plan, start).unwrap_or_else(|e| bail_fail(Fail::refused(format!("plan failed: {e}"))));
            if !a.yes {
                bail_fail(Fail::refused("this resizes by relocating the partitions listed above — review and re-run with --yes"));
            }
            let (chunk, mut logger) = chunk_logger(a, &src, g.header.disk_guid);
            let o = settle_layout(movepart::apply(&mut src, &g, &plan, chunk, a.no_fs, &mut |m| logger.log(m)), &src);
            return finish_resize(a, o, is_pv, is_block, cur_bytes);
        }
        let (chunk, mut logger) = chunk_logger(a, &src, g.header.disk_guid);
        let o = settle_layout(movepart::resize_part(&mut src, &g, repair, part, start, new_end, chunk, a.no_fs, &mut |m| logger.log(m)), &src);
        finish_resize(a, o, is_pv, is_block, cur_bytes)
    }
}

/// superfloppy resize（无分区表，FS 即整盘）：无表可写，唯一有意义的是 FS grow
/// 到盘/镜像末端——缩无处可缩（无分区边界），显式 SIZE 只接受等于当前值。
/// 镜像/盘须已是大尺寸（dd 后或 truncate 预扩），本命令不负责扩文件本身
fn cmd_resize_superfloppy(a: &Args, size_arg: Option<&str>, src_ro: &FileSource) -> u8 {
    if let Some(n) = a.part {
        bail_fail(Fail::refused(format!("target has no partition table — drop :{n} (the FS occupies the whole device)")));
    }
    if a.grow_lv || a.lv.is_some() {
        bail_fail(Fail::refused("--grow-lv/--lv needs an LVM PV — superfloppy has no partitions".to_string()));
    }
    // superfloppy 无分区可改，唯一动作就是 FS 扩容：--no-fs 会让命令无事可做，
    // --allow-move 无分区可搬。静默照常执行会做出旗标明确排除的事
    if a.no_fs {
        bail_fail(Fail::refused("--no-fs leaves nothing to do on a superfloppy — there is no partition to change, the only action is the filesystem grow".to_string()));
    }
    if a.allow_move {
        bail_fail(Fail::refused("--allow-move has nothing to do on a superfloppy — there are no partitions to relocate".to_string()));
    }
    let cur_bytes = src_ro.size;
    let (target, grow_to_end) = resolve_size_request(a, size_arg, cur_bytes);
    if let Some(t) = target {
        if t < cur_bytes {
            bail_fail(Fail::refused("superfloppy cannot shrink — the FS occupies the whole device, there is no partition boundary to shrink to".to_string()));
        }
        if t > cur_bytes {
            bail_fail(Fail::refused("target exceeds device/image size — extend the image or replace the disk first (this tool does not resize the container)".to_string()));
        }
        // SIZE == 当前值：与 grow 等价（FS 可能仍小于盘）
    } else if !grow_to_end {
        unreachable!("resolve_size_request guarantees a target or grow_to_end");
    }
    let fstype = fsid::identify(src_ro, 0, cur_bytes)
        .unwrap_or_else(|e| bail_fail(Fail::infra(format!("identify failed: {e}"))));
    if matches!(fstype, "lvm2_pv" | "swap" | "unknown") {
        bail_fail(Fail::refused(format!("whole-device {fstype} is not a resizable filesystem (no partition table on target)")));
    }
    // FS grow 本身不改分区表，无 kernel_resync 必要
    let src = open_target_for_write(a).unwrap_or_else(|f| bail_fail(f));
    fsops::resize_fs_whole(&src, fstype).unwrap_or_else(|e| bail_fail(Fail::from(e).context("FS grow failed")));
    println!("superfloppy: {fstype} grown to full device ({} bytes) — verify with: diskedit info {}", cur_bytes, a.target);
    EXIT_OK
}

/// MBR 分区被扩到更大之后的收尾：FS 步（swap 重建 / FS 扩容）→ 内核重读 → 统一收尾。
/// grow-to-end 与显式 SIZE 扩容的后置条件完全相同，故两条路径共用本函数——
/// FS 步是后置条件的一部分，缺了会"分区变大、文件系统没变大"却报成功。
/// `table_written` 决定是否需要内核重读；p 是**扩容前**解析出的条目（swap 头部探测用它
/// 原区间，原尺寸是 LVM 位移的基线）
fn mbr_grow_finish(
    a: &Args,
    src: &FileSource,
    p: &table::MbrPartition,
    fstype: &str,
    table_written: bool,
    is_pv: bool,
    is_block: bool,
) -> u8 {
    let part = p.num;
    let ss = src.sector_size;
    let cur_bytes = p.size_lba as u64 * ss;
    let mut pending: Vec<crate::outcome::Pending> = Vec::new();
    // --no-fs：分区层之外的后置条件整体出局，与 GPT 路径同语义
    if !a.no_fs {
        match fstype {
            // 探测用扩容前的区间：swap 签名恒在分区首 32K 内，起点未变，原长度足够容纳
            "unknown" | "lvm2_pv" => {
                match movepart::swap_rebuild_pending(
                    src, part, p.start_lba as u64 * ss, p.size_lba as u64 * ss,
                ) {
                    Ok(Some(missed)) => pending.push(missed),
                    Ok(None) => {}
                    // 探测读失败属设备故障，且此刻表多半已写：Failed 的"盘可能已改变"
                    // 警示比伪造一条 Pending 如实
                    Err(e) => bail_fail(Fail::from(e)),
                }
            }
            // swap：内容可弃，表项已扩 → mkswap 重建使新空间生效（UUID/卷标保持；
            // 离线路径仅镜像，块设备走在线路径且 active swap 已被守卫拒绝）
            "swap" => {
                let ident = movepart::read_swap_identity(src, p.start_lba as u64, p.size_lba as u64, ss);
                if let Err(e) = fsops::recreate_swap(src, part, ident) {
                    pending.push(crate::outcome::Pending::new(
                        part,
                        crate::outcome::PendingKind::Swap,
                        e.to_string(),
                        fsops::rescue_hint("swap", &dev::part_dev_hint(src, part, p.start_lba as u64 * ss)),
                    ));
                }
            }
            _ => {
                if let Err(e) = fsops::resize_fs(src, part, fstype) {
                    pending.push(crate::outcome::Pending::new(
                        part,
                        crate::outcome::PendingKind::Fs,
                        e.to_string(),
                        fsops::rescue_hint(fstype, &dev::part_dev_hint(src, part, p.start_lba as u64 * ss)),
                    ));
                }
            }
        }
    }
    if pending.is_empty() {
        // 表已写 → 走统一收尾（内核重读 + 报告）；未写表则无需重读
        let o = if table_written {
            settle_layout(crate::outcome::Outcome::applied_with(Vec::new()), src)
        } else {
            crate::outcome::Outcome::applied_with(Vec::new())
        };
        finish_resize(a, o, is_pv, is_block, cur_bytes)
    } else {
        let mut o = crate::outcome::Outcome::applied_with(pending);
        if table_written && !kernel_resync(src) {
            o.mark_kernel_stale();
        }
        o.report();
        o.exit_code()
    }
}

/// resize 的 MBR 分支（仅主分区 1..4；逻辑分区与扩展容器不支持）。原位纯扩缩：扩须右侧
/// 空闲足够（MBR 无搬移能力），缩走与 GPT 相同的"FS 先缩 → 写表"守卫链。块设备复用在线
/// 路径（基于 sysfs + sfdisk，与表类型无关）
fn cmd_resize_msdos(a: &Args, part: u32, size_arg: Option<&str>, src_ro: &FileSource) -> u8 {
    // MBR resize 没有搬移能力：--allow-move 承诺的"搬开挡路分区"不存在，
    // 静默按不可搬移处理会让来自脚本的调用只看到一条"空间不足"
    if a.allow_move {
        bail_fail(Fail::refused("--allow-move is not supported for MBR resize — MBR cannot relocate blocking partitions".to_string()));
    }
    let mbr = table::parse_mbr(src_ro)
        .unwrap_or_else(|e| bail_fail(Fail::infra(format!("parse failed: {e}"))))
        .unwrap_or_else(|| bail_fail(Fail::refused("no MBR on target".to_string())));
    let p = match mbr.iter().find(|p| p.num == part) {
        Some(p) => p,
        None => bail_fail(Fail::refused(format!("partition {part} not found (MBR resize covers primary partitions 1..4 only)"))),
    };
    if p.is_container {
        bail_fail(Fail::refused("extended partition container cannot be resized (logical partitions are out of scope)".to_string()));
    }
    let ss = src_ro.sector_size;
    let cur_bytes = p.size_lba as u64 * ss;
    let fstype = fsid::identify(src_ro, p.start_lba as u64 * ss, p.size_lba as u64 * ss)
        .unwrap_or_else(|e| bail_fail(Fail::infra(format!("identify failed: {e}"))));
    let is_pv = fstype == "lvm2_pv";

    let (target, grow_to_end) = resolve_size_request(a, size_arg, cur_bytes);
    let shrinking = target.is_some_and(|t| t < cur_bytes);

    check_pv_intent(part, fstype, is_pv, shrinking, a.grow_lv).unwrap_or_else(|f| bail_fail(f));
    let is_block = src_ro.is_block;

    // 块设备在线路径：与 GPT 同一份实现，随表类型变的只有右侧空闲数
    #[cfg(target_os = "linux")]
    if let Some(code) = resize_online(a, src_ro, &ResizeTarget {
        part,
        ss,
        cur_bytes,
        is_pv,
        free_right_lba: free_right_msdos(&mbr, p, src_ro.size / ss),
        target,
        grow_to_end,
    }) {
        return code;
    }

    // 离线路径
    let mut src = open_target_for_write(a).unwrap_or_else(|f| bail_fail(f));
    // 锁下重取权威几何（与 GPT 分支同一原则）：只读阶段与取得独占权之间盘可以被别人
    // 改写（sfdisk/fdisk 不守本工具的锁），用锁前的 mbr/p/free 驱动写分支就是把过期
    // 决定写进盘——resize_mdos_entry 不复核邻接，过期的 free 没有第二道防线
    let mbr = table::parse_mbr(&src)
        .unwrap_or_else(|e| bail_fail(Fail::infra(format!("parse failed: {e}"))))
        .unwrap_or_else(|| bail_fail(Fail::refused("no MBR on target".to_string())));
    let p = match mbr.iter().find(|p| p.num == part) {
        Some(p) => p,
        None => bail_fail(Fail::refused(format!("partition {part} not found (MBR resize covers primary partitions 1..4 only)"))),
    };
    if p.is_container {
        bail_fail(Fail::refused("extended partition container cannot be resized (logical partitions are out of scope)".to_string()));
    }
    // SIZE 锚点按锁下的新分区尺寸重算；grow 标记是请求的形状，锁前锁后同值（见 GPT 分支）
    let cur_bytes = p.size_lba as u64 * ss;
    let (target, _) = resolve_size_request(a, size_arg, cur_bytes);
    let shrinking = target.is_some_and(|t| t < cur_bytes);
    check_pv_intent(part, fstype, is_pv, shrinking, a.grow_lv).unwrap_or_else(|f| bail_fail(f));
    let total_sectors = src.size / ss;
    if grow_to_end {
        let free = free_right_msdos(&mbr, p, total_sectors);
        let mut table_written = false;
        if free > 0 {
            let new_size_lba = p.size_lba as u64 + free;
            if new_size_lba > u32::MAX as u64 {
                bail_fail(Fail::refused(format!("new size {new_size_lba} sectors exceeds MBR 32-bit LBA limit")));
            }
            table::resize_mdos_entry(&mut src, part, new_size_lba as u32)
                .unwrap_or_else(|f| bail_fail(f));
            table_written = true;
        }
        // free == 0：分区已吃满右侧，表不动，FS 工具直接扩满现分区（与 GPT 路径同语义）
        mbr_grow_finish(a, &src, p, fstype, table_written, is_pv, is_block)
    } else {
        let Some(bytes) = target else { crate::args::usage() };
        if bytes < ss {
            bail_fail(Fail::refused(format!("size {bytes} < one sector ({ss})")));
        }
        let new_size_lba = bytes / ss;
        if new_size_lba > u32::MAX as u64 {
            bail_fail(Fail::refused(format!("size {new_size_lba} sectors exceeds MBR 32-bit LBA limit")));
        }
        if new_size_lba >= p.size_lba as u64 {
            // no-op（==）不触发缩容守卫链；扩（>）须右侧空闲足够
            let want = new_size_lba - p.size_lba as u64;
            if want > free_right_msdos(&mbr, p, total_sectors) {
                bail_fail(Fail::refused("not enough contiguous free space to the right (MBR resize cannot relocate blocking partitions)".to_string()));
            }
            if want > 0 {
                table::resize_mdos_entry(&mut src, part, new_size_lba as u32)
                    .unwrap_or_else(|f| bail_fail(f));
            }
            // 分区层做完 → 与 grow-to-end 同一条收尾
            mbr_grow_finish(a, &src, p, fstype, want > 0, is_pv, is_block)
        } else {
            // 缩：与 movepart GPT 路径同守卫链——FS 先缩成功才写表。
            // --no-fs 与缩容不可共存（分区末端会切进未缩的 FS 元数据），与 GPT 同判据
            if a.no_fs {
                bail_fail(Fail::refused("--no-fs cannot shrink: the filesystem has to be shrunk first, otherwise the new partition end would cut into filesystem metadata".to_string()));
            }
            // FS 收缩的前置检查与 GPT 路径同一处（fsops::check_shrink）：能否缩 / 类型是否
            // 认得 / 工具是否齐备只写一份
            fsops::check_shrink(fstype).unwrap_or_else(|e| bail_fail(Fail::from(e)));
            if let Some(min) = fsops::fs_min_bytes(&src, part, fstype)
                .unwrap_or_else(|e| bail_fail(Fail::from(e).context("min-size probe failed")))
                && bytes < min
            {
                bail_fail(Fail::refused(format!("target size {bytes} < minimum FS size {min} bytes (resize2fs -P)")));
            }
            fsops::shrink_fs(&src, part, fstype, new_size_lba * ss)
                .unwrap_or_else(|e| bail_fail(Fail::from(e).context("FS shrink failed")));
            table::resize_mdos_entry(&mut src, part, new_size_lba as u32)
                .unwrap_or_else(|f| bail_fail(f));
            let o = settle_layout(crate::outcome::Outcome::applied_with(Vec::new()), &src);
            finish_resize(a, o, is_pv, is_block, cur_bytes)
        }
    }
}

/// resize 的收尾：布局结果 →（PV 时才继续）LVM 链，取两者中更严重的退出码。
/// 未写入 → 直接返回；已写入但后置条件未全满足 → 非 PV 也直接返回，不打印成功字样
/// （否则与 PARTIAL 矛盾），PV 则仍需跑 pvresize/lvextend 链
fn finish_resize(a: &Args, o: crate::outcome::Outcome, is_pv: bool, is_block: bool, old_bytes: u64) -> u8 {
    if !o.is_applied() {
        return o.exit_code();
    }
    if !o.is_complete() && !is_pv {
        return o.exit_code();
    }
    resize_done(a, is_pv, is_block, old_bytes).max(o.exit_code())
}

/// 分区扩容收尾：从盘上表项重读实际新尺寸（搬移路径的扩容终点由计划决定，
/// 不能用操作前的预估）。PV 一律走 pvresize（--grow-lv 再传 LV）：块设备直接对
/// 分区节点；镜像经 losetup 临时映射该分区（attach → pvresize/lvextend → detach）。
fn resize_done(a: &Args, is_pv: bool, is_block: bool, old_bytes: u64) -> u8 {
    if !is_pv {
        println!("resized (verify with: diskedit info {})", a.target);
        return EXIT_OK;
    }
    #[cfg(target_os = "linux")]
    {
        // 分区号在 cmd_resize 入口就已解析（usage 兜底），到这里还没有是调用方的 bug：
        // 用 unwrap_or(0) 继续算，报出来的会是"分区从表里消失了"这种指向盘内容的假话
        let Some(part) = a.part else {
            bail_fail(Fail::infra("internal error: resize finished without a partition number".to_string()))
        };
        // 只打开一次：下面读"实际新尺寸"与给 LVM 链取分区节点用的是同一份盘上现状
        let src = open_target_ro(a).unwrap_or_else(|f| bail_fail(f));
        // 表项重读：GPT 与 MBR 的分派、表自身 ss 的换算都在 gpt_policy::partition_bytes 一处
        let new_bytes = match crate::gpt_policy::partition_bytes(&src, part) {
            Ok((_, len)) => len,
            // 分区号在 resize 入口已由锁下几何验证过一次，此时查不到即盘内容异常 →
            // 升级为 Infra，原因原样带上（into_io_error 正是"此处已越界"的取消息方式）
            Err(f) => bail_fail(Fail::infra(format!("post-resize: {}", crate::outcome::into_io_error(f)))),
        };
        let delta = new_bytes.saturating_sub(old_bytes);
        let r = if is_block {
            lvm_grow_chain(&part_dev_path(&a.target, part), delta, a.grow_lv, a.lv.as_deref())
        } else {
            // offset+sizelimit 映射出的 loop 设备 = 该分区的整块设备，PV 整设备语义下
            // pvresize/lvextend 直接可用，无需 -P partscan
            fsops::with_partition_device(&src, part, |pv| {
                lvm_grow_chain(pv, delta, a.grow_lv, a.lv.as_deref()).map_err(fsops::FsError::CommandFailed)
            })
            .map_err(|e| e.to_string())
        };
        match r {
            Ok(()) => EXIT_OK,
            // LVM 链失败 = 后置条件未满足：经 Outcome 的 Pending 通道报告并换算 PARTIAL，
            // 不在此手拼退出码（退出码映射的唯一处是 outcome）
            Err(e) => {
                let o = crate::outcome::Outcome::applied_with(vec![crate::outcome::Pending::new(
                    part,
                    crate::outcome::PendingKind::Other("lvm chain (pvresize/lvextend)"),
                    e,
                    "pvresize <part>; lvextend -l +<ext> -r <lv> (see pvresize(8))",
                )]);
                o.report();
                o.exit_code()
            }
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (is_block, old_bytes);
        // 分区号守卫与 Linux 分支同款（规范见上）：unwrap_or(0) 会把"没拿到分区号"
        // 报成指向盘内容的假话
        let Some(part) = a.part else {
            bail_fail(Fail::infra("internal error: resize finished without a partition number".to_string()))
        };
        let o = crate::outcome::Outcome::applied_with(vec![crate::outcome::Pending::new(
            part,
            crate::outcome::PendingKind::Other("lvm chain (pvresize/lvextend)"),
            "pvresize/lvextend require Linux — run pvresize on the partition manually",
            "",
        )]);
        o.report();
        o.exit_code()
    }
}

#[cfg(test)]
mod tests {
    use super::resolve_size_request;
    use crate::support::base_args;

    /// SIZE 请求解析：绝对/增量/百分比（先乘后除、下取整到 1MiB），锚定当前分区字节数
    #[test]
    fn size_request_resolution() {
        const MIB: u64 = 1024 * 1024;
        const G: u64 = 1024 * MIB;
        let a = base_args();
        assert_eq!(resolve_size_request(&a, Some("10G"), 1), (Some(10 * G), false));
        assert_eq!(resolve_size_request(&a, Some("+2G"), G), (Some(3 * G), false));
        assert_eq!(resolve_size_request(&a, Some("-500M"), G), (Some(G - 500 * MIB), false));
        // 百分比：raw = cur×10/100 后向下取整到 1MiB
        let raw = G * 10 / 100; // 107374182
        let delta = raw / MIB * MIB; // 106954752
        assert_eq!(resolve_size_request(&a, Some("+10%"), G), (Some(G + delta), false));
        assert_eq!(resolve_size_request(&a, Some("-10%"), G), (Some(G - delta), false));
        // 百分比增量不足 1MiB 时取整为 0（近似语义）
        assert_eq!(resolve_size_request(&a, Some("+1%"), 5 * MIB), (Some(5 * MIB), false));
        // "grow" 与 --grow-to-end 等价；--size 走绝对目标
        assert_eq!(resolve_size_request(&a, Some("grow"), 1), (None, true));
        let mut a2 = base_args();
        a2.size = Some(4096);
        assert_eq!(resolve_size_request(&a2, None, 1), (Some(4096), false));
        let mut a3 = base_args();
        a3.grow_to_end = true;
        assert_eq!(resolve_size_request(&a3, None, 1), (None, true));
    }
}