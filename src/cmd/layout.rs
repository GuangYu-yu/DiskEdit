//! 低阶布局命令：new / add / delete / resize-part / move / copy。

use crate::cli::{
    AddRequest, CopyRequest, DeleteRequest, MoveRequest, NewRequest,
    ResizePartRequest, StartSelector,
};
use crate::dev::WriteSource;
use crate::support::*;
use crate::{movepart, table};

use super::confirm;

pub(crate) const DETAILS_NEW: &str = r#"  Create a fresh partition table; overwrites any existing one. Default gpt.
  Drawing a table writes the entry arrays at the front of the disk and, for
  gpt, a backup copy in the last sectors; for mbr, residual GPT header
  signatures are cleared — bytes an existing partition may hold. Such a
  collision is refused and named; --overwrite-data accepts overwriting those
  bytes (--yes alone only confirms the rebuild)."#;

pub(crate) const DETAILS_ADD: &str = r#"  Add a partition that holds SIZE. Without --start the entry goes in the first
  1 MiB-aligned free gap that fits; --align changes the alignment (none = the
  LBAs as given). With --start it begins at that LBA as given and is refused
  if it does not fit. SIZE is an absolute number of bytes (K/M/G/T
  suffixes allowed); relative forms (+32M, -32M, 32%) are refused — a new
  partition has no previous size to be relative to. Without --part-number the
  entry takes the first unused slot. On GPT the type defaults to Linux
  filesystem data; on MBR --type-guid takes 0xNN and defaults to 0x83 (Linux)."#;

pub(crate) const DETAILS_DELETE: &str = r#"  Delete a partition entry (the data area is not wiped)."#;

pub(crate) const DETAILS_RESIZE_PART: &str = r#"  Low-level grow/shrink in place: repartition the end without moving the
  partition. The start stays at the partition's current starting LBA;
  relocating it is `diskedit move`. --grow-to-end pins end at
  last_usable_lba (fill semantics).
  A grow also resizes the filesystem inside.

  CONFIRMATION: --yes is required. An end below the current end shrinks the
  partition together with its filesystem; a shrink that gets interrupted
  cannot be rolled back with `undo`."#;

pub(crate) const DETAILS_MOVE: &str = r#"  Move a partition, data follows (chunked copy, resumable from its recovery record).

  LOCATION:
    <LBA>     new start LBA (aligned per --align; default none = the LBA as given)
    end       tail-pack to the last possible position

  --no-fs has no effect: a move keeps the partition's size, so there is no
  filesystem step to skip.

  A swap partition is relocated by committing its entry and rebuilding the area
  at the new position (mkswap); its contents are not copied.

  CONFIRMATION: required. The destination range is overwritten as-is and an
  interrupted relocation cannot be rolled back with `undo`; pass --yes."#;

pub(crate) const DETAILS_COPY: &str = r#"  Byte-wise copy a partition to a new location; the source is untouched.
  --start end packs the copy against the end of the usable range.
  No resume: an interrupted copy cannot pick up where it left off. It leaves
  a non-rollbackable journal on the target, so write commands refuse until
  that is released with `diskedit abandon` — re-run then to rewrite the
  destination from the beginning.

  PARTUUID: the copy gets a freshly generated v4 GUID, so the two
  partitions stay distinguishable. --keep-partuuid reuses the source
  GUID instead — mount-by-PARTUUID then resolves to either partition.

  CONFIRMATION: required. The destination range is overwritten with the
  source bytes and the journal records the write as non-rollbackable —
  `undo` cannot restore what was there before; pass --yes."#;

pub(crate) fn cmd_new(a: &NewRequest) -> crate::outcome::RunOutcome {
    // new 作用于整盘：`:N` 若被静默忽略，用户会误以为只动了某个分区
    if let Some(n) = a.part {
        bail_fail(RunOutcome::refused(format!("`new` operates on the whole target — drop :{n}")));
    }
    // 分区节点不是建表的合法载体：GPT 头与条目数组写在节点最前 16KiB，会直接覆盖
    // 该分区的数据（512B 扇区下 ext 超级块在 +1024）。建表只认整设备
    refuse_partition_node_target("new", &a.target);
    let kind = a.table.unwrap_or(table::TableKind::Gpt);
    let (mut src, resumed) =
        open_record_resumable(a, crate::recovery::OpKind::New).unwrap_or_else(|f| bail_fail(f));
    let ss = src.sector_size;
    // 续跑只认同一份请求：表种类、--overwrite-data 与扇区大小决定写入内容
    let digest = crate::recovery::PlanDigest::of_request_fields(
        crate::recovery::OpKind::New,
        &[match kind { table::TableKind::Gpt => 1, table::TableKind::Mbr => 2 }, u64::from(a.overwrite_data), ss],
        &[],
    );
    // 覆盖门是新建记录的决策：同一请求的记录已在，说明首跑已经签过字；此刻盘上的非零
    // 与可识别表有相当一部分就是首跑写下的，不据此再要求一次 --overwrite-data
    let resume_confirmed = resumed
        && src.record_journal().is_some_and(|j| j.plan_digest() == digest);
    // 尺寸/几何判据先于一切写入：过不了就该报 refused（3），而不是等建表把它当
    // 失败的 io::Error 误报成 Partial（5 + "盘可能已改变"）。两族表各有自己的下限
    let preflight = match kind {
        table::TableKind::Gpt => table::create_gpt_preflight(src.size, ss),
        table::TableKind::Mbr => table::create_mbr_preflight(src.size, ss),
    };
    preflight.unwrap_or_else(|f| bail_fail(f));
    // 写前基准：三段（首 LBA、LBA1、末 LBA）摘要。写入口拿它复核"盘面在这中间没变过"
    let stamp = table::table_stamp(&src, ss).unwrap_or_else(|f| bail_fail(f));
    // 计划期内容：GPT 是纯计算（空条目表 + 新盘 GUID）；MBR 的 LBA0 原文只由快照供给
    let content = match kind {
        table::TableKind::Gpt => match table::new_gpt_content(&src, None) {
            Ok(c) => Some(c),
            Err(f) => bail_fail(f),
        },
        table::TableKind::Mbr => None,
    };
    // 单快照：识别、冲突、全零判定与内容构造读的都是这一份
    let snapshot = match &content {
        Some(c) => crate::assembly::take_table_snapshot(
            &src,
            &crate::assembly::TableContent::Gpt {
                kind: table::TableWriteKind::Replace,
                content: c,
                protective_mbr: None,
            },
        ),
        None => crate::assembly::take_table_snapshot(&src, &crate::assembly::TableContent::NewMbr),
    }
    .unwrap_or_else(|f| bail_fail(f));
    // 覆盖门：三态探测为「空」才不用 --overwrite-data。可识别表与非空但读不出表
    // 都不算空——前者是重建已有表，后者无法证明这些字节只属于表结构区
    let recognized = match table::recognized_table(&src) {
        Ok(v) => v,
        Err(e) => bail_fail(RunOutcome::refused(format!(
            "cannot identify the existing partition table ({e}) — the table sectors cannot be proven \
             blank, so the rebuild is refused"
        ))),
    };
    let probe = table::TableProbe::classify(
        recognized,
        snapshot.head_tail_all_zero(ss, table::container_last_lba(&src, ss)),
    );
    if probe != table::TableProbe::Empty && !a.overwrite_data && !resume_confirmed {
        bail_fail(RunOutcome::refused(format!(
            "the table sectors already hold data (probe={}); \
             regions {} would be overwritten — pass --overwrite-data to accept it",
            probe.as_str(),
            region_list(&snapshot.regions)
        )));
    }
    // 新表写入落在旧分区数据里的部分，必须另有明确授权：`--yes` 只确认"重建表"。
    // 探测读不出旧表时同样不放行——无法证明写入只落在表结构区，就不是"确认无冲突"。
    // 候选扇区大小的签名位置由这道门探测，它们不在本次写入范围内，不会被静默覆盖
    let conflict = table::new_table_conflict(&src, ss, kind);
    match conflict {
        Ok(Some(why)) if !a.overwrite_data => bail_fail(RunOutcome::refused(format!(
            "{why} — pass --overwrite-data to accept overwriting those bytes"
        ))),
        Ok(Some(_)) => {}
        Ok(None) => {}
        Err(e) if !a.overwrite_data => bail_fail(RunOutcome::refused(format!(
            "cannot check the existing partition table against the bytes this new table writes ({e}) — \
             pass --overwrite-data to proceed without that check"
        ))),
        Err(e) => eprintln!(
            "warning: cannot check the existing table against the bytes this new table writes ({e})"
        ),
    }
    // 写前复核：三段摘要与入口那份不等即不写。首跑 3、有活跃记录 5、
    // 重读 I/O 失败 4
    table::verify_table_stamp(&src, stamp, ss).unwrap_or_else(|f| bail_fail(f));
    // 写表前的占用闸：目标上任一分区在用（镜像：任一绑定到它的 loop 在用）即拒绝，
    // 否则新表会与内核仍在使用的分区视图分叉
    if let Err(f) = crate::fsops::ensure_table_write_idle(&src) {
        bail_fail(f);
    }
    // 记录存在时只认同一份请求：表种类、--overwrite-data 与扇区大小决定写入内容
    if let Some(journal) = src.record_journal_mut()
        && let Err(why) = journal.bind_plan(digest)
    {
        bail_fail(RunOutcome::refused(why));
    }
    // 保护性 MBR 由 `protective_mbr_content` 判：盘上那份已规范时返回 None，本次不写 LBA0
    let protective = match kind {
        table::TableKind::Gpt => match table::protective_mbr_content(&src, ss) {
            Ok(p) => p,
            Err(f) => bail_fail(f),
        },
        table::TableKind::Mbr => None,
    };
    let view = match &content {
        Some(c) => crate::assembly::TableContent::Gpt {
            kind: table::TableWriteKind::Replace,
            content: c,
            protective_mbr: protective.as_deref(),
        },
        None => crate::assembly::TableContent::NewMbr,
    };
    let plan = crate::assembly::confirmed_table_step(&src, view, snapshot).unwrap_or_else(|f| bail_fail(f));
    // 全部写区间的原文先一次登记并落盘，这些步骤成功后才有第一次目标写入
    record_table_preimages(&mut src, &plan).unwrap_or_else(|f| bail_fail(f));
    // 表步之后的整份替换由 `table_record_done` 完成：sink 只接收进度，不写记录
    match crate::execution::execute_plan_complete(&mut src, &plan, None, &mut crate::execution::NoProgress) {
        Ok(()) => table_record_done(
            &mut src,
            &format!("created {} table (verify with: diskedit info {})", kind.as_str(), a.target),
        ),
        // 执行前扫描出的拒绝发生在首次写盘之前，原样报 3
        Err(f @ RunOutcome::Refused(_)) => bail_fail(f),
        // 3 / 4 的分工由执行器按写事实（`target_may_have_been_written`）与 FsError 映射
        // 给出；命令层不再拿"记录落没落盘"二次改判
        Err(f) => bail_fail(f.context("new failed")),
    }
}

