//! 低阶布局命令：new / add / del / resize-part / move / copy / create。

use crate::support::*;
use crate::args::Args;
use crate::fsops::MkfsFs;
use crate::{movepart, table};

pub(crate) const HELP_NEW: &str = r#"diskedit new <TARGET> [--table gpt|msdos] --yes

  Create a fresh partition table; overwrites any existing one. Default gpt."#;

pub(crate) const HELP_ADD: &str = r#"diskedit add <TARGET> --start LBA --end LBA [--name S] [--type T] [--align A]

  Add a partition over [start, end] LBA. GPT --type takes a standard GUID
  text (default: Linux filesystem data); MBR --type takes 0xXX (default 0x83).
  LBA values are aligned per --align (default 1MiB: start rounds up, end
  rounds down); pass --align none to use them as given."#;

pub(crate) const HELP_DELETE: &str = r#"diskedit delete <TARGET>:N --yes
diskedit del <TARGET>:N --yes

  Delete a partition entry (the data area is not wiped)."#;

pub(crate) const HELP_RESIZE_PART: &str = r#"diskedit resize-part <TARGET>:N --start LBA (--end LBA | --grow-to-end)

  Low-level grow/shrink/move in one: repartition [start, end] with data
  relocation. --grow-to-end pins end at last_usable_lba (fill semantics);
  --align mib|cyl|none and --chunk-size MiB control placement and copy
  granularity. A grow also resizes the filesystem inside; one that cannot be
  grown here (unidentified, or not wired to a tool) needs an explicit --no-fs."#;

pub(crate) const HELP_MOVE: &str = r#"diskedit move <TARGET>:N --start <LBA|end>

  Move a partition, data follows (chunked copy, resumable via checkpoint).

  LOCATION:
    <LBA>     new start LBA (aligned per --align, default 1MiB)
    end       tail-pack to the last possible position

  --no-fs skips the filesystem step for the moved partition (a swap that
  the relocation rebuilds still runs — it belongs to the relocation)."#;

pub(crate) const HELP_COPY: &str = r#"diskedit copy <TARGET>:N --start <LBA|end> [--name S] [--clone]

  Byte-wise copy a partition to a new location; the source is untouched.
  --start end packs the copy against the end of the usable range.
  No resume: an interrupted copy cannot pick up where it left off. The
  slot it held stays busy until released with `diskedit abandon` — then
  re-run to rewrite the destination from the beginning.

  PARTUUID: the copy gets a freshly generated v4 GUID, so the two
  partitions stay distinguishable. --clone keeps the source PARTUUID
  instead — mount-by-PARTUUID then resolves to either partition."#;

pub(crate) const HELP_CREATE: &str = r#"diskedit create <TARGET> [--size SIZE] [--name S] [--fs F]

  Create a partition in free space. With --size, the first aligned gap that
  fits; without, the largest aligned gap. --fs formats after creation.

  SIZE: absolute size (units b/k/m/g/t, 1024 base, e.g. 32M | 2G | bytes)"#;

