//! 文件系统类命令：mkfs / resizefs / check / set（FS 属性部分）。

use crate::cli::{CheckRequest, MkfsRequest, ResizefsRequest, SetRequest};
use crate::support::*;
use crate::dev::PartSelector;
use crate::fsid::ContentKind;
use crate::outcome::RunOutcome;
use crate::{fsid, fsops};

pub(crate) const DETAILS_MKFS: &str = r#"  Create a filesystem. Destroys all data on the partition; stale signatures
  are erased first. Supported: ext2/3/4, xfs, btrfs, f2fs, vfat, exfat,
  ntfs, swap."#;

pub(crate) const DETAILS_RESIZEFS: &str = r#"  Resize a filesystem. The form picks the mode: <TARGET>:N is offline and grows
  the FS into its partition; a mountpoint is online and operates on the mounted
  partition. No growable filesystem in the region is refused: an LVM PV takes
  its space through `resize --grow-lv`, and an unidentified type has no action
  to take. The OpenWrt overlay layer before its first mount needs nothing here
  (exit 0): fstools creates it at first mount, filling the trailing region.

  Online form: the partition table is never changed; the filesystem is grown
  inside the current partition (btrfs also shrinks). A size beyond the current
  partition is refused — run `diskedit resize` to enlarge the partition first.
  The target size is absolute (units k/m/g/t, 1024 base; plain bytes when no
  unit; fractions allowed); omitted means grow to fill the partition."#;

pub(crate) const DETAILS_CHECK: &str = r#"  Check filesystem consistency without writing. The tool depends on the type
  and always runs in its read-only form: ext e2fsck -n, ntfs ntfsfix -n,
  xfs xfs_repair -n, btrfs check, vfat/exfat fsck -n. A non-zero tool exit
  is a failed check (exit 3)."#;

pub(crate) const DETAILS_SET: &str = r#"  Set a partition property.
    name          GPT partition name
    type          GPT: partition type GUID (hyphens optional, e.g.
                  C12A7328-F81F-11D2-BA4B-00A0C93EC93B); MBR: type byte (0xXX)
    label / uuid  filesystem label / UUID (FS-aware). --random asks the target
                  to generate a new value (the literal value `random` counts
                  as --random). The tool form differs by filesystem: ext
                  tune2fs -U random, xfs xfs_admin -U generate, btrfs
                  btrfstune -u; swap has no generate form; ntfs's serial is
                  not the Windows volume UUID
    flag          GPT: esp|boot|legacy|legacy_boot|hidden|required — esp/boot on
                  switches the type GUID to EFI System Partition; off switches
                  it back only when the type is currently ESP (falling back to
                  Microsoft basic data for FAT/NTFS content, Linux filesystem
                  data otherwise; the previous type GUID is not preserved).
                  Other type GUIDs are left untouched. MBR: boot|hidden"#;

/// 请求形式与目标能力的比对。能力事实取自 `fsops::uuid_support`（不在此另抄一份），
/// 但"不合能力该不该拒、拿什么措辞拒"由命令层给出"改参数也许有解"
/// 的可读拒绝（退出码 3）
fn check_uuid_request(fstype: Option<ContentKind>, req: &fsops::UuidRequest) -> Result<(), String> {
    use fsops::{UuidRequest, UuidSupport};
    let name = fsid::content_name(fstype.unwrap_or(ContentKind::Unknown));
    match (fsops::uuid_support(fstype), req) {
        (UuidSupport::No(reason), _) => Err(format!("cannot set uuid on {name}: {reason}")),
        (UuidSupport::GeneratedOnly, UuidRequest::Explicit(_)) => Err(format!(
            "{name} exposes a settable serial, not a volume UUID — it cannot take a chosen value; \
             use --random to generate a new one (a serial is not the Windows volume UUID)"
        )),
        (UuidSupport::ExplicitOnly, UuidRequest::NewRandom) => Err(format!(
            "{name} has no generate form — pass an explicit UUID value"
        )),
        _ => Ok(()),
    }
}