/// 装配后、首次目标写入前的 pre-image 采集：一次登记全部写区间并单次落盘。执行期的
/// `write_at` 会再遇到同一批区间，记录层按区间跳过、保留这里登记的原文
fn record_table_preimages(
    src: &mut crate::dev::WriteSource,
    plan: &crate::planner::ConfirmedPlan,
) -> Result<(), RunOutcome> {
    let sector_size = src.sector_size;
    let mut spans = Vec::new();
    for step in plan.plan().steps() {
        let crate::planner::PlanStep::WriteTable(table) = step else {
            continue;
        };
        for w in &table.writes {
            // 记录粒度是扇区，与 `write_at` 取法一致：写入区间按扇区边界扩展后记整段原文。
            // 签名段只有 8 字节且可能不在扇区起点上，按字节区间登记会丢掉段内偏移
            let first = w.region.offset - w.region.offset % sector_size;
            let end = (w.region.offset + w.region.len)
                .div_ceil(sector_size)
                .saturating_mul(sector_size)
                .min(src.size);
            if end <= first {
                continue;
            }
            let mut original = vec![0u8; (end - first) as usize];
            src.read_at(first, &mut original).map_err(RunOutcome::no_mutation_io)?;
            let mut after = original.clone();
            let at = (w.region.offset - first) as usize;
            after[at..at + w.bytes.len()].copy_from_slice(&w.bytes);
            spans.push((first / sector_size, original, after));
        }
    }
    let Some(journal) = src.record_journal_mut() else {
        return Ok(());
    };
    journal.record_spans(spans).map_err(|e| {
        RunOutcome::no_mutation(format!("the pre-image of the table regions was not recorded: {e}"))
    })
}

/// 覆盖门文案里的区间清单：`[offset+len)` 形式，按快照顺序
fn region_list(regions: &[table::TableRegion]) -> String {
    regions
        .iter()
        .map(|r| format!("[{}+{})", r.offset, r.len))
        .collect::<Vec<_>>()
        .join(" ")
}

/// `--name` 写的是 GPT 分区名；MBR 条目没有这一列。旗标白名单是命令级的，
/// 分支级的消费差异必须在分支里显式拒绝——静默丢弃等于命令做了旗标
/// 明确排除的事却报成功
fn refuse_name_on_msdos(name: Option<&str>) {
    if name.is_some() {
        bail_fail(RunOutcome::refused("`--name` sets a GPT partition name — MBR entries have none; drop --name".to_string()));
    }
}

pub(crate) fn cmd_add(a: &AddRequest) -> crate::outcome::RunOutcome {
    // 分区号走 `--part-number`；`:N` 不是 add 的写法，静默忽略会让用户以为写进了 N 号槽
    if let Some(n) = a.part {
        bail_fail(RunOutcome::refused(format!("`add` takes the partition number as --part-number, not :{n}")));
    }
    // 分区节点不是合法载体：表条目写在整盘最前 16KiB，落到分区节点上会直接覆盖该
    // 分区的数据——判据与文案见 refuse_partition_node_target
    refuse_partition_node_target("add", &a.target);
    let len_bytes = match a.size {
        crate::cli::SizeSpec::Absolute(b) => b,
        other => bail_fail(RunOutcome::usage(format!(
            "`add` needs an absolute SIZE, got {} (see diskedit help add)",
            other.text()
        ))),
    };
    let mut src = open_table_record(a, crate::recovery::OpKind::Add).unwrap_or_else(|f| bail_fail(f));
    // 写表前的占用闸（见 fsops::ensure_table_write_idle）。放在几何判定之前：
    // 占用是"目标现在能不能写"的事实，与本条命令的参数是否合法无关
    if let Err(f) = crate::fsops::ensure_table_write_idle(&src) {
        bail_fail(f);
    }
    // 坐标系在几何计算前确定：GPT 条目按表头 ss 对齐，MBR 条目按容器 ss 对齐
    match table::table_label(&src) {
        Err(e) => bail_fail(crate::outcome::table_parse_failure(&e, format!("label probe failed: {e}"))),
        // 判型只用 table_label 一个来源（与 delete/create 同口径）：load_gpt 单独判会把
        // "保护 MBR 受损、条目数组可能完好"的 GPT 盘当作无表盘降级到 msdos 分支
        Ok(table::TableLabel::GptDamaged) => bail_fail(RunOutcome::refused(
            "the GPT on this target is damaged — repair it first (or recover it manually); \
             `new` would overwrite a table whose entries may still be recoverable"
                .to_string(),
        )),
        Ok(table::TableLabel::Gpt) => {
            // 判型刚由 load_gpt 成功得出；这里再取一次几何只有读盘，没有第二个判据
            let g = match table::load_gpt(&src) {
                Ok(Some(g)) => g,
                Ok(None) => bail_fail(RunOutcome::no_mutation("the GPT disappeared between reads — retry".to_string())),
                Err(e) => bail_fail(crate::outcome::table_parse_failure(&e, format!("label probe failed: {e}"))),
            };
            // 省略 --type-guid 取 Linux filesystem data 类型 GUID
            let type_guid = match a.type_guid {
                None => table::LINUX_FS_TYPE_GUID,
                Some(crate::cli::TypeSelector::Guid(g)) => g,
                Some(crate::cli::TypeSelector::Mbr(_)) => bail_fail(RunOutcome::refused(
                    "`--type-guid 0xNN` is an MBR type byte — a GPT entry takes a GUID, e.g. \
                     0fc63daf-8483-4772-8e79-3d69d8477de4",
                )),
            };
            // 全零类型 GUID 是"未用条目"的形状：LBA 非零而类型为零的条目在本工具与 UEFI/内核
            // 两套判据下含义相反，判据与文案同 gpt_policy::set_type_guid
            if type_guid == [0u8; 16] {
                bail_fail(RunOutcome::refused("type GUID cannot be all-zero (that marks an unused entry — use `delete`)"));
            }
            // 名字守卫在消费点：GPT 分支是 --name 的唯一落盘处（MBR 分支已拒绝 --name）
            if let Some(n) = &a.name { crate::support::check_gpt_name_len(n); }
            let len = partition_len(len_bytes, g.ss);
            let used: Vec<(u64, u64)> = g
                .entries
                .iter()
                .filter(|e| e.ending_lba != 0)
                .map(|e| (e.starting_lba, e.ending_lba))
                .collect();
            let unit = placement_unit(a.align, a.align_given, g.ss);
            let (start, end) = match a.start {
                Some(StartSelector::Lba(lba)) => fixed_start(
                    &used,
                    g.header.first_usable_lba,
                    g.header.last_usable_lba,
                    a.align,
                    g.ss,
                    len,
                    lba,
                ),
                Some(StartSelector::PinnedToEnd) => bail_fail(RunOutcome::refused(
                    "`add --start end` has no end point to pin to — give the starting LBA or omit --start",
                )),
                None => first_fit(
                    &used,
                    g.header.first_usable_lba,
                    g.header.last_usable_lba,
                    unit,
                    len,
                ),
            };
            let unique_guid = table::derive_guid(&src.path);
            // 几何准备含修复：盘上的表需要修复时这里会写盘，之后失败不再能声称"未写盘"，
            // 是否写过由进程写事实判定（写原语入口置位）
            let (geometry, num) = match crate::gpt_policy::add_entry_geometry(
                &mut src,
                a.part_number,
                start,
                end,
                a.name.as_deref().unwrap_or(""),
                type_guid,
                unique_guid,
            ) {
                Ok(v) => v,
                Err(f) => bail_fail(f),
            };
            let plan = match crate::assembly::assemble_gpt_add(&src, &geometry) {
                Ok(p) => p,
                Err(f) => bail_fail(f),
            };
            match crate::execution::execute_plan_complete(&mut src, &plan, None, &mut crate::execution::NoProgress) {
                Ok(()) => table_record_done(&mut src, &format!("added partition #{num} (verify with: diskedit info {})", a.target)),
                Err(f) => bail_fail(f),
            }
        }
        Ok(table::TableLabel::Mbr) => match table::parse_mbr(&src) {
            Err(e) => bail_fail(crate::outcome::mbr_parse_failure(&e, format!("label probe failed: {e}"))),
            Ok(Some(parts)) => {
                refuse_name_on_msdos(a.name.as_deref());
                // MBR 收 `0xNN`，省略取 0x83（util-linux pt-mbr.h MBR_LINUX_DATA_PARTITION）
                let os_type = match a.type_guid {
                    None => 0x83,
                    Some(crate::cli::TypeSelector::Mbr(t)) => t,
                    Some(crate::cli::TypeSelector::Guid(_)) => bail_fail(RunOutcome::refused(
                        "`--type-guid` with a GUID is a GPT type — an MBR entry takes a type byte 0xNN",
                    )),
                };
                let ss = src.sector_size;
                let total = src.size / ss;
                let len = partition_len(len_bytes, ss);
                let used: Vec<(u64, u64)> = parts
                    .iter()
                    .filter(|p| p.size_lba != 0)
                    .map(|p| {
                        let s = u64::from(p.start_lba);
                        (s, s + u64::from(p.size_lba) - 1)
                    })
                    .collect();
                let unit = placement_unit(a.align, a.align_given, ss);
                let (start, end) = match a.start {
                    Some(StartSelector::Lba(lba)) => {
                        fixed_start(&used, 1, total.saturating_sub(1), a.align, ss, len, lba)
                    }
                    Some(StartSelector::PinnedToEnd) => bail_fail(RunOutcome::refused(
                        "`add --start end` has no end point to pin to — give the starting LBA or omit --start",
                    )),
                    None => first_fit(&used, 1, total.saturating_sub(1), unit, len),
                };
                match add_mbr_entry(&mut src, a.part_number, start, end, os_type) {
                    Ok(num) => table_record_done(&mut src, &format!("added partition #{num} (verify with: diskedit info {})", a.target)),
                    Err(f) => bail_fail(f),
                }
            }
            Ok(None) => bail_fail(RunOutcome::no_mutation("the MBR disappeared between reads — retry".to_string())),
        },
        Ok(table::TableLabel::None) => bail_fail(RunOutcome::refused("cannot add on none label — run `new` first".to_string())),
    }
}

