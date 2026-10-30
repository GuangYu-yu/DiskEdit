//! 文件系统类命令：mkfs / resizefs / check / set（FS 属性部分）。

use crate::support::*;
use crate::args::{parse_size_delta, Args};
use crate::dev::PartSelector;
use crate::fsid::FsKind;
use crate::outcome::Outcome;
use crate::{fsid, fsops};

pub(crate) const HELP_MKFS: &str = r#"diskedit mkfs <TARGET>:N <FS> --yes

  Create a filesystem. Destroys all data on the partition; stale signatures
  are erased first. Supported: ext2/3/4, xfs, btrfs, f2fs, vfat, exfat,
  ntfs, swap."#;

pub(crate) const HELP_RESIZEFS: &str = r#"diskedit resizefs <TARGET>:N
diskedit resizefs <MOUNTPOINT> [BYTES]

  Resize a filesystem. The form picks the mode: <TARGET>:N is offline and grows
  the FS into its partition; a mountpoint is online and operates on the mounted
  partition. No growable filesystem in the region is refused: an LVM PV takes
  its space through `resize --grow-lv`, and an unidentified type has no action
  to take. The OpenWrt overlay layer before its first mount needs nothing here
  (exit 0): fstools creates it at first mount, filling the trailing region.

  Online form: the partition table is never changed; the filesystem is grown
  inside the current partition (btrfs also shrinks). A size beyond the current
  partition is refused — run `diskedit resize` to enlarge the partition first.
  The target size is absolute (units b/k/m/g/t, 1024 base); omitted means grow
  to fill the partition."#;

pub(crate) const HELP_CHECK: &str = r#"diskedit check <TARGET>:N

  Check filesystem consistency. The tool depends on the type: ext runs
  e2fsck -fp, ntfs runs ntfsfix -d. Both may write repairs back to the
  filesystem, so this needs the target to be writable."#;

pub(crate) const HELP_SET: &str = r#"diskedit set <TARGET>:N name S | label S | uuid U | flag F on|off
diskedit set <TARGET>:N type GUID | type 0xXX

  Set a partition property.
    name          GPT partition name
    type          GPT: partition type GUID (hyphens optional, e.g.
                  C12A7328-F81F-11D2-BA4B-00A0C93EC93B); MBR: type byte (0xXX)
    label / uuid  filesystem label / UUID (FS-aware). --random asks the target
                  to generate a new value (the literal value `random` counts
                  as --random; ntfs only: its serial is not the Windows
                  volume UUID)
    flag          GPT: esp|boot|legacy|legacy_boot|hidden|required — esp/boot on
                  switches the type GUID to EFI System Partition; off switches
                  it back only when the type is currently ESP (falling back to
                  Microsoft basic data for FAT/NTFS content, Linux filesystem
                  data otherwise; the previous type GUID is not preserved).
                  Other type GUIDs are left untouched. MBR: boot|hidden"#;

/// 请求形式与目标能力的比对。能力事实取自 `fsops::uuid_support`（不在此另抄一份），
/// 但"不合能力该不该拒、拿什么措辞拒"是产品的决定，只有命令层能给出"改参数也许有解"
/// 的可读拒绝（退出码 10）
fn check_uuid_request(fstype: Option<FsKind>, req: &fsops::UuidRequest) -> Result<(), String> {
    use fsops::{UuidRequest, UuidSupport};
    let name = fsid::fs_name(fstype);
    match (fsops::uuid_support(fstype), req) {
        (UuidSupport::No(reason), _) => Err(format!("cannot set uuid on {name}: {reason}")),
        (UuidSupport::RandomOnly, UuidRequest::Explicit(_)) => Err(format!(
            "{name} exposes a settable serial, not a volume UUID — it cannot take a chosen value; \
             use --random to generate a new one (a serial is not the Windows volume UUID)"
        )),
        (UuidSupport::Yes, UuidRequest::NewRandom) => {
            Err(format!("--random is not supported for {name}; pass an explicit UUID value"))
        }
        _ => Ok(()),
    }
}