pub(crate) fn cmd_mkfs(a: &MkfsRequest) -> RunOutcome {
    // 分区与 FS 名由命令行 schema 取齐：`<TARGET>:N` 与 `<FS>`
    let Some(pref) = a.part else {
        bail_fail(RunOutcome::refused("`mkfs` needs a partition: diskedit mkfs <TARGET>:N <FS> (see diskedit help mkfs)".to_string()));
    };
    let fstype_name = a.fstype.clone();
    // 分区节点不是整盘：`:N` 的解析会把节点首扇区当分区表读，随后的擦写落在
    // 一个由伪表算出的区间上。判据与 new/add/undo 等命令同一处
    crate::support::refuse_partition_node_target("mkfs", &a.target);
    if !a.yes {
        bail_fail(RunOutcome::refused("mkfs destroys all data on the target partition; pass --yes to confirm"));
    }
    // 先问类型认不认得、工具在不在，再落不可回滚屏障：一个拼错的类型名、一个没装的
    // 工具包，都不该把目标锁进"未收尾"状态、要用户再跑一次 abandon。检查放在开事务
    // **之后**：目标被别人的未收尾现场占着时，busy 的闸口文案（含出路指引）比
    // "工具缺失"更该先到达。记录在第一次目标写入之前不落盘，因此这些拒绝不会留下
    // 空记录占住落点
    let (mut src, _resumed) =
        open_record_resumable(a, crate::recovery::OpKind::Mkfs).unwrap_or_else(|f| bail_fail(f));
    // 锁下快照
    let part = crate::gpt_policy::resolve_part(&src, pref).unwrap_or_else(|f| bail_fail(f));
    let part_off = match crate::gpt_policy::partition_bytes(&src, part) {
        Ok((off, _len)) => off,
        Err(f) => bail_fail(f),
    };
    let fstype = match fsops::parse_fstype(&fstype_name) {
        Ok(k) => k,
        Err(e) => bail_fail(RunOutcome::from(e)),
    };
    if let Err(e) = fsops::mkfs_capability(fstype) {
        bail_fail(RunOutcome::from(e));
    }
    // 卷标是建 FS 参数的一部分：容量判据与工具能力一样属于"动手前可判定的可执行性"，
    // 必须落在擦除第一个字节之前——否则会出现"FS 已建成、卷标被工具拒绝"的中间态
    if let Some(label) = a.label.as_deref()
        && let Err(e) = fsops::check_label_limits(Some(fstype.as_content_kind()), label)
    {
        bail_fail(RunOutcome::from(e));
    }
    // linux/root 是纯只读判定，失败必然"一个字节都没写"——必须落在屏障之前：屏障一旦
    // 落地，零写入的失败也会留下一个只能靠 abandon 收拾的现场。镜像的 losetup
    // 属于设备建立，在屏障之后，其失败不在"确定未写盘"的范围内
    if let Err(e) = fsops::require_linux().and_then(|()| fsops::require_root()) {
        bail_fail(RunOutcome::from(e));
    }
    // 占用闸必须在屏障之前：镜像被 loop 占用（或块设备分区被挂载/活动 swap）是
    // "请求与现状不符"，此刻一个字节都没写；放到 fsops::mkfs 里探测就是屏障之后，
    // 那次拒绝只能按 Partial 报，"确定未写盘"这句话随之失效
    if let Err(f) = fsops::ensure_idle_before_write(&src, part, part_off, fsops::WriteKind::Overwrite) {
        bail_fail(f);
    }
    // 续跑只认同一份请求：分区、类型名、标签及其"是否给出"都决定写入结果
    let digest = crate::recovery::PlanDigest::of_request_fields(
        crate::recovery::OpKind::Mkfs,
        &[u64::from(part), u64::from(a.label.is_some()), src.sector_size],
        &[fstype_name.as_bytes(), a.label.as_deref().unwrap_or("").as_bytes()],
    );
    if let Some(journal) = src.record_journal_mut()
        && let Err(why) = journal.bind_plan(digest)
    {
        bail_fail(RunOutcome::refused(why));
    }
    // 屏障不在这里：建 loop、定位分区节点、开设备都会在真正擦除之前失败，把屏障放在
    // 命令层会让这些"确定未写盘"的失败也留下一个只能 abandon 的现场。它落在
    // fsops::mkfs 内、紧贴第一次擦除，执行器只负责把计划里的几何与类型交给它
    // label=None 不传卷标参数，工具写的就是空标签，不造默认值
    let plan = crate::assembly::assemble_fs_create(&src, part, fstype, a.label.clone())
        .unwrap_or_else(|f| bail_fail(f));
    // 计划里没有搬移步骤，块大小无人消费；FS 步骤也不写游标，sink 不落任何进度
    match crate::execution::execute_plan_complete(&mut src, &plan, None, &mut crate::execution::NoProgress) {
        // FS 内容已写完、表未动：记录在此提交并删除，收尾按 Applied 做一次内核重读
        Ok(()) => table_record_done_if_complete(&mut src, RunOutcome::applied_with(Vec::new())),
        Err(e) => bail_fail(e),
    }
}