/// msdos 的 add 编排：目标可写判定 → 一次读定快照（LBA0/LBA1/末 LBA）→ 用快照里的
/// LBA0 原文算新内容（纯计算，不再读盘）→ 装配 → 执行。写入与比对只在命令层出现，
/// `table` 不反向依赖 `assembly` / `execution`
fn add_mbr_entry(
    src: &mut crate::dev::WriteSource,
    slot: Option<u32>,
    start: u64,
    end: u64,
    os_type: u8,
) -> Result<u32, RunOutcome> {
    table::require_writable_mbr(src)?;
    let regions = table::mbr_compare_regions(src)?;
    let snapshot = table::table_snapshot(src, &regions)?;
    let Some(lba0) = snapshot.bytes.first() else {
        return Err(RunOutcome::internal("the table snapshot is empty — LBA0 is missing"));
    };
    let total = src.size / src.sector_size;
    let (lba0, num) = table::add_mdos_entry_image(lba0, slot, start, end, os_type, total)?;
    let content = crate::assembly::TableContent::Mbr { kind: table::TableWriteKind::Add, lba0: &lba0 };
    let plan = crate::assembly::confirmed_table_step(src, content, snapshot)?;
    crate::execution::execute_plan_complete(src, &plan, None, &mut crate::execution::NoProgress)?;
    Ok(num)
}

/// 自动放置的对齐单位：未显式给 `--align` 时按 1 MiB（分区对齐是性能与寿命的基本要求，
/// 也是 `create` 一直以来的行为）；显式给了就按值，`none` 为 1 个 LBA。
///
/// 显式 `--start` 不走这里：起点是用户给的精确 LBA，缺省不隐式挪动。
fn placement_unit(align: crate::domain::Align, align_given: bool, sector_size: u64) -> u64 {
    if align_given {
        align.unit_sectors(sector_size).unwrap_or(1)
    } else {
        crate::support::mib_in_sectors(sector_size)
    }
}

/// 分区长度（LBA，向上取整）：SIZE 是字节数，条目按扇区记账
fn partition_len(bytes: u64, sector_size: u64) -> u64 {
    bytes.div_ceil(sector_size)
}

/// 给出起点时按该起点落位；对齐后越界或压到已有分区即前置拒绝
fn fixed_start(
    used: &[(u64, u64)],
    lo: u64,
    hi: u64,
    align: crate::domain::Align,
    ss: u64,
    len: u64,
    requested: u64,
) -> (u64, u64) {
    let start = align_start(align, requested, ss);
    let Some(end) = start.checked_add(len - 1) else {
        bail_fail(RunOutcome::refused(format!(
            "a partition of {len} sectors starting at LBA {start} runs past the end of the address space"
        )));
    };
    if start < lo || end > hi {
        bail_fail(RunOutcome::refused(format!(
            "--start {requested} lands at LBA {start}..{end}, outside the usable range {lo}..{hi}"
        )));
    }
    if let Some((s, e)) = used.iter().find(|(s, e)| start <= *e && *s <= end) {
        bail_fail(RunOutcome::refused(format!(
            "--start {requested} lands at LBA {start}..{end}, which overlaps the partition at LBA {s}..{e}"
        )));
    }
    (start, end)
}

/// 按 `unit` 对齐后能完整容纳 `len` 的第一个空洞；找不到即前置拒绝
fn first_fit(used: &[(u64, u64)], lo: u64, hi: u64, unit: u64, len: u64) -> (u64, u64) {
    let Some(start) = aligned_gaps(used, lo, hi, unit)
        .into_iter()
        .find_map(|(s, e)| (e - s + 1 >= len).then_some(s))
    else {
        bail_fail(RunOutcome::refused(format!(
            "no aligned free gap fits {len} sectors — the target has no room for this partition"
        )));
    };
    (start, start + len - 1)
}

/// msdos 的 delete 编排：目标可写判定 → 一次读定快照（LBA0/LBA1/末 LBA）→ 用快照里的
/// LBA0 原文算新内容（纯计算，不再读盘）→ 装配 → 执行。写入与比对只在命令层出现，
/// `table` 不反向依赖 `assembly` / `execution`。
fn delete_mbr_entry(src: &mut crate::dev::WriteSource, part: u32) -> Result<(), RunOutcome> {
    table::require_writable_mbr(src)?;
    let regions = table::mbr_compare_regions(src)?;
    let snapshot = table::table_snapshot(src, &regions)?;
    let Some(lba0) = snapshot.bytes.first() else {
        return Err(RunOutcome::internal("the table snapshot is empty — LBA0 is missing"));
    };
    let lba0 = table::delete_mdos_entry(lba0, part)?;
    let content = crate::assembly::TableContent::Mbr { kind: table::TableWriteKind::Remove, lba0: &lba0 };
    let plan = crate::assembly::confirmed_table_step(src, content, snapshot)?;
    // 表步之后的整份替换由 `cmd_delete` 的 `table_record_done` 完成：sink 只接收进度，
    // 不写记录
    crate::execution::execute_plan_complete(src, &plan, None, &mut crate::execution::NoProgress)
}

pub(crate) fn cmd_delete(a: &DeleteRequest) -> crate::outcome::RunOutcome {
    let Some(pref) = a.part else {
        bail_fail(RunOutcome::refused("`delete` needs a partition: diskedit delete <TARGET>:N --yes (see diskedit help delete)".to_string()));
    };
    // 载体必须是整盘：delete 清零的是节点 LBA0 里的条目槽，而分区节点上被判成 msdos 的
    // 可能只是文件系统引导扇区——FAT/NTFS 的引导扇区末尾同为 0x55AA，MBR 判据只认
    // 这个签名。判据与 new/add/create 同源
    refuse_partition_node_target("delete", &a.target);
    if !a.yes {
        bail_fail(RunOutcome::refused("`delete` removes the target partition entry; pass --yes to confirm"));
    } else {
        let mut src = open_table_record(a, crate::recovery::OpKind::Delete).unwrap_or_else(|f| bail_fail(f));
        // 写表前的占用闸：删掉一个仍被内核使用的分区，会让盘上表与挂载视图分叉
        if let Err(f) = crate::fsops::ensure_table_write_idle(&src) {
            bail_fail(f);
        }
        // 锁下快照
        let part = crate::gpt_policy::resolve_part(&src, pref).unwrap_or_else(|f| bail_fail(f));
        let r = match table::table_label(&src) {
            Ok(table::TableLabel::Gpt) => crate::gpt_policy::delete_entry(&mut src, part),
            Ok(table::TableLabel::Mbr) => delete_mbr_entry(&mut src, part),
            Ok(other) => bail_fail(RunOutcome::refused(format!("`delete` needs a GPT or MBR target (label: {other})"))),
            Err(e) => bail_fail(crate::outcome::table_parse_failure(&e, format!("label probe failed: {e}"))),
        };
        match r {
            Ok(()) => table_record_done(&mut src, &format!("deleted partition #{part} (verify with: diskedit info {})", a.target)),
            Err(f) => bail_fail(f),
        }
    }
}

pub(crate) fn cmd_resize_part(a: &ResizePartRequest) -> crate::outcome::RunOutcome {
    let Some(pref) = a.part else {
        bail_fail(RunOutcome::refused("`resize-part` needs :N (see diskedit help resize-part)".to_string()));
    };
    if a.grow_to_end && a.end.is_some() {
        bail_fail(RunOutcome::refused("--end and --grow-to-end are mutually exclusive".to_string()));
    }
    // 载体必须是整盘：条目按节点 LBA0 的偏移读写（判据与 new/add/create 同源）
    refuse_partition_node_target("resize-part", &a.target);
    let (mut src, _resumed) =
        open_record_resumable(a, crate::recovery::OpKind::ResizePart).unwrap_or_else(|f| bail_fail(f));
    // 坐标系在几何计算前确定：resize-part 仅支持 GPT，条目按表头 ss 对齐（可与容器 ss 不同）。
    // 几何走唯一构造点（条目重叠在此被拒），修复后的 last_usable 也由它给出
    let (g, repair) = crate::gpt_policy::require_gpt_geometry(&src, "resize-part").unwrap_or_else(|f| bail_fail(f));
    // 锁下快照
    let part = crate::gpt_policy::resolve_part_in(crate::gpt_policy::TableEntries::Gpt(g.entries()), pref)
        .unwrap_or_else(|f| bail_fail(f));
    let e = crate::gpt_policy::live_entry(&g, part).unwrap_or_else(|f| bail_fail(f));
    // resize-part 只改末端：起点就是盘上的现有起点，挪位归 move
    let start = e.starting_lba;
    let end = if a.grow_to_end {
        // 吃满后方可用区（本工具语义）：扩到 last_usable_lba；
        // 后方有分区时由 resize_part 的重叠校验拒绝
        g.last_usable_lba()
    } else {
        let Some(e) = a.end else {
            bail_fail(RunOutcome::refused("`resize-part` needs --end (or --grow-to-end)".to_string()));
        };
        // --grow-to-end 钉死 last_usable 不做下取整；常规路径只对末端下取整
        align_end(a.align, start, e, g.sector_size())
    };
    // 确认门看盘上几何算出的影响面：末端下移会截短分区（连同其中的文件系统），
    // 越过 undo 的回滚点。纯扩容与"请求范围即现状"不在此列
    confirm::gate(a.yes, confirm::Impact {
        shrinking: end < e.ending_lba,
        ..Default::default()
    });
    // resize-part 不登记 --chunk：恢复粒度取记录层的默认块字节
    let chunk = crate::execution::DEFAULT_CHUNK_BYTES;
    let mut logger = Logger::open(&src, Some(g.disk_guid()));
    let o = resize_part_via_plan(&mut src, &g, repair, part, start, end, chunk, &mut |m| logger.log(m));
    let o = table_record_done_if_complete(&mut src, o);
    if o.is_complete() {
        println!("resize-part complete (verify with: diskedit info {})", a.target);
    }
    o
}

/// `resize-part` 的只读判定结果：prepare 算一次，execute 只按它落盘。
///
/// `committed_old` 与 `fs_action` 对外开放：`movepart::tests` 用它们断言"已提交的现场
/// 重跑时仍会做 FS 步"，其余字段只在本模块内消费
pub(crate) struct PlannedResizePart {
    ss: u64,
    part: u32,
    old_start: u64,
    old_end: u64,
    new_start: u64,
    new_end: u64,
    fstype: crate::fsid::ContentKind,
    fs_shrunk: movepart::FsShrink,
    resume_chunks: u64,
    pub(crate) committed_old: Option<(u64, u64)>,
    pub(crate) fs_action: movepart::FsAction,
    repair: crate::gpt_policy::RepairAction,
    new_bytes: u64,
    no_fs: bool,
    defer_lvm: bool,
    lvm: Option<movepart::LvmPostFacts>,
    /// 起点变更的搬移段 `(part, src_first, dst_first, sectors)`；None = 纯扩缩或已提交
    relocate: Option<(u32, u64, u64, u64)>,
    chunk_len: u64,
    g: crate::geometry::ValidatedGeometry,
}