pub(crate) fn cmd_mkfs(a: &Args) -> Outcome {
    // FS 名是第二个位置参数（第一个是 <TARGET>:N，已解析进 a.target/a.part）
    let (Some(pref), Some(fstype_name)) = (a.part, a.pos.get(1).cloned()) else { crate::args::usage() };
    // 分区节点不是整盘：`:N` 的解析会把节点首扇区当分区表读，随后的擦写落在
    // 一个由伪表算出的区间上。判据与 new/add/undo 等命令同一处
    crate::support::refuse_partition_node_target("mkfs", &a.target);
    if !a.yes {
        bail_fail(Fail::refused("mkfs destroys all data on the target partition; pass --yes to confirm"));
    }
    // 先问类型认不认得、工具在不在，再落不可回滚屏障：一个拼错的类型名、一个没装的
    // 工具包，都不该把目标锁进"未收尾"状态、要用户再跑一次 abandon。检查放在开事务
    // **之后**：目标被别人的未收尾现场占着时，busy 的闸口文案（含出路指引）比
    // "工具缺失"更该先到达——journal 是惰性的，此处拒绝同样不留任何痕迹
    let mut src = open_target_for_write(a).unwrap_or_else(|f| bail_fail(f));
    // 锁下快照
    let part = crate::gpt_policy::resolve_part(&src, pref).unwrap_or_else(|f| bail_fail(f));
    let part_off = match crate::gpt_policy::partition_bytes(&src, part) {
        Ok((off, _len)) => off,
        Err(f) => bail_fail(f),
    };
    let fstype = match fsops::parse_fstype(&fstype_name) {
        Ok(k) => k,
        Err(e) => bail_fail(Fail::from(e)),
    };
    if let Err(e) = fsops::mkfs_capability(fstype) {
        bail_fail(Fail::from(e));
    }
    // linux/root 是纯只读判定，失败必然"一个字节都没写"——必须落在屏障之前：屏障一旦
    // 落地，零写入的失败也会留下一个只能靠 abandon 收拾的现场（见下）。镜像的 losetup
    // 属于设备建立，在屏障之后，其失败不在"确定未写盘"的范围内
    if let Err(e) = fsops::require_linux().and_then(|()| fsops::require_root()) {
        bail_fail(Fail::from(e));
    }
    // 占用闸必须在屏障之前：镜像被 loop 占用（或块设备分区被挂载/活动 swap）是
    // "请求与现状不符"，此刻一个字节都没写；放到 fsops::mkfs 里探测则是屏障之后，
    // 拒绝会以 Infra(30) 退出并把目标锁进只能 abandon 的现场
    if let Err(f) = fsops::ensure_idle_before_write(&src, part, part_off, fsops::WriteKind::Overwrite) {
        bail_fail(f);
    }
    // 屏障不在这里：建 loop、定位分区节点、开设备都会在真正擦除之前失败，把屏障放在
    // 命令层会让这些"确定未写盘"的失败也留下一个只能 abandon 的现场。它落在
    // fsops::mkfs 内、紧贴第一次擦除
    match fsops::mkfs(&mut src, part, fstype) {
        // 分区表没动，事务已完成：无待办、无需内核重读
        Ok(()) => Outcome::applied_with(Vec::new()),
        // mkfs 的契约：越过屏障之后的失败一律是 CommandFailed，故分类照用即可——
        // Refused/Infra 只可能来自擦除之前的步骤
        Err(e) => bail_fail(Fail::from(e)),
    }
}

pub(crate) fn cmd_resizefs(a: &Args) -> u8 {
    // 目标形态即模式：带 :N 的是镜像/块设备（离线，把 FS 扩进分区），不带的是挂载点
    // （在线，经挂载点解析）。模式由形态唯一确定，不另设一个可以与之矛盾的旗标
    match a.part {
        Some(pref) => resizefs_offline(a, pref),
        None => resizefs_online(a),
    }
}

