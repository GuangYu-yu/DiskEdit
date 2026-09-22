//! 低阶布局命令：new / add / del / resize-part / move / copy / create。

use crate::support::*;
use crate::args::Args;
use crate::{movepart, table};

pub(crate) const HELP_NEW: &str = r#"diskedit new <TARGET> [--table gpt|msdos] --yes

  Create a fresh partition table; overwrites any existing one. Default gpt."#;

pub(crate) const HELP_ADD: &str = r#"diskedit add <TARGET> --start LBA --end LBA [--name S] [--type T]

  Add a partition over [start, end] LBA. GPT --type takes a standard GUID
  text (default: Linux filesystem data); MBR --type takes 0xXX (default 0x83)."#;

pub(crate) const HELP_DELETE: &str = r#"diskedit delete <TARGET>:N --yes
diskedit del <TARGET>:N --yes

  Delete a partition entry (the data area is not wiped)."#;

pub(crate) const HELP_RESIZE_PART: &str = r#"diskedit resize-part <TARGET>:N --start LBA (--end LBA | --grow-to-end)

  Low-level grow/shrink/move in one: repartition [start, end] with data
  relocation. --grow-to-end pins end at last_usable_lba (fill semantics);
  --align mib|cyl|none and --chunk-size MiB control placement and copy
  granularity."#;

pub(crate) const HELP_MOVE: &str = r#"diskedit move <TARGET>:N --start <LBA|end>

  Move a partition, data follows (chunked copy, resumable via checkpoint).

  LOCATION:
    <LBA>     new start LBA (aligned per --align, default 1MiB)
    end       tail-pack to the last possible position"#;

pub(crate) const HELP_COPY: &str = r#"diskedit copy <TARGET>:N --start <LBA|end> [--name S]

  Byte-wise copy a partition to a new location; the source is untouched.
  --start end packs the copy against the end of the usable range.
  No resume: an interrupted copy is redone from the beginning on re-run
  (the destination bytes are simply rewritten)."#;

pub(crate) const HELP_CREATE: &str = r#"diskedit create <TARGET> [--size SIZE] [--name S] [--fs F]

  Create a partition in free space. With --size, the first aligned gap that
  fits; without, the largest aligned gap. --fs formats after creation.

  SIZE: absolute size (units b/k/m/g/t, 1024 base, e.g. 32M | 2G | bytes)"#;

pub(crate) fn cmd_new(a: &Args) -> u8 {
    // new 作用于整盘：`:N` 若被静默忽略，用户会误以为只动了某个分区
    if let Some(n) = a.part {
        bail_fail(Fail::refused(format!("`new` operates on the whole target — drop :{n}")));
    }
    if !a.yes {
        bail_fail(Fail::refused("`new` overwrites any existing partition table; pass --yes to confirm"));
    } else {
        let kind = a.table.unwrap_or(table::TableKind::Gpt);
        let mut src = open_target_for_write(a).unwrap_or_else(|f| bail_fail(f));
        let ss = src.sector_size;
        // 尺寸/几何判据先于一切写入：过不了就该报 refused（10），而不是等建表把它当
        // 失败的 io::Error 误报成 Failed（30 + "盘可能已改变"）。两族表各有自己的下限
        let preflight = match kind {
            table::TableKind::Gpt => table::create_gpt_preflight(src.size, ss),
            table::TableKind::Msdos => table::create_mbr_preflight(src.size, ss),
        };
        preflight.unwrap_or_else(|f| bail_fail(f));
        let r = match kind {
            table::TableKind::Gpt => table::create_gpt(&mut src, ss, None),
            table::TableKind::Msdos => table::create_mbr(&mut src),
        };
        match r {
            Ok(()) => table_write_done(&src, &format!("created {} table (verify with: diskedit info {})", kind.as_str(), a.target)),
            // 建表失败可能停在写完一半的中间态：此刻已不能声称"未写盘"，故经
            // `From<io::Error>` 落到 Failed（报 30 并附"盘可能已改变"的提示）
            Err(e) => bail_fail(Fail::from(e).context("new failed")),
        }
    }
}