/// 事务入口：绑定计划身份与搬移前区间，跑完只读判定后把表与扩容交给装配与执行器。
// 参数是一次作业的独立事实（几何 / 区间 / 开关 / 日志），合成结构体只是把它们换个位置
#[allow(clippy::too_many_arguments)]
fn resize_part_via_plan(
    src: &mut WriteSource,
    g: &crate::geometry::ValidatedGeometry,
    repair: crate::gpt_policy::RepairAction,
    part: u32,
    new_start: u64,
    new_end: u64,
    chunk_len: u64,
    log: &mut dyn FnMut(&str),
) -> RunOutcome {
    resize_part_via_plan_with(
        src, g, repair, part, new_start, new_end, chunk_len, false, None, false, log,
    )
}

/// `resize` 的原地入口：与 `resize-part` 共用装配-执行链，额外带上 `--no-fs` 与
/// LVM 收尾基线。`defer_lvm` 表示 LVM 链由命令层在表提交之后单独收口。
// 参数是一次作业的独立事实（几何 / 区间 / 开关 / 日志），合成结构体只是把它们换个位置
#[allow(clippy::too_many_arguments)]
pub(crate) fn resize_part_via_plan_with(
    src: &mut WriteSource,
    g: &crate::geometry::ValidatedGeometry,
    repair: crate::gpt_policy::RepairAction,
    part: u32,
    new_start: u64,
    new_end: u64,
    chunk_len: u64,
    no_fs: bool,
    lvm: Option<movepart::LvmSelection>,
    defer_lvm: bool,
    log: &mut dyn FnMut(&str),
) -> RunOutcome {
    // 出口归责：执行器只看计划内的写入，repair 与阶段 0 的 FS 缩容不在它的视野里。
    // 函数体内各早退原样返回，由这一处统一过判定点
    resize_part_via_plan_unreconciled(
        src,
        g,
        repair,
        (part, new_start, new_end, chunk_len),
        (no_fs, defer_lvm),
        lvm,
        log,
    )
    .reconcile_with_writes()
}

/// 两个 LBA 的有符号差：Δ<0 表示目标在源左侧
fn signed_delta(dst: u64, src: u64) -> Result<i64, RunOutcome> {
    let magnitude = i64::try_from(dst.abs_diff(src)).map_err(|_| {
        RunOutcome::refused("the relocation shift exceeds a signed LBA offset — refusing")
    })?;
    Ok(if dst >= src { magnitude } else { -magnitude })
}

/// [`resize_part_via_plan_with`] 的主体：各出口只陈述本步结论，归责留给外层。
/// `range` = (分区号, 起点, 终点, 块大小)；`switches` = (`no_fs`, `defer_lvm`)
fn resize_part_via_plan_unreconciled(
    src: &mut WriteSource,
    g: &crate::geometry::ValidatedGeometry,
    repair: crate::gpt_policy::RepairAction,
    range: (u32, u64, u64, u64),
    switches: (bool, bool),
    lvm: Option<movepart::LvmSelection>,
    log: &mut dyn FnMut(&str),
) -> RunOutcome {
    let (part, new_start, new_end, chunk_len) = range;
    let (no_fs, defer_lvm) = switches;
    // 事务入口的注入点（与 apply 同义）：此刻 abort 必须与"从未运行过"不可区分
    movepart::fault_before_any_write();
    // 记录型现场：先把"这是哪一次请求"写进记录。新记录只写一次，已有记录必须与
    // 本次请求一致；`--chunk` 是执行参数、不在摘要里，由搬移校验单独核对
    let disk_size = src.size;
    if let Some(journal) = src.record_journal_mut() {
        let digest = crate::recovery::PlanDigest::of_request(
            journal.op(),
            part,
            new_start,
            new_end,
            g.sector_size(),
            disk_size,
        );
        if let Err(why) = journal.bind_plan(digest) {
            return RunOutcome::refused(format!("{why} — refusing to resume it"));
        }
    }
    let p = match prepare_resize_part(
        src,
        g,
        repair,
        part,
        new_start,
        new_end,
        chunk_len,
        no_fs,
        lvm,
        defer_lvm,
        log,
    ) {
        Ok(p) => p,
        Err(f) => return f,
    };
    // 记录型现场：把搬移前区间写进记录。它必须在首次目标写入之前落盘——表提交之后
    // 盘上只剩新区间，再想记搬移前形状就已经没有了
    if let Some(journal) = src.record_journal_mut() {
        if let Err(e) = journal.bind_old_range((p.old_start, p.old_end)) {
            return RunOutcome::no_mutation(format!("the pre-commit range was not recorded ({e})"));
        }
        // 起点变更的搬移与 move 同构：条目表绑进记录——执行器的游标恢复、续跑一致性
        // 校验与「已提交条目整组跳过」共用同一套机制。已提交态与纯扩缩不绑
        if let Some((rpart, src_first, dst_first, sectors)) = p.relocate {
            let delta = match signed_delta(dst_first, src_first) {
                Ok(d) => d,
                Err(f) => return f,
            };
            let mut reloc = crate::recovery::Relocation {
                entries: vec![crate::recovery::RelocEntry {
                    partition: rpart,
                    src_first,
                    len: sectors,
                    delta,
                    is_swap: false,
                }],
                entry_index: 0,
                chunks_in_entry: p.resume_chunks,
                chunk_bytes: p.chunk_len,
                kind: crate::recovery::RelocKind::Shift,
                grow_part: 0,
                disk_size,
                ss: p.ss,
            };
            // 续跑时记录里已有同一条目表：校验计划字段一致后沿用记录里的双游标
            if let Some(recorded) = journal.relocation() {
                if recorded.entries != reloc.entries
                    || recorded.chunk_bytes != reloc.chunk_bytes
                    || recorded.kind != reloc.kind
                    || recorded.grow_part != reloc.grow_part
                    || recorded.disk_size != reloc.disk_size
                    || recorded.ss != reloc.ss
                {
                    return RunOutcome::refused(
                        "the recovery record's relocation plan does not match this request"
                            .to_string(),
                    );
                }
                reloc = recorded.clone();
            }
            if let Err(e) = journal.bind_relocation(reloc, None) {
                return RunOutcome::no_mutation(format!(
                    "the relocation plan was not recorded ({e})"
                ));
            }
        }
        if journal.op() == crate::recovery::OpKind::Resize
            && let Some(f) = p.lvm.as_ref().filter(|f| f.lv_path.is_some())
            && let Err(e) = journal.record_lvm_post(f.to_record())
        {
            return RunOutcome::no_mutation(format!(
                "the LVM finish baseline was not recorded ({e})"
            ));
        }
    }
    // 探测（identify/fs_min_bytes）可能耗时良久，此间的盘上表不再等于决策依据。
    // 首写（阶段 0 的 FS 缩）之前的最后一笔复核读当前表
    if let Err(f) = p.g.ensure_entries_unchanged_fail(src) {
        return f;
    }
    if let Some(what) = p.repair.describe() {
        if let Err(f) = crate::gpt_policy::apply_repair(src, &p.repair) {
            return f;
        }
        log(&format!("[repair] {what}"));
    }
    movepart::fault_after_repair();
    // 从这里起的失败只有两种归宿：首写之前的执行器原变体，或越界之后的 Partial
    let mut post: Vec<crate::outcome::Pending> = Vec::new();
    match execute_resize_part(src, p, log, &mut post) {
        Ok(()) => RunOutcome::applied_with(post),
        // 越过首写之后的执行器失败已折成 Partial，已知待办随它一并报出
        Err(RunOutcome::Partial { cause, pending, origin }) => RunOutcome::Partial {
            cause,
            pending: pending.into_iter().chain(post).collect(),
            origin,
        },
        // 待办只由写盘之后的步骤产生：非空即已越过首写，"确定未写盘"的原变体不再成立
        Err(other) if !post.is_empty() => RunOutcome::partial_with(other.to_string(), post),
        Err(other) => other,
    }
}