/// 在线形式：目标就是挂载点，不打开分区表，扇区大小因而无消费点
fn resizefs_online(a: &Args) -> u8 {
    if a.sector_size.is_some() {
        bail_fail(Fail::refused("--sector-size only applies to image targets — a mountpoint form takes no sector size"));
    }
    // 目标尺寸只有位置参数一种给法，且只接受绝对值（`+N` 锚定"当前尺寸"在在线路径
    // 另有含义，混进来会让同一个字面量在两种命令下取不同值）
    let size = a.pos.get(1).map(|s| match parse_size_delta(s) {
        Some((bytes, 0, false)) => bytes,
        _ => bail_fail(Fail::refused(format!("size {s:?} must be an absolute size (use 10G | 500M | plain bytes)"))),
    });
    #[cfg(target_os = "linux")]
    {
        let o = crate::online::resize_online(std::path::Path::new(&a.target), size);
        if o.is_complete() {
            println!("resized online (verify with: diskedit info {})", a.target);
        } else {
            o.report();
        }
        o.exit_code()
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = size;
        bail_fail(Fail::infra("online resize requires Linux".to_string()));
    }
}

/// 离线形式：把 FS 扩进所在分区，没有尺寸参数——BYTES 是在线给法，消费不到即拒绝，
/// 否则会报成功却没扩到请求的尺寸
fn resizefs_offline(a: &Args, pref: PartSelector) -> u8 {
    if let Some(s) = a.pos.get(1) {
        bail_fail(Fail::refused(format!("size {s:?} only applies to the mountpoint form — offline resizes the filesystem into its partition")));
    }
    // 载体必须是整盘：分区节点首扇区的引导字节可能被当成一张 MBR，按 :N 取到的区间
    // 与实际分区无关。判据与 mkfs/resize/set 同源
    refuse_partition_node_target("resizefs", &a.target);
    let src = open_target_owned(a).unwrap_or_else(|f| bail_fail(f));
    // 锁下快照
    let part = crate::gpt_policy::resolve_part(&src, pref).unwrap_or_else(|f| bail_fail(f));
    let (start, len) = crate::gpt_policy::partition_bytes(&src, part).unwrap_or_else(|f| bail_fail(f));
    // 与 mkfs 同判据：扩 FS 是外部写入，未收尾的恢复现场必须先收拾
    refuse_if_pending_recovery(&src, "resizefs").unwrap_or_else(|f| bail_fail(f));
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
            // 故只报"没有可扩的文件系统"，不替用户断言是哪一种
            println!(
                "nothing to resize on partition #{part} — no growable filesystem was identified in the trailing region \
                 (unformatted, unrecognised, or too short to hold a signature — this tool does not tell them apart)"
            );
            return EXIT_OK;
        }
        // 页格式在本机激活不了的 swap：不是"里面没有 FS"，但也不是本命令扩得了的 FS——
        // 用了新空间要靠重建那块区域（与 `resize` 那条 Pending 的补救提示同一件事）
        Ok(fsops::Growable::SwapUnactivatable) => bail_fail(Fail::refused(format!(
            "nothing to resize on partition #{part} — a swap area whose page format is not activatable on this host (its space takes effect once the area is rebuilt with `mkswap`)"
        ))),
        // 没有可扩的文件系统就报成功等于报出一件没做过的事；PV 另有出路，指路到那条链
        Ok(fsops::Growable::NoFilesystem(Some(FsKind::Lvm2Pv))) => bail_fail(Fail::refused(format!(
            "nothing to resize on partition #{part} — an LVM PV is not a filesystem (its space takes effect through `resize --grow-lv`)"
        ))),
        // 类型未识别既可能是空区域，也可能是认不出的 FS，或可供识别的字节不足：
        // 把识别结果原样带出，不替它下"里面没有文件系统"的结论
        Ok(fsops::Growable::NoFilesystem(f)) => bail_fail(Fail::refused(format!(
            "nothing to resize on partition #{part} — no growable filesystem was identified (identified as `{}`; \
             the region may be empty, hold a format this tool does not recognise, or be too short to carry a \
             signature)",
            fsid::fs_name(f)
        ))),
        Err(e) => bail_fail(Fail::from(e)),
    };
    // 事前能力探测：与 superfloppy 路径同一契约（见 cmd/resize 的同名判定——check_grow
    // 必须在首次写盘之前）。扩 FS 的修复步骤（e2fsck -fp 等）本身写盘，缺工具若在那时
    // 才暴露，Infra(30) 会谎报"确定未写盘"。类型接线判断不在此拦：不接线的类型不会写盘，
    // 由 resize_fs_in 按类型给出准确的拒绝文案
    match fsops::check_grow(target.fstype, src.is_block, fsops::needs_loop(&target.scope, src.is_block)) {
        Err(fsops::FsError::UnsupportedFs(_)) => {}
        other => other.unwrap_or_else(|e| bail_fail(Fail::from(e))),
    }
    match target.resize_fs(&src) {
        Ok(leak) => {
            // 挂载点没卸掉是一条未完成的后置条件：它随结果浮出，退出码因此是 20。
            // 整体成功文案只由 is_complete() 放行；有待办时只报"FS 扩容这一步做完"
            let pending = match leak {
                Some(leak) => {
                    eprintln!("warning: {leak}");
                    vec![leak.into_pending(part)]
                }
                None => Vec::new(),
            };
            let out = Outcome::applied_with(pending);
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
            // 待办必须让人看到：本命令只返回退出码，没有别处会把它呈现出来
            out.report();
            out.exit_code()
        }
        Err(e) => bail_fail(Fail::from(e)),
    }
}