pub(crate) fn cmd_resizefs(a: &ResizefsRequest) -> RunOutcome {
    // 目标形态即模式：带 :N 的是镜像/块设备（离线，把 FS 扩进分区），不带的是挂载点
    // （在线，经挂载点解析）。模式由形态唯一确定，不另设一个可以与之矛盾的旗标
    match a.part {
        Some(pref) => resizefs_offline(a, pref),
        // 在线形式的尺寸只有位置参数一种给法，命令行层已解析成绝对字节数
        None => resizefs_online(a, a.bytes),
    }
}

/// 在线形式：目标就是挂载点，不打开分区表，扇区大小因而无消费点
#[cfg(target_os = "linux")]
fn resizefs_online(a: &ResizefsRequest, size: Option<u64>) -> RunOutcome {
    if a.sector_size.is_some() {
        bail_fail(RunOutcome::usage("--sector-size only applies to image targets — a mountpoint form takes no sector size"));
    }
    let o = crate::online::resize_online(std::path::Path::new(&a.target), size);
    if o.is_complete() {
        println!("resized online (verify with: diskedit info {})", a.target);
        o
    } else {
        o.report_final()
    }
}

#[cfg(not(target_os = "linux"))]
fn resizefs_online(_: &ResizefsRequest, _: Option<u64>) -> RunOutcome {
    bail_fail(RunOutcome::no_mutation("online resize requires Linux".to_string()));
}