/// 事前判定（只读）：占用闸、范围、重叠、恢复三态与 FS 能力。返回 `Err` 一律发生在
/// 任何写盘之前；签名里没有 `&mut WriteSource`，因此它不可能写盘。
// 参数是一次作业的独立事实（几何 / 区间 / 开关 / 日志），合成结构体只是把它们换个位置
#[allow(clippy::too_many_arguments)]
pub(crate) fn prepare_resize_part(
    src: &crate::dev::FileSource,
    g: &crate::geometry::ValidatedGeometry,
    repair: crate::gpt_policy::RepairAction,
    part: u32,
    new_start: u64,
    new_end: u64,
    chunk_len: u64,
    no_fs: bool,
    lvm: Option<movepart::LvmSelection>,
    defer_lvm: bool,
    log: &mut dyn FnMut(&str),
) -> Result<PlannedResizePart, RunOutcome> {
    let g = g.clone();
    let ss = g.sector_size();
    let e = crate::gpt_policy::live_entry(&g, part)?;
    // 起点不变、末端不变或只增的请求按 Extend 放行 DM 栈持有；缩小要求无持有者
    let edit = if new_start == e.starting_lba && new_end >= e.ending_lba {
        crate::fsops::WriteKind::Extend
    } else {
        crate::fsops::WriteKind::Overwrite
    };
    crate::fsops::ensure_idle_before_write(
        src,
        part,
        e.starting_lba
            .checked_mul(ss)
            .ok_or_else(|| RunOutcome::no_mutation("partition start overflows byte offset"))?,
        edit,
    )?;
    // 只拦搬移（起点变化）：LUKS/LVM PV/swap 的纯扩缩不搬数据，允许
    if (movepart::is_forbidden_type_guid(&e.partition_type_guid)
        || movepart::is_swap_guid(&e.partition_type_guid))
        && new_start != e.starting_lba
    {
        return Err(RunOutcome::refused(
            "swap/LUKS/LVM PV relocation refused (size-only changes are allowed)",
        ));
    }
    if new_start < g.first_usable_lba() || new_end > g.last_usable_lba() || new_start > new_end {
        return Err(RunOutcome::refused(format!(
            "new range {new_start}..{new_end} outside usable {}..{}",
            g.first_usable_lba(),
            g.last_usable_lba()
        )));
    }
    let old_start = e.starting_lba;
    let old_end = e.ending_lba;
    for (i, other) in g.entries().iter().enumerate() {
        if i + 1 == part as usize || other.ending_lba == 0 {
            continue;
        }
        if !(new_end < other.starting_lba || new_start > other.ending_lba) {
            return Err(RunOutcome::refused(format!(
                "new range overlaps partition #{}",
                i + 1
            )));
        }
    }
    // 搬移几何对所有路径同一份：起点变更时按 min(旧长, 新长) 拷贝，块布局与 move 的
    // 求解器同源
    let moved = old_start.abs_diff(new_start);
    let old_len = old_end - old_start + 1;
    let copy_len = old_len.min(new_end - new_start + 1);
    // 自重叠判据取**本次实际读取的扇区数**（= copy_len）：缩容时旧区尾部不读，
    // 位移落在 copy_len..old_len 之间时目的区与读区并不相交，按旧长判会把块白白缩小
    let total_chunks = crate::planner::chunk::move_chunk_layout(copy_len, ss, moved, copy_len, chunk_len)
        .map(|(_, count)| count)
        .ok_or_else(|| RunOutcome::no_mutation("resize LBA arithmetic overflows byte offset"))?;
    // 恢复判定取三个维度：本次请求 / 记录里的持久状态 / 盘上几何
    let (fs_shrunk, resume_chunks, committed_old, recorded_lvm) =
        if let Some(journal) = src.record_journal.as_ref()
    {
        let progress = journal.progress();
        let recorded_old = journal.old_range();
        // 收尾基线只在第一次运行算一次并落进记录：续跑时 LV 已经涨过一截，
        // 按现状重算只会得出"增量 0"或重复扩容
        let recorded_lvm = journal.lvm_post().map(|post| movepart::LvmPostFacts {
            lv_path: None,
            lv_size_before: post.lv_size_before,
            lv_target: post.lv_target,
            extent_bytes: post.extent_bytes,
        });
        // 盘上条目必须是搬移前区间（未提交）或请求区间（已提交）
        if let Some((rs, re)) = recorded_old
            && (old_start, old_end) != (rs, re)
            && (old_start, old_end) != (new_start, new_end)
        {
            return Err(RunOutcome::refused(format!(
                "partition {part} sits at {old_start}..{old_end}, neither the record's pre-commit range {rs}..{re} nor the requested {new_start}..{new_end} — the layout moved under the job; refusing"
            )));
        }
        let at_target = (old_start, old_end) == (new_start, new_end);
        if !journal.is_persisted() {
            (movepart::FsShrink::NotShrunk, 0, None, None)
        } else if at_target && progress.transfer_cursor != total_chunks && moved != 0 {
            // 表已停在新区间、游标却停在半途：只可能是盘面被外部改到了目标位
            return Err(RunOutcome::refused(format!(
                "partition {part} already sits at the requested range but the record's cursor is {}/{} — the layout moved under the job; refusing",
                progress.transfer_cursor, total_chunks
            )));
        } else if at_target && (moved == 0 || progress.transfer_cursor == total_chunks) {
            log("resize already committed — finishing filesystem step");
            // 表提交之后盘上只剩新区间；FS 步按记录里的搬移前几何判 Grow
            let pre_commit = match (journal.op(), recorded_old) {
                (crate::recovery::OpKind::Resize | crate::recovery::OpKind::ResizePart, None) => {
                    return Err(RunOutcome::refused(format!(
                        "the record for partition {part} has no pre-commit range — the filesystem step \
                         cannot be judged; refusing: release it with `diskedit abandon`, then run the \
                         whole command again"
                    )));
                }
                (_, Some(range)) => range,
                (_, None) => (new_start, new_end),
            };
            (
                movepart::fs_shrunk_from_phase(progress.fs_phase),
                progress.transfer_cursor,
                Some(pre_commit),
                recorded_lvm,
            )
        } else {
            log(&format!(
                "resuming at chunk {} (recovery record)",
                progress.transfer_cursor
            ));
            (
                movepart::fs_shrunk_from_phase(progress.fs_phase),
                progress.transfer_cursor,
                None,
                recorded_lvm,
            )
        }
    } else {
        // 没有记录的时间点只出现在测试夹具；生产入口都先挂上记录再走到这里
        (movepart::FsShrink::NotShrunk, 0, None, None)
    };
    // 阶段 0/3 的 FS 大小判据：已提交态取记录里的搬移前形状，否则取本次请求的旧形状
    let (fb_start, fb_end) = committed_old.unwrap_or((old_start, old_end));
    let byte_overflow = || RunOutcome::no_mutation("resize LBA arithmetic overflows byte offset");
    let old_bytes = (fb_end - fb_start + 1).checked_mul(ss).ok_or_else(byte_overflow)?;
    let new_bytes = (new_end - new_start + 1).checked_mul(ss).ok_or_else(byte_overflow)?;
    // LVM 收尾事实：记录里的基线优先，新跑时由本次请求与新增字节导出
    let lvm_post = match recorded_lvm {
        Some(f) => Some(f),
        None => movepart::lvm_post_for(defer_lvm, lvm.as_ref(), old_bytes, new_bytes)?,
    };
    // 表项 LBA 的单位是表自身的 ss。identify 只读目标内容，仍属事前判定
    let fstype = crate::fsid::identify(
        src,
        old_start.checked_mul(ss).ok_or_else(byte_overflow)?,
        (old_end - old_start + 1).checked_mul(ss).ok_or_else(byte_overflow)?,
    )
    .map_err(RunOutcome::no_mutation_io)?;
    // 缩容的 FS 步就是首次写盘：工具缺失或类型不可缩必须在它之前拒绝
    let needs_shrink = new_bytes < old_bytes && fs_shrunk != movepart::FsShrink::Verified;
    // --no-fs 与缩容不可共存：分区末端越过未缩的 FS 元数据即数据损坏
    if no_fs && needs_shrink {
        return Err(RunOutcome::refused(
            "--no-fs cannot shrink: the filesystem has to be shrunk first, otherwise the new partition end would cut into filesystem metadata",
        ));
    }
    // 已提交而 FS 步仍未完成的扩容：--no-fs 会让收尾以"无事可做"结束并撤掉记录
    if no_fs && committed_old.is_some() && new_bytes > old_bytes {
        return Err(RunOutcome::refused(
            "--no-fs cannot drop the filesystem step of an unfinished resize: the partition is already extended but its filesystem is not — re-run without --no-fs",
        ));
    }
    let fs_action = if needs_shrink {
        movepart::FsAction::Shrink
    } else if new_bytes > old_bytes && !no_fs {
        movepart::FsAction::Grow
    } else {
        movepart::FsAction::Leave
    };
    match fs_action {
        movepart::FsAction::Shrink => crate::fsops::check_shrink(
            Some(fstype),
            src.is_block,
            crate::fsops::needs_loop(&crate::fsops::DeviceScope::Partition(part), src.is_block),
        )?,
        // 扩的目标与收尾取同一判据（grow_target_at）：要动的那段可能是分区尾部的
        // RW overlay 层，也可能是分区自己。这道写盘前的判定放在写表之前。
        // base 与长度必须同源——都取盘上条目：已提交续跑时盘上已是新区间（数据与
        // swap 签名都随搬移落位），拿记录里的搬移前长度去配新起点会探到一块谁也不
        // 认识的混合区间，FS 步被误判为 cannot grow unknown
        movepart::FsAction::Grow => crate::fsops::check_grow_step(
            crate::fsops::grow_target_at(
                src,
                part,
                old_start.checked_mul(ss).ok_or_else(byte_overflow)?,
                (old_end - old_start + 1)
                    .checked_mul(ss)
                    .ok_or_else(byte_overflow)?,
            )?,
            src.is_block,
        )?,
        movepart::FsAction::Leave => {}
    }
    // ext 预查 FS 最小尺寸（resize2fs -P × dumpe2fs -h 块大小），缩太小在写盘前拒绝
    if fs_action == movepart::FsAction::Shrink
        && let Some(min) = crate::fsops::fs_min_bytes(src, part, Some(fstype))?
        && new_bytes < min
    {
        return Err(RunOutcome::refused(format!(
            "target size {new_bytes} < minimum FS size {min} bytes (resize2fs -P)"
        )));
    }
    // 起点变更的搬移段 `(part, src_first, dst_first, sectors)`：非空时由执行器的
    // Relocate 步骤承担（分块、方向、自重叠收敛、durable 游标与 move 同一条机制），
    // 表提交在其后。已提交态与纯扩缩为 None
    let relocate = if moved != 0 && committed_old.is_none() {
        Some((part, old_start, new_start, copy_len))
    } else {
        None
    };
    Ok(PlannedResizePart {
        ss,
        part,
        old_start,
        old_end,
        new_start,
        new_end,
        fstype,
        fs_shrunk,
        resume_chunks,
        committed_old,
        fs_action,
        repair,
        new_bytes,
        no_fs,
        defer_lvm,
        lvm: lvm_post,
        relocate,
        chunk_len,
        g,
    })
}

/// 推进记录里的阶段号 / 游标 / FS 相位
fn save_resize_part_progress(
    src: &mut WriteSource,
    chunks: u64,
    fs_shrunk: movepart::FsShrink,
    step: u32,
) -> std::io::Result<()> {
    match src.record_journal_mut() {
        Some(journal) => journal.record_progress(step, chunks, fs_shrunk as u8),
        None => Ok(()),
    }
}