pub(crate) fn cmd_check(a: &Args) -> u8 {
    let Some(pref) = a.part else { crate::args::usage() };
    // e2fsck/ntfsfix 会写回 FS：分区节点上的 :N 不是盘级表项，键按伪 MBR 解析会落到
    // 该分区自己的引导扇区。判据与 mkfs/resize/set 同源
    refuse_partition_node_target("check", &a.target);
    let src = open_target_owned(a).unwrap_or_else(|f| bail_fail(f));
    // 锁下快照
    let part = crate::gpt_policy::resolve_part(&src, pref).unwrap_or_else(|f| bail_fail(f));
    let (start, len) = crate::gpt_policy::partition_bytes(&src, part).unwrap_or_else(|f| bail_fail(f));
    // e2fsck -fp / ntfsfix -d 会把修复写进 FS（不经 journal）：同 mkfs 的理由
    refuse_if_pending_recovery(&src, "check").unwrap_or_else(|f| bail_fail(f));
    let fstype = fsid::identify(&src, start, len).unwrap_or_else(|e| bail_fail(Fail::infra(format!("identify failed: {e}"))));
    match fsops::check_fs(&src, part, fstype) {
        Ok(()) => {
            println!("check done on partition #{part} ({})", fsid::fs_name(fstype));
            EXIT_OK
        }
        Err(e) => bail_fail(Fail::from(e).context("check failed")),
    }
}