pub(crate) fn cmd_add(a: &Args) -> u8 {
    // add 追加到最低空闲槽位，不接受 `:N` 指定槽位：静默忽略会让用户以为写进了 N 号槽
    if let Some(n) = a.part {
        bail_fail(Fail::refused(format!("`add` picks the first free slot itself — drop :{n}")));
    }
    let (Some(start), Some(end)) = (a.start, a.end) else { crate::args::usage() };
    let mut src = open_target_for_write(a).unwrap_or_else(|f| bail_fail(f));
    // 坐标系在几何计算前确定：GPT 条目按表头 ss 对齐，MBR 条目按容器 ss 对齐
    match table::load_gpt(&src) {
        Err(e) => bail_fail(Fail::infra(format!("label probe failed: {e}"))),
        Ok(Some(g)) => {
            let (start, end) = align_range(a, start, end, g.ss);
            let type_guid = match &a.type_guid {
                Some(s) => parse_guid(s).unwrap_or_else(|| bail_fail(Fail::refused(format!("invalid GUID {s:?} (expect standard text like C12A7328-F81F-11D2-BA4B-00A0C93EC93B, hyphens optional)")))),
                None => table::LINUX_FS_TYPE_GUID, // Linux filesystem data（util-linux GPT_DEFAULT_ENTRY_TYPE）
            };
            match crate::gpt_policy::add_entry(&mut src, start, end, a.name.as_deref().unwrap_or(""), type_guid) {
                Ok(num) => table_write_done(&src, &format!("added partition #{num} (verify with: diskedit info {})", a.target)),
                Err(f) => bail_fail(f),
            }
        }
        Ok(None) => match table::parse_mbr(&src) {
            Err(e) => bail_fail(Fail::infra(format!("label probe failed: {e}"))),
            Ok(Some(_)) => {
                let (start, end) = align_range(a, start, end, src.sector_size);
                let os_type = match &a.type_guid {
                    Some(s) => {
                        // 0x 前缀大小写不限（0X83 是常见写法）
                        let hex = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")).unwrap_or(s);
                        u8::from_str_radix(hex, 16).unwrap_or_else(|_| bail_fail(Fail::refused(format!("invalid MBR type {s:?} (expect 0xXX)"))))
                    }
                    // 默认 Linux 数据分区（util-linux pt-mbr.h MBR_LINUX_DATA_PARTITION）
                    None => 0x83, // Linux
                };
                match table::add_mdos_entry(&mut src, start, end, os_type) {
                    Ok(num) => table_write_done(&src, &format!("added partition #{num} (verify with: diskedit info {})", a.target)),
                    Err(f) => bail_fail(f),
                }
            }
            Ok(None) => bail_fail(Fail::refused("cannot add on none label — run `new` first".to_string())),
        },
    }
}

pub(crate) fn cmd_del(a: &Args) -> u8 {
    let Some(part) = a.part else { crate::args::usage() };
    if !a.yes {
        bail_fail(Fail::refused(format!("`del` removes partition entry {part}; pass --yes to confirm")));
    } else {
        let mut src = open_target_for_write(a).unwrap_or_else(|f| bail_fail(f));
        let r = match table::table_label(&src) {
            Ok(table::TableLabel::Gpt) => crate::gpt_policy::del_entry(&mut src, part),
            Ok(table::TableLabel::Mbr) => table::del_mdos_entry(&mut src, part),
            Ok(other) => bail_fail(Fail::refused(format!("cannot del on {other} label"))),
            Err(e) => bail_fail(Fail::infra(format!("label probe failed: {e}"))),
        };
        match r {
            Ok(()) => table_write_done(&src, &format!("deleted partition #{part} (verify with: diskedit info {})", a.target)),
            Err(f) => bail_fail(f),
        }
    }
}