/// 离线形式：把 FS 扩进所在分区，没有尺寸参数——BYTES 是在线给法，消费不到即拒绝，
/// 否则会报成功却没扩到请求的尺寸
fn resizefs_offline(a: &ResizefsRequest, pref: PartSelector) -> RunOutcome {
    if let Some(s) = a.bytes_text.as_deref() {
        bail_fail(RunOutcome::usage(format!("size {s:?} only applies to the mountpoint form — offline resizes the filesystem into its partition")));
    }
    // 载体必须是整盘：分区节点首扇区的引导字节可能被当成一张 MBR，按 :N 取到的区间
    // 与实际分区无关。判据与 mkfs/resize/set 同源
    refuse_partition_node_target("resizefs", &a.target);
    let (mut src, _resumed) =
        open_record_resumable(a, crate::recovery::OpKind::ResizeFs).unwrap_or_else(|f| bail_fail(f));
    // 锁下快照
    let part = crate::gpt_policy::resolve_part(&src, pref).unwrap_or_else(|f| bail_fail(f));
    let (start, len) = crate::gpt_policy::partition_bytes(&src, part).unwrap_or_else(|f| bail_fail(f));
    // 续跑时只认同一份请求：操作、分区与区间一致（摘要只作请求身份比对）。
    // 早退分支也要用同一份判据——它可以决定删除一条既有记录
    let digest = crate::recovery::PlanDigest::of_request(
        crate::recovery::OpKind::ResizeFs,
        part,
        start,
        len,
        src.sector_size,
        src.size,
    );
    // 要扩哪一段、里面是什么 FS 取的是与 `resize` 同一处的判据（overlay 的 RW 层即由此
    // 落到内层区间）。判据只有一处，两条命令的后置条件却不同：`resize` 的后置条件含
    // 分区层，FS 那一步做不了时它在写表之前就拒绝；本命令只动 FS，同一个结论落在这里
    // 就成了**什么都没做**，报成功会让脚本以为空间已经可用
    let target = match fsops::grow_target_at(&src, part, start, len) {
        Ok(fsops::Growable::Target(t)) => t,
        Ok(fsops::Growable::OverlayPending) => {
            // 这段区间里没有认得出的文件系统：可能是尚未格式化、可能是认不出的格式、
            // 也可能是可供识别的字节不足——`identify` 的 `None` 分不出这三种。本命令在这
            // 三种情形下都无事后置条件可做（该层的空间由首次挂载时的 fstools 建满），
            // 故只报"没有可扩的文件系统"，不替用户断言是哪一种。
            // 仅当记录属于本次请求时，此处才没有可做的事：续跑进来、盘面已无可扩层，
            // 记录的结论与本次请求一致，随本次成功收尾一并清理。记录属于别的请求时
            // 既不能删，也不能带着它继续写
            let persisted = src
                .record_journal()
                .filter(|j| j.is_persisted())
                .map(|j| (j.request_match(digest), j.record_path().to_path_buf()));
            match persisted {
                Some((crate::recovery::RequestMatch::Same, _)) => {
                    if let Some(journal) = src.take_record_journal()
                        && let Err(e) = journal.commit_and_discard()
                    {
                        // 目录项已移除、持久化未确认：只陈述这一事实，不承诺记录还在
                        let cause = if e.record_still_on_disk() {
                            format!(
                                "the recovery record was not cleaned up ({e}) — release it with `diskedit abandon`"
                            )
                        } else {
                            format!("{e}")
                        };
                        let out = RunOutcome::applied_with(Vec::new()).degrade_partial(cause);
                        return out.report_final();
                    }
                }
                Some((verdict, path)) => {
                    let why = if verdict == crate::recovery::RequestMatch::Unbound {
                        "carries no request identity, so this run cannot prove it belongs here"
                    } else {
                        "belongs to a different `resizefs` request (the partition, range, size, or options changed)"
                    };
                    bail_fail(RunOutcome::refused(format!(
                        "the recovery record at {} {why} — run `diskedit inspect {}` or `diskedit undo {}` first",
                        path.display(),
                        a.target,
                        a.target
                    )));
                }
                None => {}
            }
            println!(
                "nothing to resize on partition #{part} — no growable filesystem was identified in the trailing region \
                 (unformatted, unrecognised, or too short to hold a signature — this tool does not tell them apart)"
            );
            return RunOutcome::applied_with(Vec::new());
        }
        // 页格式在本机激活不了的 swap：不是"里面没有 FS"，但也不是本命令扩得了的 FS——
        // 用了新空间要靠重建那块区域（与 `resize` 那条 Pending 的补救提示同一件事）
        Ok(fsops::Growable::SwapUnactivatable) => bail_fail(RunOutcome::refused(format!(
            "nothing to resize on partition #{part} — a swap area whose page format is not activatable on this host (its space takes effect once the area is rebuilt with `mkswap`)"
        ))),
        // 没有可扩的文件系统就报成功等于报出一件没做过的事；PV 另有出路，指路到那条链
        Ok(fsops::Growable::NoFilesystem(Some(ContentKind::Lvm2Pv))) => bail_fail(RunOutcome::refused(format!(
            "nothing to resize on partition #{part} — an LVM PV is not a filesystem (its space takes effect through `resize --grow-lv`)"
        ))),
        // 类型未识别既可能是空区域，也可能是认不出的 FS，或可供识别的字节不足：
        // 把识别结果原样带出，不替它下"里面没有文件系统"的结论
        Ok(fsops::Growable::NoFilesystem(f)) => bail_fail(RunOutcome::refused(format!(
            "nothing to resize on partition #{part} — no growable filesystem was identified (identified as `{}`; \
             the region may be empty, hold a format this tool does not recognise, or be too short to carry a \
             signature)",
            fsid::content_name(f.unwrap_or(ContentKind::Unknown))
        ))),
        Err(e) => bail_fail(RunOutcome::from(e)),
    };
    // 事前能力探测：与 superfloppy 路径同一契约（见 cmd/resize 的同名判定——check_grow
    // 必须在首次写盘之前）。扩 FS 的修复步骤（e2fsck -fp 等）本身写盘，缺工具若在那时
    // 才暴露，本次运行只能按 Partial(5) 报，无法再声称"确定未写盘"。类型接线判断不在此拦：不接线的类型不会写盘，
    // 由 resize_fs_in 按类型给出准确的拒绝文案
    match fsops::check_grow(target.fstype, src.is_block, fsops::needs_loop(&target.scope, src.is_block)) {
        Err(fsops::FsError::UnsupportedFs { .. }) => {}
        other => other.unwrap_or_else(|e| bail_fail(RunOutcome::from(e))),
    }
    // 屏障不在这里：执行器在启动外部工具之前落盘（`execution::fs::execute`）
    if let Some(journal) = src.record_journal_mut()
        && let Err(why) = journal.bind_plan(digest)
    {
        bail_fail(RunOutcome::refused(why));
    }
    let plan = crate::assembly::assemble_fs_resize(&src, part, Some(target.fstype))
        .unwrap_or_else(|f| bail_fail(f));
    // 计划里没有搬移步骤，块大小无人消费；FS 步骤也不写游标，sink 不落任何进度
    match crate::execution::execute_plan_complete(&mut src, &plan, None, &mut crate::execution::NoProgress) {
        Ok(()) => {
            let mut out = RunOutcome::applied_with(Vec::new());
            // 完整结束才提交并删除记录；挂载点未卸下（执行器报 5）时记录留在落点，下一次运行接着用它
            if let Some(journal) = src.take_record_journal()
                && let Err(e) = journal.commit_and_discard()
            {
                // 目录项已移除、持久化未确认：只陈述这一事实，不承诺记录还在
                let cause = if e.record_still_on_disk() {
                    format!("the recovery record was not cleaned up ({e}) — release it with `diskedit abandon`")
                } else {
                    format!("{e}")
                };
                out = out.degrade_partial(cause);
            }
            announce_applied(
                &out,
                || format!("resized (verify with: diskedit info {})", a.target),
                || {
                    format!(
                        "filesystem grow step completed (verify with: diskedit info {}); follow-up steps are pending — see below",
                        a.target
                    )
                },
            );
            // 待办必须让人看到：分派处只取退出码，不会把它呈现出来
            out.report_final()
        }
        Err(f) => bail_fail(f),
    }
}