/// set 的一键入口：统一 name/label/uuid/flag 四类属性
pub(crate) fn cmd_set(a: &Args) -> Outcome {
    let Some(pref) = a.part else { crate::args::usage() };
    let key = a.pos.get(1).map(|s| s.as_str()).unwrap_or_else(|| crate::args::usage());
    // --random 是 `uuid` 子命令的专属旗标，其余分支不消费它——白名单是命令级的，
    // 分支级差异必须在此显式拒绝（同 resizefs 离线对位置 BYTES 的处置）
    if a.random && key != "uuid" {
        bail_fail(Fail::refused(format!("--random only applies to `set <target>:N uuid`, not `{key}`")));
    }
    let value = a.pos.get(2).cloned().unwrap_or_default();
    let state = a.pos.get(3).cloned().unwrap_or_default();
    // state 是 `flag` 分支的专属位（on|off）：其余分支不消费它，静默吞掉
    // 会让 `set img:1 name foo bar` 这类笔误看起来成功了
    if key != "flag" && !state.is_empty() {
        bail_fail(Fail::refused(format!(
            "`set {key}` takes no state argument — only `set <target>:N flag <flag> on|off` does"
        )));
    }
    // 载体必须是整盘：MBR 的 type/flag 分支直接改目标 LBA0 的记录槽，而分区节点上被
    // parse_mbr 认成 msdos 的可能只是 FAT/NTFS 引导扇区（末尾同为 0x55AA）——写入会
    // 落进文件系统的引导扇区。判据与 new/add/delete 同源
    refuse_partition_node_target("set", &a.target);
    let mut src = open_target_for_write(a).unwrap_or_else(|f| bail_fail(f));
    // 锁下快照
    let part = crate::gpt_policy::resolve_part(&src, pref).unwrap_or_else(|f| bail_fail(f));
    // name/type/flag 直接改表，走整盘占用闸；label/uuid 写的是 FS 内容，由
    // with_partition_device 自己的占用判定把关（它同样要求目标空闲）
    if matches!(key, "name" | "type" | "flag")
        && let Err(f) = fsops::ensure_table_write_idle(&src)
    {
        bail_fail(f);
    }
    if key == "name" {
        if value.is_empty() { crate::args::usage(); }
        crate::support::check_gpt_name_len(&value);
        return match crate::gpt_policy::rename_entry(&mut src, part, &value) {
            Ok(()) => table_write_done(&src, &format!("renamed partition #{part} to {value:?}")),
            Err(f) => bail_fail(f),
        };
    }
    if key == "type" {
        if value.is_empty() { crate::args::usage(); }
        let r = match crate::table::table_label(&src) {
            Ok(crate::table::TableLabel::Gpt) => {
                let Some(guid) = crate::support::parse_guid(&value) else {
                    bail_fail(Fail::refused(format!("invalid GUID {value:?} (expect standard text like C12A7328-F81F-11D2-BA4B-00A0C93EC93B, hyphens optional)")));
                };
                crate::gpt_policy::set_type_guid(&mut src, part, guid)
            }
            Ok(crate::table::TableLabel::Mbr) => match crate::support::parse_os_type(&value) {
                Some(t) => crate::table::set_mdos_type(&mut src, part, t),
                None => bail_fail(Fail::refused(format!("invalid MBR type {value:?} (expect 0xXX)"))),
            },
            Ok(other) => bail_fail(Fail::refused(format!("cannot set type on {other} label"))),
            Err(e) => bail_fail(Fail::infra(format!("label probe failed: {e}"))),
        };
        return match r {
            Ok(()) => table_write_done(&src, &format!("type set on partition #{part} (verify with: diskedit info {})", a.target)),
            Err(f) => bail_fail(f),
        };
    }
    if key == "flag" {
        if value.is_empty() { crate::args::usage(); }
        let on = match state.as_str() { "on" => true, "off" => false, _ => crate::args::usage() };
        let r = match crate::table::table_label(&src) {
            Ok(crate::table::TableLabel::Gpt) => crate::gpt_policy::GptFlag::parse(&value)
                .and_then(|f| crate::gpt_policy::set_gpt_flag(&mut src, part, f, on)),
            Ok(crate::table::TableLabel::Mbr) if value == "boot" => crate::table::set_mdos_boot(&mut src, part, on),
            Ok(crate::table::TableLabel::Mbr) if value == "hidden" => crate::table::set_mdos_hidden(&mut src, part, on),
            Ok(crate::table::TableLabel::Mbr) => bail_fail(Fail::refused("msdos flags: only `boot` and `hidden` are supported".to_string())),
            Ok(other) => bail_fail(Fail::refused(format!("cannot set flag on {other} label"))),
            Err(e) => bail_fail(Fail::infra(format!("label probe failed: {e}"))),
        };
        return match r {
            Ok(()) => table_write_done(&src, &format!("flag {value}={on} on partition #{part}")),
            Err(f) => bail_fail(f),
        };
    }
    // label/uuid 需要 FS 识别
    let (start, len) = crate::gpt_policy::partition_bytes(&src, part).unwrap_or_else(|f| bail_fail(f));
    let fstype = fsid::identify(&src, start, len).unwrap_or_else(|e| bail_fail(Fail::infra(format!("identify failed: {e}"))));
    let r = match key {
        "label" if !value.is_empty() => fsops::set_label(&src, part, fstype, &value),
        "uuid" => {
            let req = match (a.random, value.is_empty()) {
                (true, false) => bail_fail(Fail::refused("`uuid` takes either a value or --random, not both".to_string())),
                (true, true) => fsops::UuidRequest::NewRandom,
                // 字面量 `random` 与 `--random` 归入同一请求形式：生成随机值是同一个意图，
                // 必须过同一套能力矩阵——否则 ext 上 `--random` 被拒而 `uuid random` 过闸，
                // 同一意图两条路径结论相反
                (false, false) if value == "random" => fsops::UuidRequest::NewRandom,
                (false, false) => fsops::UuidRequest::Explicit(value.clone()),
                // 既没给值也没给 --random：无从知道用户要什么
                (false, true) => crate::args::usage(),
            };
            if let Err(msg) = check_uuid_request(fstype, &req) {
                bail_fail(Fail::refused(msg));
            }
            fsops::set_uuid(&src, part, fstype, &req)
        }
        _ => crate::args::usage(),
    };
    match r {
        Ok(()) => {
            println!("set {key} on partition #{part} ({})", fsid::fs_name(fstype));
            // label/uuid 写在 FS 内容里，分区表未动：事务完成且无待办
            Outcome::applied_with(Vec::new())
        }
        Err(e) => bail_fail(Fail::from(e).context(&format!("set {key} failed"))),
    }
}