pub(crate) fn cmd_resize_part(a: &Args) -> u8 {
    // `--start end` 是 move/copy 的"贴着可用区尾部打包"语法，resize-part 不挪位置，
    // 对应的语义是"扩到最后一个可用 LBA"。语义错与用法错共用 usage() 出口，
    // 用户只会看到一段用法而不知该改哪个旗标
    if a.start_end {
        bail_fail(Fail::refused(
            "`--start end` does not apply to resize-part (it does not relocate) — use --grow-to-end to extend the partition to the last usable LBA",
        ));
    }
    let (Some(part), Some(start)) = (a.part, a.start) else { crate::args::usage() };
    if a.grow_to_end && a.end.is_some() {
        bail_fail(Fail::refused("--end and --grow-to-end are mutually exclusive".to_string()));
    }
    let (mut src, _resumed) = open_target_for_data_move(a).unwrap_or_else(|f| bail_fail(f));
    // 坐标系在几何计算前确定：resize-part 仅支持 GPT，条目按表头 ss 对齐（可与容器 ss 不同）。
    // 几何走唯一构造点（条目重叠在此被拒），修复后的 last_usable 也由它给出
    let (g, repair) = match crate::gpt_policy::resolve_geometry(&src) {
        Ok(Some(v)) => v,
        Ok(None) => bail_fail(Fail::refused("no GPT on target".to_string())),
        Err(f) => bail_fail(f),
    };
    let end = if a.grow_to_end {
        // 吃满后方可用区（本工具语义）：扩到 last_usable_lba；
        // 后方有分区时由 resize_part 的重叠校验拒绝
        g.last_usable_lba()
    } else {
        let Some(e) = a.end else { crate::args::usage() };
        e
    };
    // --grow-to-end：end 钉死 last_usable 不做下取整（吃满语义），--align 仅作用于 start；
    // 常规路径 start 上取整、end 下取整
    let (start, end) = if a.grow_to_end {
        (align_start(a, start, g.ss), end)
    } else {
        align_range(a, start, end, g.ss)
    };
    let (chunk, mut logger) = chunk_logger(a, &src, g.header.disk_guid);
    let o = settle_layout(movepart::resize_part(&mut src, &g, repair, part, start, end, chunk, a.no_fs, &mut |m| logger.log(m)), &src);
    if o.is_complete() {
        println!("resize-part complete (verify with: diskedit info {})", a.target);
    }
    o.exit_code()
}

pub(crate) fn cmd_move(a: &Args) -> u8 {
    let (Some(part), start_opt) = (a.part, a.start) else { crate::args::usage() };
    if !a.start_end && start_opt.is_none() {
        crate::args::usage();
    }
    let (mut src, _resumed) = open_target_for_data_move(a).unwrap_or_else(|f| bail_fail(f));
    let (g, repair) = match crate::gpt_policy::resolve_geometry(&src) {
        Ok(Some(v)) => v,
        Ok(None) => bail_fail(Fail::refused("move requires a GPT target".to_string())),
        Err(f) => bail_fail(f),
    };
    let e = crate::gpt_policy::live_entry(&g, part).unwrap_or_else(|f| bail_fail(f));
    // 平移保持长度（本工具语义）：new_end = new_start + 原长度 - 1
    let len = e.ending_lba - e.starting_lba + 1;
    let start = if a.start_end {
        g.last_usable_lba()
            .checked_sub(len - 1)
            .unwrap_or_else(|| bail_fail(Fail::refused("partition longer than usable range".to_string())))
    } else {
        align_start(a, start_opt.unwrap(), g.ss)
    };
    // checked：start 来自 CLI 原始输入（--align none 时无上界），回绕会骗过 resize_part 的边界校验
    let end = start.checked_add(len - 1)
        .unwrap_or_else(|| bail_fail(Fail::refused("end LBA overflows address space".to_string())));
    let (chunk, mut logger) = chunk_logger(a, &src, g.header.disk_guid);
    let o = settle_layout(movepart::resize_part(&mut src, &g, repair, part, start, end, chunk, a.no_fs, &mut |m| logger.log(m)), &src);
    if o.is_complete() {
        println!("moved (verify with: diskedit info {})", a.target);
    }
    o.exit_code()
}