pub(crate) fn cmd_check(a: &CheckRequest) -> u8 {
    let Some(pref) = a.part else {
        bail_fail(RunOutcome::refused("`check` needs a partition: diskedit check <TARGET>:N (see diskedit help check)".to_string()));
    };
    // 分区节点上的 :N 不是盘级表项，键按伪 MBR 解析会落到该分区自己的引导扇区。
    // 判据与 mkfs/resize/set 同源
    refuse_partition_node_target("check", &a.target);
    // 只读打开，不建状态根、不打开证据路径
    let src = crate::cmd::open_read_only_target(&a.target, a.sector_size).unwrap_or_else(|f| bail_fail(f));
    let part = crate::gpt_policy::resolve_part(&src, pref).unwrap_or_else(|f| bail_fail(f));
    let (start, len) = crate::gpt_policy::partition_bytes(&src, part).unwrap_or_else(|f| bail_fail(f));
    let fstype = fsid::identify(&src, start, len).unwrap_or_else(|e| bail_fail(RunOutcome::no_mutation(format!("identify failed: {e}"))));
    match fsops::check_fs(&src, part, Some(fstype)) {
        Ok(()) => {
            println!("check done on partition #{part} ({})", fsid::content_name(fstype));
            EXIT_OK
        }
        // 外部工具退出非零是拒绝（3），不是"可能已写"（5）
        Err(crate::fsops::FsError::CommandFailed(m)) => bail_fail(RunOutcome::refused(format!("check failed: {m}"))),
        Err(e) => bail_fail(RunOutcome::from(e).context("check failed")),
    }
}