#[cfg(test)]
mod tests {
    use super::check_uuid_request;
    use crate::fsid::FsKind;
    use crate::fsops::{UuidRequest, UuidSupport};

    /// UUID 请求与目标能力的比对：能设值的类型只接受显式值，ntfs 只接受"生成新值"，
    /// 其余不接线。ntfs 收到具体值必须**拒**——静默丢弃用户给的值正是要防的漂移
    #[test]
    fn uuid_request_capability_matrix() {
        for f in [FsKind::Ext, FsKind::Xfs, FsKind::Btrfs] {
            assert!(check_uuid_request(Some(f), &UuidRequest::Explicit("x".into())).is_ok(), "{f}");
            assert!(check_uuid_request(Some(f), &UuidRequest::NewRandom).is_err(), "{f}");
        }
        assert!(check_uuid_request(Some(FsKind::Ntfs), &UuidRequest::NewRandom).is_ok());
        let e = check_uuid_request(Some(FsKind::Ntfs), &UuidRequest::Explicit("x".into())).unwrap_err();
        assert!(e.contains("--random"), "{e}");
        assert!(check_uuid_request(Some(FsKind::Vfat), &UuidRequest::Explicit("x".into())).is_err());
        // 判据同源：gate 读的是 fsops 的能力表，不是命令层另抄的一份
        assert!(matches!(crate::fsops::uuid_support(Some(FsKind::Ntfs)), UuidSupport::RandomOnly));
        assert!(matches!(crate::fsops::uuid_support(Some(FsKind::Ext)), UuidSupport::Yes));
        assert!(matches!(crate::fsops::uuid_support(None), UuidSupport::No(_)));
    }
}