pub(crate) fn cmd_copy(a: &Args) -> u8 {
    let (Some(part), start_opt) = (a.part, a.start) else { crate::args::usage() };
    if !a.start_end && start_opt.is_none() {
        crate::args::usage();
    }
    // copy 不读不写 checkpoint（见 HELP_COPY 的 "No resume"）：它没有可续跑的东西，
    // 也就没有资格接管任何未收尾的现场。走严格打开——目标上有任何 active 现场即拒绝，
    // 绝不出现"copy 成功后把别人的 journal 当自己的清掉、而那份 ckpt 原样留在槽里"
    let mut src = open_target_for_write(a).unwrap_or_else(|f| bail_fail(f));
    // 坐标系在几何计算前确定：copy 仅支持 GPT，条目按表头 ss 对齐（可与容器 ss 不同）
    let (g, repair) = match crate::gpt_policy::resolve_geometry(&src) {
        Ok(Some(v)) => v,
        Ok(None) => bail_fail(Fail::refused("no GPT on target".to_string())),
        Err(f) => bail_fail(f),
    };
    let start = if a.start_end {
        let e = crate::gpt_policy::live_entry(&g, part).unwrap_or_else(|f| bail_fail(f));
        let len = e.ending_lba - e.starting_lba + 1;
        g.last_usable_lba()
            .checked_sub(len - 1)
            .unwrap_or_else(|| bail_fail(Fail::refused("partition longer than usable range".to_string())))
    } else {
        align_start(a, start_opt.unwrap(), g.ss)
    };
    let (chunk, mut logger) = chunk_logger(a, &src, g.header.disk_guid);
    match movepart::copy_part(&mut src, &g, repair, part, start, a.name.as_deref().unwrap_or(""), chunk, &mut |m| logger.log(m)) {
        Ok(num) => table_write_done(&src, &format!("copied to partition #{num} (verify with: diskedit info {})", a.target)),
        Err(f) => bail_fail(f),
    }
}