/// 执行（写盘）：缩容 FS → 表步骤（装配 + 执行器）→ 扩容 FS（执行器）。
/// 失败按原变体上抛：越过首写之后的改判由退出点的归责按写事实完成。
fn execute_resize_part(
    src: &mut WriteSource,
    p: PlannedResizePart,
    log: &mut dyn FnMut(&str),
    post: &mut Vec<crate::outcome::Pending>,
) -> Result<(), RunOutcome> {
    let (ss, part) = (p.ss, p.part);
    let (new_start, new_end) = (p.new_start, p.new_end);
    let new_bytes = p.new_bytes;
    let fstype = p.fstype;
    let committed_old = p.committed_old;
    let mut fs_shrunk = p.fs_shrunk;
    let chunks_done = p.resume_chunks;

    // ---- 阶段 0：缩容 FS 先缩（只做一次）----
    // 缩容不落不可回滚标记：缩小之后再回滚表项只会得到"FS 小于分区"这一自洽状态，
    // undo 仍然安全可用。判据是可逆性，不是"有没有调外部工具"
    if p.fs_action == movepart::FsAction::Shrink {
        let mut shrink_done = false;
        if p.fs_shrunk == movepart::FsShrink::Unverified {
            match crate::fsops::shrink_already_at(src, part, Some(fstype), new_bytes) {
                Some(true) => {
                    fs_shrunk = movepart::FsShrink::Verified;
                    save_resize_part_progress(src, chunks_done, fs_shrunk, 1)
                        .map_err(RunOutcome::from)?;
                    log(&format!(
                        "filesystem already at {new_bytes} bytes (re-read) — shrink step skipped"
                    ));
                    movepart::fault_rs_after_fs_shrink();
                    shrink_done = true;
                }
                Some(false) => {}
                None => {
                    // 回读仍不可用：缩容是否到位无从判断，接着改表会切进未缩的元数据
                    post.push(crate::outcome::Pending::new(
                        part,
                        crate::outcome::PendingKind::Other("fs shrink"),
                        "the filesystem size still cannot be read back, so the earlier shrink cannot be confirmed".to_string(),
                        "inspect the filesystem size by hand, then re-run the same command".to_string(),
                    ));
                    return Err(RunOutcome::partial_unverified(
                        "the earlier filesystem shrink cannot be confirmed",
                        std::mem::take(post),
                    ));
                }
            }
        }
        if !shrink_done {
            let o = crate::fsops::shrink_fs(src, part, Some(fstype), new_bytes)
                .map_err(|e| RunOutcome::partial(e.to_string()))?;
            // 卸挂结论要在消费结果之前取出：`pending` 拿走 leak 之后无从再判
            let release_failed = o.release_failed();
            let unverified = match &o {
                crate::fsops::ShrinkRunOutcome::Unverified { why, .. } => Some(why.clone()),
                crate::fsops::ShrinkRunOutcome::Verified { .. } => None,
            };
            let line = o.ok_line(new_bytes);
            for p in o.pending(part) {
                post.push(p);
            }
            if let Some(why) = unverified {
                fs_shrunk = movepart::FsShrink::Unverified;
                save_resize_part_progress(src, chunks_done, fs_shrunk, 1)
                    .map_err(RunOutcome::from)?;
                return Err(RunOutcome::partial_unverified(why, std::mem::take(post)));
            }
            fs_shrunk = movepart::FsShrink::Verified;
            save_resize_part_progress(src, chunks_done, fs_shrunk, 1)
                .map_err(RunOutcome::from)?;
            if let Some(l) = line {
                log(&l);
            }
            // 临时挂载点没卸掉 = 分区仍挂载中，改表等于在挂载中挪动分区
            if release_failed {
                return Ok(());
            }
            movepart::fault_rs_after_fs_shrink();
        }
    }

    // 阶段 0 的缩容已在它自己的首写之前由上面的复核覆盖；这里再读一次当前表
    p.g.ensure_entries_unchanged(src).map_err(RunOutcome::from)?;

    // 纯扩容没有其余 save 落点：FS 步失败或崩溃后，重跑会按 Fresh 算出尺寸不变而报成功，
    // FS 永不扩。复核通过后、写表之前落记录
    if p.fs_action == movepart::FsAction::Grow
        && new_start == p.old_start
        && committed_old.is_none()
    {
        save_resize_part_progress(src, chunks_done, fs_shrunk, 1).map_err(RunOutcome::from)?;
    }

    // ---- 阶段 2：提交表项（已提交态跳过）----
    if committed_old.is_none() {
        movepart::fault_rs_before_commit();
        let mut g2 = p.g.clone();
        g2.update_entry((part - 1) as usize, |te| {
            te.starting_lba = new_start;
            te.ending_lba = new_end;
        })
        .map_err(RunOutcome::from)?;
        let kind = if new_end > p.old_end {
            table::TableWriteKind::Grow
        } else if new_end < p.old_end {
            table::TableWriteKind::Shrink
        } else {
            table::TableWriteKind::Repartition
        };
        let plan = crate::assembly::assemble_resize_part(src, &g2, kind, p.relocate)
            .map_err(|e| RunOutcome::partial(e.to_string()))?;
        let mut sink = ResizePartSink {
            part,
            new_start,
            new_end,
            fs_phase: fs_shrunk as u8,
            log,
        };
        // 执行器按计划内的写盘进度分类；本进程此前写过的目标字节（修复表头、缩容 FS）
        // 不在它的视野里，改判由退出点的归责按写事实完成。
        // chunk 与请求同源：Relocate 步骤的分块、durable 游标节奏都按它算
        crate::execution::execute_plan_complete(src, &plan, Some(p.chunk_len), &mut sink)?;
    }

    // 起始 LBA 变了才需要修 NTFS HiddenSectors（扩缩不动 start 时跳过；已提交态续跑时
    // 盘上几何已是新值，搬移与否改由记录里的搬移前起点判定）
    if committed_old.is_some()
        && fstype == crate::fsid::ContentKind::Filesystem(crate::fsid::FsKind::Ntfs)
    {
        movepart::fix_ntfs_hidden_sectors(src, new_start, ss, part, log).map_err(RunOutcome::from)?;
    }

    // ---- 阶段 3：扩容 FS ----
    if p.fs_action == movepart::FsAction::Grow {
        // 扩到设备末端的工具以内核尺寸为准：内核视图停在旧尺寸时会以"无需处理"退出 0。
        // 重读失败时不跑 FS 步，把扩容记成待办
        if crate::support::kernel_resync(src) {
            let overflow = || RunOutcome::partial("growth byte offset overflows — refusing");
            let old_len = p.old_end.checked_sub(p.old_start).and_then(|v| v.checked_add(1))
                .ok_or_else(overflow)?;
            let new_len = new_end.checked_sub(new_start).and_then(|v| v.checked_add(1))
                .ok_or_else(overflow)?;
            // 扩容收尾只有一份实现：与 `resize --allow-move` 走同一条判定与同一条执行链
            let declared = Some(movepart::is_swap_guid(
                &crate::gpt_policy::live_entry(&p.g, part)?.partition_type_guid,
            ));
            movepart::finalize_growth(
                src,
                part,
                movepart::GrowthRegions {
                    old: (p.old_start, old_len),
                    new: (new_start, new_len),
                },
                ss,
                declared,
                log,
                post,
            )?;
        } else {
            post.push(crate::outcome::Pending::new(
                part,
                crate::outcome::PendingKind::Fs,
                "the kernel partition view still reports the old size, so the filesystem was not resized — refresh the view (partprobe / partx) and re-run to finish".to_string(),
                String::new(),
            ));
        }
    } else if p.no_fs {
        log("partition resized (--no-fs: filesystem left untouched)");
    }
    // 分区已提交但 LVM 链未跑：留待办，记录不在这里撤
    if let Some(facts) = &p.lvm
        && !p.defer_lvm
    {
        let what = match &facts.lv_path {
            Some(lv) => format!("partition resized but pvresize / lvextend for {lv} was not run"),
            None => "partition resized but pvresize was not run".to_string(),
        };
        post.push(crate::outcome::Pending::new(
            part,
            crate::outcome::PendingKind::Other("lvm chain (pvresize/lvextend)"),
            what,
            "re-run `diskedit resize` on the same target to finish the PV/LV chain (the recovery record holds the recorded extent count)".to_string(),
        ));
    }
    Ok(())
}

/// 表步骤完成后的记录落点：表提交后把进度推到阶段 2，再输出提交行。
struct ResizePartSink<'a> {
    part: u32,
    new_start: u64,
    new_end: u64,
    fs_phase: u8,
    log: &'a mut dyn FnMut(&str),
}

impl crate::execution::StepSink for ResizePartSink<'_> {
    fn transfer_checkpoint(
        &mut self,
        src: &mut WriteSource,
        state: &crate::execution::TransferState,
    ) -> Result<(), RunOutcome> {
        // 搬移步骤的 durable 边界：只有已经 sync 过的块才允许进入记录
        if let Some(journal) = src.record_journal_mut() {
            journal
                .record_transfer_checkpoint(
                    Some((state.entry_index, state.chunks_in_entry)),
                    state.completed_steps,
                    state.transfer_cursor,
                )
                .map_err(|e| {
                    RunOutcome::partial(format!("the transfer progress could not be persisted ({e})"))
                })?;
        }
        Ok(())
    }

    fn note(&mut self, msg: &str) {
        (self.log)(msg)
    }

    fn table_step_done(
        &mut self,
        src: &mut WriteSource,
        _step: usize,
        _state: &crate::execution::TransferState,
    ) -> Result<(), RunOutcome> {
        if let Some(journal) = src.record_journal_mut() {
            // 游标取记录现值：搬移步骤可能刚把它推到 total，回写旧值会触发
            // 「进度回退」拒绝
            let cursor = journal.progress().transfer_cursor;
            journal
                .record_progress(2, cursor, self.fs_phase)
                .map_err(|e| {
                    RunOutcome::partial(format!(
                        "the partition table was written but the progress step could not be recorded ({e})"
                    ))
                })?;
        }
        (self.log)(&format!(
            "partition {} committed at {}..{}",
            self.part, self.new_start, self.new_end
        ));
        movepart::fault_rs_after_commit();
        Ok(())
    }

    /// 文件系统步骤不经过本 sink：缩容在搬移与表提交之前、扩容在其后，都由命令层直接落地
    fn fs_step_done(
        &mut self,
        _src: &mut WriteSource,
        _step: usize,
        _state: &crate::execution::TransferState,
    ) -> Result<(), RunOutcome> {
        Ok(())
    }
}

/// `--start end` 把起点钉在 `last_usable_lba - len + 1`，是"贴尾打包"而非"挑一个对齐
/// 位置"；此时再给 `--align` 没有被消费的余地，静默忽略会让用户以为对齐生效。旗标白名单
/// 是命令级的，这个分支级的消费差异只能在分支里拒绝。旗标字段缺省是 `none`
/// （`add` 自动放置另有 1 MiB 缺省），所以只有 `align_given` 分得出"没给"与"显式给了"；
/// 分不出就无法在 `--start end` 上把被静默忽略的旗标拒绝掉
fn refuse_align_on_start_end(start: Option<StartSelector>, align_given: bool) {
    if matches!(start, Some(StartSelector::PinnedToEnd)) && align_given {
        bail_fail(RunOutcome::refused(
            "`--align` does not apply to `--start end` — the start is pinned to the last usable position; drop --align or pass an explicit start LBA",
        ));
    }
}

/// move/copy 共用的一对选择器：`:N` 与 `--start`。起点只有一个字段
/// （[`StartSelector`]），"数值与 `end` 同时给出"因此无从构造，两条命令也不必各自
/// 再判一次互斥
fn require_start(
    part: Option<crate::dev::PartSelector>,
    start: Option<StartSelector>,
    cmd: &str,
) -> (crate::dev::PartSelector, StartSelector) {
    let Some(pref) = part else {
        bail_fail(RunOutcome::refused(format!(
            "`{cmd}` needs a partition: diskedit {cmd} <TARGET>:N (see diskedit help {cmd})"
        )));
    };
    let Some(start) = start else {
        bail_fail(RunOutcome::refused(format!(
            "`{cmd}` needs --start <LBA|end> (see diskedit help {cmd})"
        )));
    };
    (pref, start)
}