/// set 的一键入口：统一 name/label/uuid/flag 四类属性
pub(crate) fn cmd_set(a: &SetRequest) -> RunOutcome {
    let Some(pref) = a.part else {
        bail_fail(RunOutcome::refused("`set` needs a partition: diskedit set <TARGET>:N <PROPERTY> ... (see diskedit help set)".to_string()));
    };
    let key = a.key.as_str();
    // --random 是 `uuid` 子命令的专属旗标，其余分支不消费它——白名单是命令级的，
    // 分支级差异必须在此显式拒绝（同 resizefs 离线对位置 BYTES 的处置）
    if a.random && key != "uuid" {
        bail_fail(RunOutcome::usage(format!("--random only applies to `set <target>:N uuid`, not `{key}`")));
    }
    let value = a.value.clone();
    let state = a.state.clone();
    // state 是 `flag` 分支的专属位（on|off）：其余分支不消费它，静默吞掉
    // 会让 `set img:1 name foo bar` 这类笔误看起来成功了
    if key != "flag" && !state.is_empty() {
        bail_fail(RunOutcome::refused(format!(
            "`set {key}` takes no state argument — only `set <target>:N flag <flag> on|off` does"
        )));
    }
    // 载体必须是整盘：MBR 的 type/flag 分支直接改目标 LBA0 的记录槽，而分区节点上被
    // parse_mbr 认成 mbr 的可能只是 FAT/NTFS 引导扇区（末尾同为 0x55AA）——写入会
    // 落进文件系统的引导扇区。判据与 new/add/delete 同源
    refuse_partition_node_target("set", &a.target);
    let (mut src, _resumed) =
        open_record_resumable(a, crate::recovery::OpKind::Set).unwrap_or_else(|f| bail_fail(f));
    // 锁下快照
    let part = crate::gpt_policy::resolve_part(&src, pref).unwrap_or_else(|f| bail_fail(f));
    // 续跑只认同一份请求：属性名、值与状态共同决定写入结果
    let digest = crate::recovery::PlanDigest::of_request_fields(
        crate::recovery::OpKind::Set,
        &[u64::from(part), u64::from(a.random)],
        &[key.as_bytes(), value.as_bytes(), state.as_bytes()],
    );
    if let Some(journal) = src.record_journal_mut()
        && let Err(why) = journal.bind_plan(digest)
    {
        bail_fail(RunOutcome::refused(why));
    }
    // name/type/flag 直接改表，走整盘占用闸；label/uuid 写的是 FS 内容，由
    // with_partition_device 自己的占用判定把关（它同样要求目标空闲）
    if matches!(key, "name" | "type" | "flag")
        && let Err(f) = fsops::ensure_table_write_idle(&src)
    {
        bail_fail(f);
    }
    if key == "name" {
        if value.is_empty() {
            bail_fail(RunOutcome::refused("`set ... name` needs a name (see diskedit help set)".to_string()));
        }
        crate::support::check_gpt_name_len(&value);
        let plan = crate::assembly::assemble_set_name(&src, part, &value).unwrap_or_else(|f| bail_fail(f));
        // 计划里只有一条表步骤：块大小无人消费，sink 只把进度与表步围栏写进记录
        return match crate::execution::execute_plan_complete(&mut src, &plan, None, &mut crate::recovery::sink::RecordSink::quiet()) {
            Ok(()) => table_record_done(&mut src, &format!("renamed partition #{part} to {value:?}")),
            Err(f) => bail_fail(f),
        };
    }
    if key == "type" {
        if value.is_empty() {
            bail_fail(RunOutcome::refused("`set ... type` needs a type (GPT: GUID text, MBR: 0xXX; see diskedit help set)".to_string()));
        }
        let plan = match crate::table::table_label(&src) {
            Ok(crate::table::TableLabel::Gpt) => {
                let Some(guid) = crate::support::parse_guid(&value) else {
                    bail_fail(RunOutcome::refused(format!("invalid GUID {value:?} (expect standard text like C12A7328-F81F-11D2-BA4B-00A0C93EC93B, hyphens optional)")));
                };
                crate::assembly::assemble_set_gpt_type(&src, part, guid)
            }
            Ok(crate::table::TableLabel::Mbr) => match crate::support::parse_os_type(&value) {
                Some(t) => crate::assembly::assemble_set_mbr_type(&src, part, t),
                None => bail_fail(RunOutcome::refused(format!("invalid MBR type {value:?} (expect 0xXX)"))),
            },
            Ok(other) => bail_fail(RunOutcome::refused(format!("cannot set type on {other} label"))),
            Err(e) => bail_fail(crate::outcome::table_parse_failure(&e, format!("label probe failed: {e}"))),
        };
        let plan = plan.unwrap_or_else(|f| bail_fail(f));
        // 计划里只有一条表步骤：块大小无人消费，sink 只把进度与表步围栏写进记录
        return match crate::execution::execute_plan_complete(&mut src, &plan, None, &mut crate::recovery::sink::RecordSink::quiet()) {
            Ok(()) => table_record_done(&mut src, &format!("type set on partition #{part} (verify with: diskedit info {})", a.target)),
            Err(f) => bail_fail(f),
        };
    }
    if key == "flag" {
        if value.is_empty() {
            bail_fail(RunOutcome::refused("`set ... flag` needs a flag name and on|off (see diskedit help set)".to_string()));
        }
        let on = match state.as_str() {
            "on" => true,
            "off" => false,
            other => bail_fail(RunOutcome::refused(format!(
                "`set ... flag` takes on or off, not {other:?} (see diskedit help set)"
            ))),
        };
        let plan = match crate::table::table_label(&src) {
            Ok(crate::table::TableLabel::Gpt) => crate::gpt_policy::GptFlag::parse(&value)
                .and_then(|f| crate::assembly::assemble_set_gpt_flag(&src, part, f, on))
                .map(Some),
            Ok(crate::table::TableLabel::Mbr) if value == "boot" => {
                crate::assembly::assemble_set_mbr_boot(&src, part, on).map(Some)
            }
            // 条目已在目标态时装配期给不出写盘内容，直接收尾
            Ok(crate::table::TableLabel::Mbr) if value == "hidden" => {
                crate::assembly::assemble_set_mbr_hidden(&src, part, on)
            }
            Ok(crate::table::TableLabel::Mbr) => bail_fail(RunOutcome::refused("mbr flags: only `boot` and `hidden` are supported".to_string())),
            Ok(other) => bail_fail(RunOutcome::refused(format!("cannot set flag on {other} label"))),
            Err(e) => bail_fail(crate::outcome::table_parse_failure(&e, format!("label probe failed: {e}"))),
        };
        let plan = plan.unwrap_or_else(|f| bail_fail(f));
        if let Some(plan) = plan {
            // 计划里只有一条表步骤：块大小无人消费，sink 只把进度与表步围栏写进记录
            if let Err(f) = crate::execution::execute_plan_complete(&mut src, &plan, None, &mut crate::recovery::sink::RecordSink::quiet()) {
                bail_fail(f);
            }
        }
        return table_record_done(&mut src, &format!("flag {value}={on} on partition #{part}"));
    }
    // label/uuid 需要 FS 识别
    let (start, len) = crate::gpt_policy::partition_bytes(&src, part).unwrap_or_else(|f| bail_fail(f));
    let fstype = fsid::identify(&src, start, len).unwrap_or_else(|e| bail_fail(RunOutcome::no_mutation(format!("identify failed: {e}"))));
    // 请求先变成可执行形式：参数错误必须在屏障之前拒绝，否则会为一次零写入留下记录
    let request = match key {
        "uuid" => {
            let req = match (a.random, value.is_empty()) {
                // 解析期已拒绝同时给出值与 --random 的 argv；这里保留同一判定作为
                // 绕过解析直接构造请求时的防御，码值与解析期一致
                (true, false) => bail_fail(RunOutcome::usage("`uuid` takes either a value or --random, not both".to_string())),
                (true, true) => fsops::UuidRequest::NewRandom,
                // 字面量 `random` 与 `--random` 归入同一请求形式：生成随机值是同一个意图，
                // 必须过同一套能力矩阵——否则 ext 上 `--random` 被拒而 `uuid random` 过闸，
                // 同一意图两条路径结论相反
                (false, false) if value == "random" => fsops::UuidRequest::NewRandom,
                (false, false) => fsops::UuidRequest::Explicit(value.clone()),
                // 既没给值也没给 --random：无从知道用户要什么
                (false, true) => bail_fail(RunOutcome::refused(
                    "`set ... uuid` needs a UUID, or --random to generate one (see diskedit help set)"
                        .to_string(),
                )),
            };
            if let Err(msg) = check_uuid_request(Some(fstype), &req) {
                bail_fail(RunOutcome::refused(msg));
            }
            crate::assembly::FsSetRequest::Uuid(req)
        }
        "label" if !value.is_empty() => crate::assembly::FsSetRequest::Label(value.clone()),
        // label 缺值不是 unknown property：uuid 对缺值有专门文案，同一函数内口径一致
        "label" => bail_fail(RunOutcome::refused(
            "`set ... label` needs a value — the new filesystem label (see diskedit help set)".to_string(),
        )),
        other => bail_fail(RunOutcome::refused(format!(
            "unknown property {other:?} for `set` — use name, label, uuid, flag or type (see diskedit help set)"
        ))),
    };
    // LVM2 PV 不是文件系统：label/uuid 走的是 FS 工具，必须在屏障之前拒绝
    // （「选项与目标形态不符 → 3」）。PV 元数据由 pvchange(8) 改，不在本命令语义内
    if fstype == ContentKind::Lvm2Pv {
        bail_fail(RunOutcome::refused(format!(
            "set {key} on partition #{part} — target is an LVM2 PV, not a filesystem; PV metadata is changed with pvchange(8)"
        )));
    }
    // 屏障不在这里：执行器在启动外部工具之前落盘（`execution::set::execute`）
    let plan = crate::assembly::assemble_fs_set(&src, part, Some(fstype), request)
        .unwrap_or_else(|f| bail_fail(f));
    // 计划里没有搬移步骤，块大小无人消费；FS 步骤也不写游标，sink 不落任何进度
    match crate::execution::execute_plan_complete(&mut src, &plan, None, &mut crate::execution::NoProgress) {
        Ok(()) => {
            println!("set {key} on partition #{part} ({})", fsid::content_name(fstype));
            // label/uuid 由外部工具写 FS 内容，屏障已在写入前落盘；完整结束才提交并删除记录
            table_record_done_if_complete(&mut src, RunOutcome::applied_with(Vec::new()))
        }
        Err(e) => bail_fail(e),
    }
}