/// create 的一键入口：自动选空闲槽（--size 给定取首个装得下的，否则取最大者），1MiB 对齐。
/// 空闲区与 --size 的换算都按 LBA 所属表的扇区算（GPT = 表头 ss，可与容器 ss 不同）
pub(crate) fn cmd_create(a: &Args) -> u8 {
    // create 自选空闲槽：`:N` 静默忽略会让用户以为分区被放进了 N 号槽
    if let Some(n) = a.part {
        bail_fail(Fail::refused(format!("`create` picks a free gap itself — drop :{n}")));
    }
    let mut src = open_target_for_write(a).unwrap_or_else(|f| bail_fail(f));
    // 一次探测同时取"表类型 + 空闲区"：后面选槽写表要用的是同一个 label，
    // 再探一次等于重解析一遍表（且可能读到与前面不同的结果）。
    // 坐标系在几何计算前确定：want 与 aligned_gaps 的单位随分支而定。
    // 第四个值是该分支的 LBA 单位（字节/扇区），后面把 LBA 换成字节偏移时只能用它：
    // GPT 条目按**表头** ss 对齐，可与容器 ss 不同，用错单位会算出偏移差一倍的提示
    let (label, gaps, want, lba_bytes) = match table::table_label(&src) {
        Ok(table::TableLabel::Gpt) => {
            let (g, _repair) = match crate::gpt_policy::resolve_geometry(&src) {
                Ok(Some(v)) => v,
                Ok(None) => bail_fail(Fail::refused("no GPT on target — run `new` first".to_string())),
                Err(f) => bail_fail(f),
            };
            let unit = mib_in_sectors(g.ss);
            let want = a.size.map(|b| {
                if b < g.ss { bail_fail(Fail::refused(format!("size {b} < one sector ({})", g.ss))); }
                b / g.ss
            });
            let used: Vec<(u64, u64)> = g.entries.iter()
                .filter(|e| !(e.starting_lba == 0 && e.ending_lba == 0))
                .map(|e| (e.starting_lba, e.ending_lba)).collect();
            (table::TableLabel::Gpt, aligned_gaps(&used, g.first_usable_lba(), g.last_usable_lba(), unit), want, g.ss)
        }
        Ok(table::TableLabel::Mbr) => {
            let mbr = match table::parse_mbr(&src) {
                Ok(Some(m)) => m,
                Ok(None) => bail_fail(Fail::refused("no partition table on target — run `new` first".to_string())),
                Err(e) => bail_fail(Fail::infra(format!("parse failed: {e}"))),
            };
            let ss = src.sector_size;
            let unit = mib_in_sectors(ss);
            let want = a.size.map(|b| {
                if b < ss { bail_fail(Fail::refused(format!("size {b} < one sector ({ss})"))); }
                b / ss
            });
            let used: Vec<(u64, u64)> = mbr.iter().map(|p| (p.start_lba as u64, p.start_lba as u64 + p.size_lba as u64 - 1)).collect();
            let disk_last = table::container_last_lba(&src, ss);
            (table::TableLabel::Mbr, aligned_gaps(&used, unit, disk_last, unit), want, ss)
        }
        Ok(other) => bail_fail(Fail::refused(format!("cannot create on {other} label — run `new` first"))),
        Err(e) => bail_fail(Fail::infra(format!("label probe failed: {e}"))),
    };
    if gaps.is_empty() {
        bail_fail(Fail::refused("no free space (after 1MiB alignment)".to_string()));
    }
    let (start, end) = match want {
        Some(n) => match gaps.iter().find(|(s, e)| e - s + 1 >= n) {
            Some(&(s, e)) => (s, (s + n - 1).min(e)), // 区间端点落在间隙内
            None => bail_fail(Fail::refused(format!("no aligned gap fits {n} sectors; free gaps: {gaps:?}"))),
        },
        None => *gaps.iter().max_by_key(|(s, e)| e - s + 1).unwrap(),
    };
    // swap 声明落进类型 GUID：movepart 靠它识别 swap 挡路者（不搬数据、mkswap 重建）
    let is_swap = a.fs.as_deref() == Some("swap");
    let r = if label == table::TableLabel::Gpt {
        let guid = if is_swap { table::SWAP_TYPE_GUID } else { table::LINUX_FS_TYPE_GUID };
        crate::gpt_policy::add_entry(&mut src, start, end, a.name.as_deref().unwrap_or(""), guid)
    } else {
        table::add_mdos_entry(&mut src, start, end, if is_swap { 0x82 } else { 0x83 })
    };
    let num = match r {
        Ok(n) => n,
        // 表写入失败的性质由 table 层判定：事前校验拒绝(10) / 写盘后失败(30，
        // commit_gpt 是四段提交，失败时盘上可能停在中间态)。调用点无从区分
        Err(f) => bail_fail(f),
    };
    // 通知内核重读分区表，且必须**早于** mkfs：新分区此时只存在于盘上，内核还没为它
    // 建出分区节点，而 mkfs 走的 `with_partition_device` 是按 sysfs 拓扑定位节点的
    // （块设备上找不到就失败）。resync 失败不改变本次结论，只并入下面的报告
    let synced = kernel_resync(&src);
    // FS 步失败不算整体失败：分区已建成，只有 mkfs 这个后置条件没满足——
    // 走 Pending 通道报 PARTIAL（补救提示由 FS 层生成），不在这里手写出口码
    let mut pending = Vec::new();
    if let Some(fstype) = &a.fs {
        // 屏障的判据是"确实把写盘交给了外部工具"：类型不认得 / 工具不在时 mkfs
        // 起都没起来，盘上只有表写入——那条分区记录仍可整体 undo，不落屏障；
        // 工具真的跑起来了（可能写了半个 FS）才落屏障，undo 从此拒绝
        if crate::fsops::mkfs_capability(fstype).is_ok() {
            src.set_mutation(crate::dev::Mutation::Mkfs);
            if let Err(e) = src.mark_non_reversible() {
                bail_fail(Fail::failed(format!(
                    "partition #{num} was created, but the transaction state could not be persisted: {e} \
                     — verify with: diskedit info {}",
                    a.target
                )));
            }
        }
        if let Err(e) = crate::fsops::mkfs(&src, num, fstype) {
            pending.push(crate::outcome::Pending::new(
                num,
                crate::outcome::PendingKind::Fs,
                e.to_string(),
                crate::fsops::rescue_hint(fstype, &crate::dev::part_dev_hint(&src, num, start * lba_bytes)),
            ));
        }
    }
    let mut o = crate::outcome::Outcome::applied_with(pending);
    if !synced {
        o.mark_kernel_stale();
    }
    o.report();
    if o.is_complete() {
        println!("created partition #{num} at {start}..{end} (verify with: diskedit info {})", a.target);
    }
    o.exit_code()
}