pub(crate) fn cmd_move(a: &MoveRequest) -> crate::outcome::RunOutcome {
    let (pref, selector) = require_start(a.part, a.start, "move");
    refuse_align_on_start_end(a.start, a.align_given);
    // 载体必须是整盘：条目按节点 LBA0 的偏移读写（判据与 new/add/create 同源）
    refuse_partition_node_target("move", &a.target);
    let (mut src, resumed) =
        open_record_resumable(a, crate::recovery::OpKind::Move).unwrap_or_else(|f| bail_fail(f));
    let (mut g, repair) = crate::gpt_policy::require_gpt_geometry(&src, "move").unwrap_or_else(|f| bail_fail(f));
    // 锁下快照
    let part = crate::gpt_policy::resolve_part_in(crate::gpt_policy::TableEntries::Gpt(g.entries()), pref)
        .unwrap_or_else(|f| bail_fail(f));
    let e = crate::gpt_policy::live_entry(&g, part).unwrap_or_else(|f| bail_fail(f));
    // 平移保持长度（本工具语义）：new_end = new_start + 原长度 - 1
    let len = e.ending_lba - e.starting_lba + 1;
    let start = match selector {
        StartSelector::PinnedToEnd => g
            .last_usable_lba()
            .checked_sub(len - 1)
            .unwrap_or_else(|| bail_fail(RunOutcome::refused("partition longer than usable range".to_string()))),
        StartSelector::Lba(lba) => align_start(a.align, lba, g.sector_size()),
    };
    // checked：start 来自 CLI 原始输入（--align none 时无上界），回绕会让目标区间越过可用范围
    let end = start.checked_add(len - 1)
        .unwrap_or_else(|| bail_fail(RunOutcome::refused("end LBA overflows address space".to_string())));
    confirm::gate(a.yes, confirm::Impact {
        relocating: usize::from(start != e.starting_lba),
        ..Default::default()
    });
    // move 保持长度：没有可跳过的文件系统步骤
    if a.no_fs {
        eprintln!(
            "note: `move` keeps the partition's size unchanged, so there is no filesystem step to \
             skip — --no-fs has no effect on this command"
        );
    }
    let chunk = movepart::chunk_bytes(a.chunk_mib)
        .unwrap_or_else(|e| bail_fail(RunOutcome::refused(e.to_string())));
    // 落位与长度取锁下几何：后面装配与记录绑定都用这一份，不按请求推算
    let (old_start, old_end) = (e.starting_lba, e.ending_lba);
    let ss = g.sector_size();
    // 请求摘要只推导一次：早退收尾与正常续跑绑定的是同一份值，两处不各算一次
    let digest = crate::recovery::PlanDigest::of_request(
        crate::recovery::OpKind::Move,
        part,
        start,
        end,
        ss,
        src.size,
    );
    // 盘上条目是否已在记录的目的位：move 的表步只改这一条，读回它的起点与末端即表已写下的
    // 证据。记录字段只说目的位是 `src_first + delta`，说不出现在在不在，故必须与锁下几何比对；
    // 长度、其他条目与目的区是否仍空闲由本命令后续的整份条目比对、
    // `verify_relocation_resume` 与 `ensure_entries_unchanged_fail` 分别回答
    let at_recorded_destination = start == old_start
        && resumed
        && src
            .record_journal()
            .and_then(|j| j.relocation())
            .is_some_and(|r| {
                r.entries.iter().any(|e| {
                    e.partition == part
                        && e.delta != 0
                        && e.src_first.checked_add_signed(e.delta) == Some(old_start)
                        && e.src_first
                            .checked_add_signed(e.delta)
                            .and_then(|d| d.checked_add(e.len))
                            .and_then(|v| v.checked_sub(1))
                            == Some(old_end)
                })
            });
    // 换区条目的重建在表提交之后：条目游标仍指着这条换区条目时，重建还没做。盘上条目
    // 已在目的位而重建未完的现场，剩下的重建要按同一条装配与执行路径重放
    let swap_rebuild_pending = at_recorded_destination
        && src
            .record_journal()
            .and_then(|j| j.relocation())
            .is_some_and(|r| {
                r.entries
                    .get(r.entry_index as usize)
                    .is_some_and(|e| e.partition == part && e.is_swap)
            });
    // 起点未变：不产生任何步骤，也不写表
    if start == old_start {
        if !resumed {
            println!(
                "partition {part} already starts at LBA {start} — nothing to move (verify with: diskedit info {})",
                a.target
            );
            return RunOutcome::applied_with(Vec::new());
        }
        // 记录在场。收尾只对"表已提交、数据已搬完"的记录合法：表是 move 计划的最后
        // 一步，阶段由记录字段派生；记录判不出阶段时，以盘上表项这一独立事实补足。
        // 补足失败只指路：不 undo、不清理、不续跑。
        let Some(journal) = src.record_journal() else {
            bail_fail(RunOutcome::refused(
                "the target was opened as resumable but its recovery record is gone; refusing".to_string(),
            ));
        };
        if journal.op() != crate::recovery::OpKind::Move {
            bail_fail(RunOutcome::refused(format!(
                "this target carries an unfinished `{}` record — finish or release it first \
                 (re-run the original command, or `diskedit abandon`)",
                journal.op().command_name()
            )));
        }
        match journal.record().stage() {
            // 表已提交：盘上条目已到记录的目的位（`at_recorded_destination`），只剩搬移后的
            // 回填与记录收尾。请求身份先核——早退不绕过 bind_plan 的管辖，换了目标的请求
            // 不能收尾别人的作业
            Ok(crate::recovery::Stage::Undecidable | crate::recovery::Stage::Relocated)
                if at_recorded_destination =>
            {
                src.record_journal_mut()
                    .expect("journal checked above")
                    .bind_plan(digest)
                    .unwrap_or_else(|msg| bail_fail(RunOutcome::refused(msg)));
                let base = start
                    .checked_mul(ss)
                    .unwrap_or_else(|| bail_fail(RunOutcome::no_mutation("partition start overflows byte offset")));
                let bytes = len
                    .checked_mul(ss)
                    .unwrap_or_else(|| bail_fail(RunOutcome::no_mutation("partition length overflows byte offset")));
                let kind = crate::fsid::identify(&src, base, bytes)
                    .unwrap_or_else(|e| bail_fail(RunOutcome::no_mutation_io(e)));
                if kind == crate::fsid::ContentKind::Filesystem(crate::fsid::FsKind::Ntfs) {
                    movepart::fix_ntfs_hidden_sectors(&mut src, start, ss, part, &mut |m| println!("{m}"))
                        .unwrap_or_else(|e| bail_fail(RunOutcome::partial_with(e.to_string(), Vec::new())));
                }
                let o = table_record_done_if_complete(&mut src, RunOutcome::applied_with(Vec::new()));
                if o.is_complete() {
                    println!(
                        "partition {part} was already relocated to LBA {start} — finishing the move (verify with: diskedit info {})",
                        a.target
                    );
                }
                return o;
            }
            // 数据搬移或收尾没走完。盘上条目仍在搬移前区间时，续跑只能用原命令（本请求的
            // 起点与原请求的起点不同），这里既不能收尾也不能替用户续跑。换区条目是例外：
            // 重建在表提交之后，中断时条目已在目的位而游标还指着它（见 `swap_rebuild_pending`），
            // 剩下的重建继续走下面同一条装配与执行路径
            Ok(crate::recovery::Stage::Planned | crate::recovery::Stage::Relocating { .. }) => {
                if !swap_rebuild_pending {
                    bail_fail(RunOutcome::refused(format!(
                        "the recorded move for partition {part} is unfinished — resume it by re-running \
                         the original `diskedit move` with its original --start"
                    )));
                }
            }
            // 字段不足以判定阶段：不自动 undo、不清理、不续跑，只指路。阶段本身不定码值：
            // 本次没写落 3、本次写过落 5，由 `bail_fail` 后的归责点按本次的写事实改判
            Ok(
                crate::recovery::Stage::Undecidable
                | crate::recovery::Stage::Relocated
                | crate::recovery::Stage::TableWritten
                | crate::recovery::Stage::Finishing,
            ) => {
                bail_fail(RunOutcome::refused(format!(
                    "the recovery record for partition {part} cannot be decided from its fields — \
                     inspect it with `diskedit inspect {}`; release it with `diskedit abandon` only if it is stale",
                    a.target
                )));
            }
            // `Complete`：已定型、只差释放——不代做动作，指 `undo` / `abandon`
            Ok(crate::recovery::Stage::Complete) => {
                bail_fail(RunOutcome::refused(format!(
                    "the recovery record for partition {part} is complete and only waits to be released — \
                     roll the recorded bytes back with `diskedit undo {}`, or release the record with \
                     `diskedit abandon {}`",
                    a.target, a.target
                )));
            }
            Err(e) => bail_fail(e),
        }
    }
    // 请求摘要在任何写盘之前绑定：已有记录属于别的请求时在此拒绝。绑定只改内存，
    // 真正的落盘随下面的原区间与搬移计划一起发生
    if let Some(journal) = src.record_journal_mut() {
        journal.bind_plan(digest).unwrap_or_else(|msg| bail_fail(RunOutcome::refused(msg)));
    }
    // 占用闸的探测取搬移前的旧区间：搬移之后原位置的内容已被覆盖
    let start_bytes = old_start
        .checked_mul(ss)
        .unwrap_or_else(|| bail_fail(RunOutcome::no_mutation("partition start overflows byte offset")));
    // 分区级占用闸：搬移整段复写目标区间，分区在用时拒绝（只读探测，写前）
    crate::fsops::ensure_idle_before_write(&src, part, start_bytes, crate::fsops::WriteKind::Overwrite)
        .unwrap_or_else(|f| bail_fail(f));
    // 续跑：盘上条目必须仍在记录里的搬移前区间。请求区间与盘上相同时已在上面收口，
    // 走到这里两者必然不同，故只比对搬移前区间。表已提交、重建未做的换区现场条目
    // 已在目的位，原区间只在记录里，不参与比对
    if resumed && !swap_rebuild_pending && let Some(journal) = src.record_journal() {
        match journal.old_range() {
            Some((recorded_start, recorded_end)) if (old_start, old_end) != (recorded_start, recorded_end) => {
                bail_fail(RunOutcome::refused(format!(
                    "partition {part} sits at {old_start}..{old_end}, not the record's pre-move range \
                     {recorded_start}..{recorded_end} — the layout changed outside this job; refusing"
                )))
            }
            // 续跑的起点随输出可见——只看输出无法区分"从断点继续"与"从头重算"，两者的
            // 耗时相差悬殊。pre-move range 在与不在只影响那半句说明
            Some(_) => eprintln!(
                "note: the on-disk range matches the record's pre-move range; resuming at chunk {}",
                journal.progress().transfer_cursor
            ),
            None => eprintln!(
                "note: the recovery record carries no pre-move range — the on-disk range cannot be \
                 cross-checked; resuming at chunk {}",
                journal.progress().transfer_cursor
            ),
        }
    }
    // 决策与写盘之间的外部改动：条目必须还是锁下读到的那一份
    g.ensure_entries_unchanged_fail(&src).unwrap_or_else(|f| bail_fail(f));
    // 换区条目在表提交之后由 mkswap 重建（镜像目标还要 losetup）：平台与工具必须在
    // 任何目标写入之前确认，否则会先把修复与表改完、再以 pending 收场。判据与
    // `resize --allow-move` 共用 movepart 的一处
    let (_, _, swap) =
        movepart::move_entry_swap(&src, &g, part, src.record_journal().and_then(|j| j.relocation()))
            .unwrap_or_else(|f| bail_fail(f));
    if swap.is_swap {
        movepart::ensure_swap_rebuild_tools(src.is_block).unwrap_or_else(|f| bail_fail(f));
    }
    if repair != crate::gpt_policy::RepairAction::None {
        crate::gpt_policy::apply_repair(&mut src, &repair).unwrap_or_else(|f| bail_fail(f));
        // 修复窗口内的外部改动再看一次：基线仍是修复前的读取，修复自身的写入收敛到目标值
        g.ensure_entries_unchanged_fail(&src).unwrap_or_else(|f| bail_fail(f));
        // 修复后重读重解析：装配的表内容与首写前快照都取自修复后的表
        g = match crate::gpt_policy::resolve_geometry(&src).unwrap_or_else(|f| bail_fail(f)) {
            Some((fresh, _)) => fresh,
            None => bail_fail(RunOutcome::refused(
                "the GPT disappeared after the repair — refusing".to_string(),
            )),
        };
    }
    let plan = crate::assembly::assemble_relocation_plan(
        &src,
        &mut g,
        &[(part, start)],
        src.record_journal().and_then(|j| j.relocation()),
        &mut |m| eprintln!("{m}"),
    )
    .unwrap_or_else(|f| bail_fail(f));
    let mut entries = Vec::new();
    for step in plan.plan().steps() {
        if let crate::planner::PlanStep::Relocate { part: moved, src_first, dst_first, sectors, is_swap } = step {
            let delta = match signed_delta(*dst_first, *src_first) {
                Ok(d) => d,
                Err(f) => bail_fail(f),
            };
            entries.push(crate::recovery::RelocEntry {
                partition: *moved,
                src_first: *src_first,
                len: *sectors,
                delta,
                is_swap: *is_swap,
            });
        }
    }
    let mut reloc = crate::recovery::Relocation {
        entries,
        entry_index: 0,
        chunks_in_entry: 0,
        chunk_bytes: chunk,
        kind: crate::recovery::RelocKind::Shift,
        grow_part: 0,
        disk_size: src.size,
        ss: plan.plan().sector_size().get(),
    };
    if let Some(journal) = src.record_journal() {
        match journal.relocation() {
            // 续跑沿用记录里的双游标：计划相关字段必须与本次装配一致
            Some(recorded) => {
                if recorded.entries != reloc.entries
                    || recorded.chunk_bytes != reloc.chunk_bytes
                    || recorded.kind != reloc.kind
                    || recorded.grow_part != reloc.grow_part
                    || recorded.disk_size != reloc.disk_size
                    || recorded.ss != reloc.ss
                {
                    bail_fail(RunOutcome::refused(
                        "the recovery record's relocation plan does not match this request"
                            .to_string(),
                    ));
                }
                // 记录与盘面的位置/目的区/块数判据与 `resize --allow-move` 共用一处
                crate::movepart::verify_relocation_resume(recorded, &g, src.size, chunk)
                    .unwrap_or_else(|f| bail_fail(f));
                reloc = recorded.clone();
            }
            // 只写了全局游标的记录：单条目作业里它就是本条目已完成的块数
            None if resumed => reloc.chunks_in_entry = journal.progress().transfer_cursor,
            None => {}
        }
    }
    if let Some(journal) = src.record_journal_mut() {
        journal.bind_old_range((old_start, old_end)).unwrap_or_else(|e| {
            bail_fail(RunOutcome::refused(format!(
                "cannot record the pre-move range: {e}"
            )))
        });
        journal.bind_relocation(reloc, None).unwrap_or_else(|e| {
            bail_fail(RunOutcome::refused(format!(
                "cannot record the relocation plan: {e}"
            )))
        });
    }
    let mut sink = crate::recovery::sink::RecordSink::quiet();
    if let Err(f) = crate::execution::execute_plan_complete(&mut src, &plan, Some(chunk), &mut sink) {
        bail_fail(f);
    }
    // 起点变了的 NTFS 回填由执行器在表提交之后做（`execution::fix_hidden_sectors`）：
    // 那里的文件系统识别取落点上的现状，比搬移前的旧区间更可靠。已提交态续跑的两处
    // 回填在 resize-part 与 move 的收尾分支直接调 movepart，不经计划步骤
    let o = table_record_done_if_complete(&mut src, RunOutcome::applied_with(Vec::new()));
    if o.is_complete() {
        println!("moved (verify with: diskedit info {})", a.target);
    }
    o
}