pub(crate) fn cmd_new(a: &Args) -> crate::outcome::Outcome {
    // new 作用于整盘：`:N` 若被静默忽略，用户会误以为只动了某个分区
    if let Some(n) = a.part {
        bail_fail(Fail::refused(format!("`new` operates on the whole target — drop :{n}")));
    }
    // 分区节点不是建表的合法载体：GPT 头与条目数组写在节点最前 16KiB，会直接覆盖
    // 该分区的数据（512B 扇区下 ext 超级块在 +1024）。建表只认整设备
    refuse_partition_node_target("new", &a.target);
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
        // GPT 的主数组与盘尾备份区都是表结构写入：旧表若把分区铺到任一段，这次写入
        // 就是清零分区数据。相交即按可证事实拒绝并点名；旧表读不出来时只告警
        // （new 本就是重建表的命令）
        if kind == table::TableKind::Gpt {
            match table::gpt_metadata_conflict(&src, ss) {
                Ok(Some(why)) => bail_fail(Fail::refused(why)),
                Ok(None) => {}
                Err(e) => eprintln!(
                    "warning: cannot check the existing table against the GPT metadata this new table writes ({e})"
                ),
            }
        }
        // 写表前的占用闸：目标上任一分区在用（镜像：任一绑定到它的 loop 在用）即拒绝，
        // 否则新表会与内核仍在使用的分区视图分叉
        if let Err(f) = crate::fsops::ensure_table_write_idle(&src) {
            bail_fail(f);
        }
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

/// `--name` 写的是 GPT 分区名；MBR 条目没有这一列。旗标白名单是命令级的，
/// 分支级的消费差异必须在分支里显式拒绝——静默丢弃等于命令做了旗标
/// 明确排除的事却报成功
fn refuse_name_on_msdos(a: &Args) {
    if a.name.is_some() {
        bail_fail(Fail::refused("`--name` sets a GPT partition name — MBR entries have none; drop --name".to_string()));
    }
}

pub(crate) fn cmd_add(a: &Args) -> crate::outcome::Outcome {
    // add 追加到最低空闲槽位，不接受 `:N` 指定槽位：静默忽略会让用户以为写进了 N 号槽
    if let Some(n) = a.part {
        bail_fail(Fail::refused(format!("`add` picks the first free slot itself — drop :{n}")));
    }
    // 分区节点不是合法载体：表条目写在整盘最前 16KiB，落到分区节点上会直接覆盖该
    // 分区的数据——判据与文案见 refuse_partition_node_target
    refuse_partition_node_target("add", &a.target);
    // `--start end` 是 move/copy 的"贴着可用区尾部打包"语法；add 不挪位置，
    // 与 resize-part 同一条针对性拒绝，而非让它落进下面"缺 --start"的通用报错
    if a.start_end {
        bail_fail(Fail::refused(
            "`--start end` does not apply to add (it is move/copy syntax for packing against the last usable LBA) — pass explicit --start and --end LBAs",
        ));
    }
    let (Some(start), Some(end)) = (a.start, a.end) else {
        bail_fail(Fail::refused("`add` needs both --start and --end".to_string()));
    };
    let mut src = open_target_for_write(a).unwrap_or_else(|f| bail_fail(f));
    // 写表前的占用闸（见 fsops::ensure_table_write_idle）。放在几何判定之前：
    // 占用是"目标现在能不能写"的事实，与本条命令的参数是否合法无关
    if let Err(f) = crate::fsops::ensure_table_write_idle(&src) {
        bail_fail(f);
    }
    // 坐标系在几何计算前确定：GPT 条目按表头 ss 对齐，MBR 条目按容器 ss 对齐
    match table::table_label(&src) {
        Err(e) => bail_fail(Fail::infra(format!("label probe failed: {e}"))),
        // 判型只用 table_label 一个来源（与 del/create 同口径）：load_gpt 单独判会把
        // "保护 MBR 受损、条目数组可能完好"的 GPT 盘当作无表盘降级到 msdos 分支
        Ok(table::TableLabel::GptDamaged) => bail_fail(Fail::refused(
            "the GPT on this target is damaged — repair it first (or recover it manually); \
             `new` would overwrite a table whose entries may still be recoverable"
                .to_string(),
        )),
        Ok(table::TableLabel::Gpt) => {
            // 判型刚由 load_gpt 成功得出；这里再取一次几何只有读盘，没有第二个判据
            let g = match table::load_gpt(&src) {
                Ok(Some(g)) => g,
                Ok(None) => bail_fail(Fail::infra("the GPT disappeared between reads — retry".to_string())),
                Err(e) => bail_fail(Fail::infra(format!("label probe failed: {e}"))),
            };
            let (start, end) = align_range(a, start, end, g.ss);
            let type_guid = match &a.type_guid {
                Some(s) => parse_guid(s).unwrap_or_else(|| bail_fail(Fail::refused(format!("invalid GUID {s:?} (expect standard text like C12A7328-F81F-11D2-BA4B-00A0C93EC93B, hyphens optional)")))),
                None => table::LINUX_FS_TYPE_GUID, // Linux filesystem data（util-linux GPT_DEFAULT_ENTRY_TYPE）
            };
            // 名字守卫在消费点：GPT 分支是 --name 的唯一落盘处（MBR 分支已拒绝 --name）
            if let Some(n) = &a.name { crate::support::check_gpt_name_len(n); }
            match crate::gpt_policy::add_entry(&mut src, start, end, a.name.as_deref().unwrap_or(""), type_guid) {
                Ok(num) => table_write_done(&src, &format!("added partition #{num} (verify with: diskedit info {})", a.target)),
                Err(f) => bail_fail(f),
            }
        }
        Ok(table::TableLabel::Mbr) => match table::parse_mbr(&src) {
            Err(e) => bail_fail(Fail::infra(format!("label probe failed: {e}"))),
            Ok(Some(_)) => {
                refuse_name_on_msdos(a);
                let (start, end) = align_range(a, start, end, src.sector_size);
                let os_type = match &a.type_guid {
                    Some(s) => parse_os_type(s).unwrap_or_else(|| bail_fail(Fail::refused(format!("invalid MBR type {s:?} (expect 0xXX)")))),
                    // 默认 Linux 数据分区（util-linux pt-mbr.h MBR_LINUX_DATA_PARTITION）
                    None => 0x83, // Linux
                };
                match table::add_mdos_entry(&mut src, start, end, os_type) {
                    Ok(num) => table_write_done(&src, &format!("added partition #{num} (verify with: diskedit info {})", a.target)),
                    Err(f) => bail_fail(f),
                }
            }
            Ok(None) => bail_fail(Fail::infra("the MBR disappeared between reads — retry".to_string())),
        },
        Ok(table::TableLabel::None) => bail_fail(Fail::refused("cannot add on none label — run `new` first".to_string())),
    }
}

pub(crate) fn cmd_del(a: &Args) -> crate::outcome::Outcome {
    let Some(pref) = a.part else { crate::args::usage() };
    // 载体必须是整盘：del 清零的是节点 LBA0 里的条目槽，而分区节点上被判成 msdos 的
    // 可能只是文件系统引导扇区——FAT/NTFS 的引导扇区末尾同为 0x55AA，MBR 判据只认
    // 这个签名。判据与 new/add/create 同源
    refuse_partition_node_target("del", &a.target);
    if !a.yes {
        bail_fail(Fail::refused("`del` removes the target partition entry; pass --yes to confirm"));
    } else {
        let mut src = open_target_for_write(a).unwrap_or_else(|f| bail_fail(f));
        // 写表前的占用闸：删掉一个仍被内核使用的分区，会让盘上表与挂载视图分叉
        if let Err(f) = crate::fsops::ensure_table_write_idle(&src) {
            bail_fail(f);
        }
        // 锁下快照
        let part = crate::gpt_policy::resolve_part(&src, pref).unwrap_or_else(|f| bail_fail(f));
        let r = match table::table_label(&src) {
            Ok(table::TableLabel::Gpt) => crate::gpt_policy::del_entry(&mut src, part),
            Ok(table::TableLabel::Mbr) => table::del_mdos_entry(&mut src, part),
            Ok(other) => bail_fail(Fail::refused(format!("`del` needs a GPT or MBR target (label: {other})"))),
            Err(e) => bail_fail(Fail::infra(format!("label probe failed: {e}"))),
        };
        match r {
            Ok(()) => table_write_done(&src, &format!("deleted partition #{part} (verify with: diskedit info {})", a.target)),
            Err(f) => bail_fail(f),
        }
    }
}

pub(crate) fn cmd_resize_part(a: &Args) -> crate::outcome::Outcome {
    // `--start end` 是 move/copy 的"贴着可用区尾部打包"语法，resize-part 不挪位置，
    // 对应的语义是"扩到最后一个可用 LBA"。语义错与用法错共用 usage() 出口，
    // 用户只会看到一段用法而不知该改哪个旗标
    if a.start_end {
        bail_fail(Fail::refused(
            "`--start end` does not apply to resize-part (it does not relocate) — use --grow-to-end to extend the partition to the last usable LBA",
        ));
    }
    let (Some(pref), Some(start)) = (a.part, a.start) else {
        bail_fail(Fail::refused("`resize-part` needs :N and --start".to_string()));
    };
    if a.grow_to_end && a.end.is_some() {
        bail_fail(Fail::refused("--end and --grow-to-end are mutually exclusive".to_string()));
    }
    // 载体必须是整盘：条目按节点 LBA0 的偏移读写（判据与 new/add/create 同源）
    refuse_partition_node_target("resize-part", &a.target);
    let (mut src, _resumed) = open_target_for_data_move(a).unwrap_or_else(|f| bail_fail(f));
    // 坐标系在几何计算前确定：resize-part 仅支持 GPT，条目按表头 ss 对齐（可与容器 ss 不同）。
    // 几何走唯一构造点（条目重叠在此被拒），修复后的 last_usable 也由它给出
    let (g, repair) = crate::gpt_policy::require_gpt_geometry(&src, "resize-part").unwrap_or_else(|f| bail_fail(f));
    // 锁下快照
    let part = crate::gpt_policy::resolve_part_in(crate::gpt_policy::TableEntries::Gpt(g.entries()), pref)
        .unwrap_or_else(|f| bail_fail(f));
    let end = if a.grow_to_end {
        // 吃满后方可用区（本工具语义）：扩到 last_usable_lba；
        // 后方有分区时由 resize_part 的重叠校验拒绝
        g.last_usable_lba()
    } else {
        let Some(e) = a.end else {
            bail_fail(Fail::refused("`resize-part` needs --end (or --grow-to-end)".to_string()));
        };
        e
    };
    // --grow-to-end：end 钉死 last_usable 不做下取整（吃满语义），--align 仅作用于 start；
    // 常规路径 start 上取整、end 下取整
    let (start, end) = if a.grow_to_end {
        (align_start(a, start, g.sector_size()), end)
    } else {
        align_range(a, start, end, g.sector_size())
    };
    let (chunk, mut logger) = chunk_logger(a, &src, g.disk_guid());
    let o = settle_layout(movepart::resize_part(&mut src, &g, repair, part, start, end, chunk, a.no_fs, &mut |m| logger.log(m)), &src);
    if o.is_complete() {
        println!("resize-part complete (verify with: diskedit info {})", a.target);
    }
    o
}

/// `--start end` 把起点钉在 `last_usable_lba - len + 1`，是"贴尾打包"而非"挑一个对齐
/// 位置"；此时再给 `--align` 没有被消费的余地，静默忽略会让用户以为对齐生效。旗标白名单
/// 是命令级的，这个分支级的消费差异只能在分支里拒绝。判据看 `seen`：`--align` 有默认值
/// （mib），只看字段分不出"没给"与"给了 mib"
fn refuse_align_on_start_end(a: &Args) {
    if a.start_end && a.seen.contains(&"--align") {
        bail_fail(Fail::refused(
            "`--align` does not apply to `--start end` — the start is pinned to the last usable position; drop --align or pass an explicit start LBA",
        ));
    }
}

pub(crate) fn cmd_move(a: &Args) -> crate::outcome::Outcome {
    // 两种给起点的方式各写一条会都留在 Args 里：start_end 恒优先，数值被静默丢弃
    if a.start_end && a.start.is_some() {
        bail_fail(Fail::refused(
            "`--start end` and a numeric --start are mutually exclusive — pass one of them".to_string(),
        ));
    }
    let (Some(pref), start_opt) = (a.part, a.start) else { crate::args::usage() };
    if !a.start_end && start_opt.is_none() {
        crate::args::usage();
    }
    refuse_align_on_start_end(a);
    // 载体必须是整盘：条目按节点 LBA0 的偏移读写（判据与 new/add/create 同源）
    refuse_partition_node_target("move", &a.target);
    let (mut src, _resumed) = open_target_for_data_move(a).unwrap_or_else(|f| bail_fail(f));
    let (g, repair) = crate::gpt_policy::require_gpt_geometry(&src, "move").unwrap_or_else(|f| bail_fail(f));
    // 锁下快照
    let part = crate::gpt_policy::resolve_part_in(crate::gpt_policy::TableEntries::Gpt(g.entries()), pref)
        .unwrap_or_else(|f| bail_fail(f));
    let e = crate::gpt_policy::live_entry(&g, part).unwrap_or_else(|f| bail_fail(f));
    // 平移保持长度（本工具语义）：new_end = new_start + 原长度 - 1
    let len = e.ending_lba - e.starting_lba + 1;
    let start = if a.start_end {
        g.last_usable_lba()
            .checked_sub(len - 1)
            .unwrap_or_else(|| bail_fail(Fail::refused("partition longer than usable range".to_string())))
    } else {
        align_start(a, start_opt.unwrap(), g.sector_size())
    };
    // checked：start 来自 CLI 原始输入（--align none 时无上界），回绕会骗过 resize_part 的边界校验
    let end = start.checked_add(len - 1)
        .unwrap_or_else(|| bail_fail(Fail::refused("end LBA overflows address space".to_string())));
    let (chunk, mut logger) = chunk_logger(a, &src, g.disk_guid());
    let o = settle_layout(movepart::resize_part(&mut src, &g, repair, part, start, end, chunk, a.no_fs, &mut |m| logger.log(m)), &src);
    if o.is_complete() {
        println!("moved (verify with: diskedit info {})", a.target);
    }
    o
}

pub(crate) fn cmd_copy(a: &Args) -> crate::outcome::Outcome {
    if a.start_end && a.start.is_some() {
        bail_fail(Fail::refused(
            "`--start end` and a numeric --start are mutually exclusive — pass one of them".to_string(),
        ));
    }
    let (Some(pref), start_opt) = (a.part, a.start) else { crate::args::usage() };
    if !a.start_end && start_opt.is_none() {
        crate::args::usage();
    }
    refuse_align_on_start_end(a);
    // copy 不读不写 checkpoint（见 HELP_COPY 的 "No resume"）：它没有可续跑的东西，
    // 也就没有资格接管任何未收尾的现场。走严格打开——目标上有任何 active 现场即拒绝，
    // 绝不出现"copy 成功后把别人的 journal 当自己的清掉、而那份 ckpt 原样留在槽里"
    // 载体必须是整盘：条目按节点 LBA0 的偏移读写（判据与 new/add/create 同源）
    refuse_partition_node_target("copy", &a.target);
    let mut src = open_target_for_write(a).unwrap_or_else(|f| bail_fail(f));
    // 坐标系在几何计算前确定：copy 仅支持 GPT，条目按表头 ss 对齐（可与容器 ss 不同）
    let (g, repair) = crate::gpt_policy::require_gpt_geometry(&src, "copy").unwrap_or_else(|f| bail_fail(f));
    // 锁下快照
    let part = crate::gpt_policy::resolve_part_in(crate::gpt_policy::TableEntries::Gpt(g.entries()), pref)
        .unwrap_or_else(|f| bail_fail(f));
    let start = if a.start_end {
        let e = crate::gpt_policy::live_entry(&g, part).unwrap_or_else(|f| bail_fail(f));
        let len = e.ending_lba - e.starting_lba + 1;
        g.last_usable_lba()
            .checked_sub(len - 1)
            .unwrap_or_else(|| bail_fail(Fail::refused("partition longer than usable range".to_string())))
    } else {
        align_start(a, start_opt.unwrap(), g.sector_size())
    };
    let (chunk, mut logger) = chunk_logger(a, &src, g.disk_guid());
    // 名字守卫在消费点：copy 的落盘名字与 add 同一个 36 码元字段
    if let Some(n) = &a.name { crate::support::check_gpt_name_len(n); }
    match movepart::copy_part(&mut src, &g, repair, part, start, a.name.as_deref().unwrap_or(""), a.clone, chunk, &mut |m| logger.log(m)) {
        Ok(num) => table_write_done(&src, &format!("copied to partition #{num} (verify with: diskedit info {})", a.target)),
        Err(f) => bail_fail(f),
    }
}

/// create 的一键入口：自动选空闲槽（--size 给定取首个装得下的，否则取最大者），1MiB 对齐。
/// 空闲区与 --size 的换算都按 LBA 所属表的扇区算（GPT = 表头 ss，可与容器 ss 不同）
pub(crate) fn cmd_create(a: &Args) -> crate::outcome::Outcome {
    // create 自选空闲槽：`:N` 静默忽略会让用户以为分区被放进了 N 号槽
    if let Some(n) = a.part {
        bail_fail(Fail::refused(format!("`create` picks a free gap itself — drop :{n}")));
    }
    // FS 类型先问再开事务（判据与出口码同 `mkfs`）：未接线的类型是请求本身的错误，
    // 用户改不动它——事前拒绝，此刻什么都没写。工具缺失不在此拦：分区照建，mkfs
    // 走后置条件通道记 PARTIAL（装上工具后按提示补做）
    let fs_kind: Option<MkfsFs> = match &a.fs {
        Some(name) => {
            let k = match crate::fsops::parse_fstype(name) {
                Ok(k) => k,
                Err(e) => bail_fail(Fail::from(e)),
            };
            if let Err(e) = crate::fsops::mkfs_capability(k)
                && matches!(&e, crate::fsops::FsError::UnsupportedFs(_))
            {
                bail_fail(Fail::from(e));
            }
            Some(k)
        }
        None => None,
    };
    // 分区节点不是合法载体（同 add：表条目写在整盘最前 16KiB）
    refuse_partition_node_target("create", &a.target);
    let mut src = open_target_for_write(a).unwrap_or_else(|f| bail_fail(f));
    // 写表前的占用闸（与 add/del 同判据）。放在几何判定之前：占用是"目标现在能不能写"
    // 的事实，与本条命令的参数是否合法无关
    if let Err(f) = crate::fsops::ensure_table_write_idle(&src) {
        bail_fail(f);
    }
    // 盘型由 table_label 判定一次，label 决定写入哪个表族的槽；空闲区在同一分支里由
    // 该表族自己的几何算出（GPT 用表头 ss 与可用区，MBR 用容器 ss 与盘尾）。写表函数
    // 内部还会解析一次表做自身校验——本命令持锁，盘面在窗口内不会变
    // 坐标系在几何计算前确定：want 与 aligned_gaps 的单位随分支而定。
    // 第四个值是该分支的 LBA 单位（字节/扇区），后面把 LBA 换成字节偏移时只能用它：
    // GPT 条目按**表头** ss 对齐，可与容器 ss 不同，用错单位会算出偏移差一倍的提示
    let (label, gaps, want, lba_bytes) = match table::table_label(&src) {
        Ok(table::TableLabel::Gpt) => {
            let (g, _repair) = crate::gpt_policy::require_gpt_geometry(&src, "create").unwrap_or_else(|f| bail_fail(f));
            let unit = mib_in_sectors(g.sector_size());
            let want = a.size.map(|b| {
                if b < g.sector_size() { bail_fail(Fail::refused(format!("size {b} < one sector ({})", g.sector_size()))); }
                crate::support::note_sector_truncation("creating", b, g.sector_size());
                b / g.sector_size()
            });
            let used: Vec<(u64, u64)> = g.entries().iter()
                .filter(|e| !(e.starting_lba == 0 && e.ending_lba == 0))
                .map(|e| (e.starting_lba, e.ending_lba)).collect();
            (table::TableLabel::Gpt, aligned_gaps(&used, g.first_usable_lba(), g.last_usable_lba(), unit), want, g.sector_size())
        }
        Ok(table::TableLabel::Mbr) => {
            refuse_name_on_msdos(a);
            let mbr = match table::parse_mbr(&src) {
                Ok(Some(m)) => m,
                Ok(None) => bail_fail(Fail::refused("no partition table on target — run `new` first".to_string())),
                Err(e) => bail_fail(Fail::infra(format!("parse failed: {e}"))),
            };
            let ss = src.sector_size;
            let unit = mib_in_sectors(ss);
            let want = a.size.map(|b| {
                if b < ss { bail_fail(Fail::refused(format!("size {b} < one sector ({ss})"))); }
                crate::support::note_sector_truncation("creating", b, ss);
                b / ss
            });
            let used: Vec<(u64, u64)> = mbr.iter().map(|p| (p.start_lba as u64, p.start_lba as u64 + p.size_lba as u64 - 1)).collect();
            let disk_last = table::container_last_lba(&src, ss);
            (table::TableLabel::Mbr, aligned_gaps(&used, unit, disk_last, unit), want, ss)
        }
        // 可辨但受损的盘不得给出会覆盖它的下一步：new 会重写头与条目数组
        Ok(table::TableLabel::GptDamaged) => bail_fail(Fail::refused(
            "the GPT on this target is damaged — repair it first (or recover it manually); `new` would overwrite it".to_string(),
        )),
        Ok(table::TableLabel::None) => bail_fail(Fail::refused("cannot create on none label — run `new` first".to_string())),
        Err(e) => bail_fail(Fail::infra(format!("label probe failed: {e}"))),
    };
    if gaps.is_empty() {
        bail_fail(Fail::refused("no free space (after 1MiB alignment)".to_string()));
    }
    // 间隙跨度全程 checked（与下面 s+n-1 同一原则）：回绕的跨度会把装不下的间隙判成装得下
    let span = |(s, e): &(u64, u64)| -> u64 {
        e.checked_sub(*s).and_then(|d| d.checked_add(1))
            .unwrap_or_else(|| bail_fail(Fail::refused("gap span overflows the LBA range (corrupted table)".to_string())))
    };
    let (start, end) = match want {
        Some(n) => match gaps.iter().find(|g| span(g) >= n) {
            // n 来自 --size（用户可控）：find 的 span≥n 已保证 s+n-1 ≤ e，checked 把
            // 这层非局部依赖显式化——回绕不依赖 find 的承诺
            Some(&(s, e)) => {
                let last = s.checked_add(n).and_then(|x| x.checked_sub(1))
                    .unwrap_or_else(|| bail_fail(Fail::refused(format!("size {n} sectors overflows the LBA range"))));
                (s, last.min(e)) // 区间端点落在间隙内
            }
            None => bail_fail(Fail::refused(format!("no aligned gap fits {n} sectors; free gaps: {gaps:?}"))),
        },
        None => *gaps.iter().max_by_key(|g| span(g)).unwrap(),
    };
    // swap 声明落进类型 GUID：movepart 靠它识别 swap 挡路者（不搬数据、mkswap 重建）
    let is_swap = fs_kind == Some(MkfsFs::Swap);
    let r = if label == table::TableLabel::Gpt {
        // 名字守卫在消费点（同 add）：MBR 分支没有名字列
        if let Some(n) = &a.name { crate::support::check_gpt_name_len(n); }
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
    if let Some(fstype) = fs_kind {
        // 屏障在 fsops::mkfs 内、紧贴第一次擦除：工具缺失或设备建不起来时它一个字节都
        // 没写，盘上只有表写入——那条分区记录仍可整体 undo，不该落屏障。放在这里则
        // 连"工具起不来"也会把目标锁进只能 abandon 的现场
        if let Err(e) = crate::fsops::mkfs(&mut src, num, fstype) {
            // create 没有"原来的 UUID"可保：新分区上的 swap 提示不带 --uuid 占位符
            let dev_hint = crate::dev::part_dev_hint(&src, num, start * lba_bytes);
            let hint = if fstype == MkfsFs::Swap {
                format!("mkswap {dev_hint}")
            } else {
                crate::fsops::rescue_hint(Some(fstype.as_fs_kind()), &dev_hint)
            };
            pending.push(crate::outcome::Pending::new(
                num,
                crate::outcome::PendingKind::Fs,
                e.to_string(),
                hint,
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
    o
}