#[cfg(test)]
mod tests {
    use super::check_uuid_request;
    use crate::fsid::{ContentKind, FsKind};
    use crate::fsops::{UuidRequest, UuidSupport};

    /// UUID 请求与目标能力的比对：ext/xfs/btrfs 两种形式都接受，swap 只接受显式值，
    /// ntfs 只接受"生成新值"，其余不接线。ntfs 收到具体值必须**拒**——静默丢弃用户
    /// 给的值正是要防的漂移
    #[test]
    fn uuid_request_capability_matrix() {
        for f in [
            ContentKind::Filesystem(FsKind::Ext),
            ContentKind::Filesystem(FsKind::Xfs),
            ContentKind::Filesystem(FsKind::Btrfs),
        ] {
            assert!(check_uuid_request(Some(f), &UuidRequest::Explicit("x".into())).is_ok(), "{f}");
            assert!(check_uuid_request(Some(f), &UuidRequest::NewRandom).is_ok(), "{f}");
        }
        assert!(check_uuid_request(Some(ContentKind::Swap), &UuidRequest::Explicit("x".into())).is_ok());
        let e = check_uuid_request(Some(ContentKind::Swap), &UuidRequest::NewRandom).unwrap_err();
        assert!(e.contains("no generate form"), "{e}");
        assert!(check_uuid_request(Some(ContentKind::Filesystem(FsKind::Ntfs)), &UuidRequest::NewRandom).is_ok());
        let e = check_uuid_request(Some(ContentKind::Filesystem(FsKind::Ntfs)), &UuidRequest::Explicit("x".into())).unwrap_err();
        assert!(e.contains("--random"), "{e}");
        assert!(check_uuid_request(Some(ContentKind::Filesystem(FsKind::Vfat)), &UuidRequest::Explicit("x".into())).is_err());
        // 判据同源：gate 读的是 fsops 的能力表，不是命令层另抄的一份
        assert!(matches!(
            crate::fsops::uuid_support(Some(ContentKind::Filesystem(FsKind::Ntfs))),
            UuidSupport::GeneratedOnly
        ));
        assert!(matches!(
            crate::fsops::uuid_support(Some(ContentKind::Filesystem(FsKind::Ext))),
            UuidSupport::ExplicitAndGenerated
        ));
        assert!(matches!(crate::fsops::uuid_support(Some(ContentKind::Swap)), UuidSupport::ExplicitOnly));
        assert!(matches!(crate::fsops::uuid_support(None), UuidSupport::No(_)));
    }
}