pub(crate) fn cmd_copy(a: &CopyRequest) -> crate::outcome::RunOutcome {
    let (pref, selector) = require_start(a.part, a.start, "copy");
    refuse_align_on_start_end(a.start, a.align_given);
    // copy 不读不写 checkpoint（见 copy 的详助 "No resume"）：它没有可续跑的东西，
    // 也就没有资格接管任何未收尾的现场。走严格打开——目标上有任何 active 现场即拒绝，
    // 绝不出现"copy 成功后把别人的记录当自己的清掉、而那份记录原样留在槽里"
    // 载体必须是整盘：条目按节点 LBA0 的偏移读写（判据与 new/add/create 同源）
    refuse_partition_node_target("copy", &a.target);
    let mut src = open_table_record(a, crate::recovery::OpKind::Copy).unwrap_or_else(|f| bail_fail(f));
    // 坐标系在几何计算前确定：copy 仅支持 GPT，条目按表头 ss 对齐（可与容器 ss 不同）
    let (g, repair) = crate::gpt_policy::require_gpt_geometry(&src, "copy").unwrap_or_else(|f| bail_fail(f));
    // 锁下快照
    let part = crate::gpt_policy::resolve_part_in(crate::gpt_policy::TableEntries::Gpt(g.entries()), pref)
        .unwrap_or_else(|f| bail_fail(f));
    let start = match selector {
        StartSelector::PinnedToEnd => {
            let e = crate::gpt_policy::live_entry(&g, part).unwrap_or_else(|f| bail_fail(f));
            let len = e.ending_lba - e.starting_lba + 1;
            g.last_usable_lba()
                .checked_sub(len - 1)
                .unwrap_or_else(|| bail_fail(RunOutcome::refused("partition longer than usable range".to_string())))
        }
        StartSelector::Lba(lba) => align_start(a.align, lba, g.sector_size()),
    };
    // copy 总是把源字节写到落点，落点原有内容无法用 undo 找回
    confirm::gate(a.yes, confirm::Impact { copying: true, ..Default::default() });
    let (chunk, mut logger) = chunk_logger(a, &src, g.disk_guid());
    // 名字守卫在消费点：copy 的落盘名字与 add 同一个 36 码元字段
    if let Some(n) = &a.name { crate::support::check_gpt_name_len(n); }
    // 源区间与摘要随记录落盘，供 undo 复核。源与副本同属本目标，源身份就是记录里的
    // target 身份（严格打开时已比对），不另存一份；摘要取一次完整读，copy 自身还要再读
    let entry = crate::gpt_policy::live_entry(&g, part).unwrap_or_else(|f| bail_fail(f));
    let source = crate::recovery::CopySource {
        partition: part,
        first_lba: entry.starting_lba,
        sectors: entry.ending_lba - entry.starting_lba + 1,
        sector_size: g.sector_size(),
        digest: range_sha256(
            &src,
            lba_bytes(entry.starting_lba, g.sector_size()),
            lba_range_bytes(entry.starting_lba, entry.ending_lba, g.sector_size()),
        )
        .unwrap_or_else(|e| bail_fail(RunOutcome::no_mutation(format!(
            "cannot read partition #{part} to digest the copy source ({e})"
        )))),
    };
    // 绑定即搬移前的复核：未绑定则写入，已绑定则逐字段比对，不同即拒绝（拒时记录未落盘）
    if let Some(journal) = src.record_journal_mut() {
        journal.bind_source(source).unwrap_or_else(|why| bail_fail(RunOutcome::refused(why)));
    }
    // 事务入口的注入点与 apply / resize_part 同形：durable boundary 之前 abort 必须
    // 留下完全未改动的盘
    movepart::fault_before_any_write();
    let p = movepart::prepare_copy(&g, repair, part, start, a.keep_partuuid)
        .unwrap_or_else(|f| bail_fail(f));
    // 修复先于任何目标写入；放 RunOutcome 域以保住分类（同 apply / move）
    if let Some(what) = p.repair.describe() {
        crate::gpt_policy::apply_repair(&mut src, &p.repair).unwrap_or_else(|f| bail_fail(f));
        logger.log(&format!("[repair] {what}"));
    }
    // 修复后重读并写入副本条目：表内容与写前快照都取自重读后的表，修复自身的写入
    // 不夹在两者之间（与 add 的几何准备同源）
    let (geometry, num) = crate::gpt_policy::add_entry_geometry(
        &mut src,
        None,
        p.new_start,
        p.new_end,
        a.name.as_deref().unwrap_or(""),
        p.type_guid,
        p.unique_guid,
    )
    .unwrap_or_else(|f| bail_fail(f));
    let plan = crate::assembly::assemble_copy(&src, &geometry, &p).unwrap_or_else(|f| bail_fail(f));
    // copy 没有续跑（见详助 "No resume"），进度不落记录：复制与表提交成对完成或整体失败
    match crate::execution::execute_plan_complete(&mut src, &plan, Some(chunk), &mut crate::execution::NoProgress) {
        Ok(()) => {
            logger.log(&format!("partition {part} copied to #{num} at {}..{}", p.new_start, p.new_end));
            table_record_done(&mut src, &format!("copied to partition #{num} (verify with: diskedit info {})", a.target))
        }
        Err(f) => bail_fail(f),
    }
}
