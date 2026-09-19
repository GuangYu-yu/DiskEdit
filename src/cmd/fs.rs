//! 文件系统类命令：mkfs / resizefs / check / set（FS 属性部分）。

use crate::support::*;
use crate::args::{parse_size_delta, Args};
use crate::dev::Mutation;
use crate::{fsid, fsops};

pub(crate) const HELP_MKFS: &str = r#"diskedit mkfs <TARGET>:N <FS> --yes

  Create a filesystem. Destroys all data on the partition; stale signatures
  are erased first. Supported: ext2/3/4, xfs, btrfs, f2fs, vfat, exfat,
  ntfs, swap."#;

pub(crate) const HELP_RESIZEFS: &str = r#"diskedit resizefs <TARGET>:N
diskedit resizefs <MOUNTPOINT> [BYTES | --size SIZE] --online

  Resize a filesystem. Offline form grows the FS into its partition.
  Online form operates on a mounted partition: grow only (btrfs also
  shrinks); the target size is absolute (units b/k/m/g/t, 1024 base) —
  omitted means grow to fill the partition."#;

pub(crate) const HELP_CHECK: &str = r#"diskedit check <TARGET>:N

  Check filesystem consistency (tool-specific; ext runs e2fsck -fp and
  ntfs runs ntfsfix -d, both may write repairs to the filesystem)."#;

pub(crate) const HELP_SET: &str = r#"diskedit set <TARGET>:N name S | label S | uuid U | uuid --random | flag F on|off

  Set a partition property.
    name          GPT partition name
    label / uuid  filesystem label / UUID (FS-aware). --random asks the target
                  to generate a new value (ntfs only: its serial is not the
                  Windows volume UUID)
    flag          GPT: esp|boot|hidden|required ; MBR: boot|hidden"#;

/// 请求形式与目标能力的比对。能力事实取自 `fsops::uuid_support`（不在此另抄一份），
/// 但"不合能力该不该拒、拿什么措辞拒"是产品的决定，只有命令层能给出"改参数也许有解"
/// 的可读拒绝（退出码 10）
fn check_uuid_request(fstype: &str, req: &fsops::UuidRequest) -> Result<(), String> {
    use fsops::{UuidRequest, UuidSupport};
    match (fsops::uuid_support(fstype), req) {
        (UuidSupport::No(reason), _) => Err(format!("cannot set uuid on {fstype}: {reason}")),
        (UuidSupport::RandomOnly, UuidRequest::Explicit(_)) => Err(format!(
            "{fstype} exposes a settable serial, not a volume UUID — it cannot take a chosen value; \
             use --random to generate a new one (a serial is not the Windows volume UUID)"
        )),
        (UuidSupport::Yes, UuidRequest::NewRandom) => {
            Err(format!("--random is not supported for {fstype}; pass an explicit UUID value"))
        }
        _ => Ok(()),
    }
}

pub(crate) fn cmd_mkfs(a: &Args) -> u8 {
    // FS 名是第二个位置参数（第一个是 <TARGET>:N，已解析进 a.target/a.part）
    let (Some(part), Some(fstype)) = (a.part, a.pos.get(1).cloned()) else { crate::args::usage() };
    if !a.yes {
        bail_fail(Fail::refused(format!("mkfs destroys all data on partition {part}; pass --yes to confirm")));
    }
    // 先问类型认不认得：拒绝的语义是"什么都没写"，而下面一开事务就会先落不可回滚屏障——
    // 让一个拼错的类型名把目标锁进"未收尾"状态、要用户再跑一次 abandon 是错的
    if let Err(e) = fsops::mkfs_supported(&fstype) {
        bail_fail(Fail::from(e));
    }
    let mut src = open_target_for_write(a).unwrap_or_else(|f| bail_fail(f));
    if let Err(f) = entry_byte_range(&src, part) {
        bail_fail(f);
    }
    // mkfs 是一次事务，但不可回滚：内容擦掉之后没有"回去"这回事。目标被别人的未收尾现场
    // 占着的话，上面那句 `open_target_for_write` 已经拒绝了。先落屏障（记下"这个分区上创建
    // 了文件系统"）再交给外部工具：成功时 main 关闭事务，失败或崩溃则留下一个 active 的
    // 不可回滚事务，出路是 `abandon`
    src.set_mutation(Mutation::Mkfs);
    src.mark_non_reversible()
        .unwrap_or_else(|e| bail_fail(Fail::infra(format!("cannot persist the transaction state: {e}"))));
    // FS 层的失败分类在此换算成出口语义：不认得的类型 ⇒ 10（改参数有解），
    // 工具缺失 / 环境故障 ⇒ 30
    match fsops::mkfs(&src, part, &fstype) {
        Ok(()) => EXIT_OK,
        Err(e) => bail_fail(Fail::from(e)),
    }
}

pub(crate) fn cmd_resizefs(a: &Args) -> u8 {
    if a.online {
        // 在线路径：positional[0] = 挂载点（非 :N），positional[1] = 可选绝对字节数
        if a.part.is_some() {
            bail_fail(Fail::refused("--online takes a mountpoint, not <target>:N"));
        }
        // 目标尺寸的两种给法（BYTES 与 --size 同一单位语法，只接受绝对值）二选一
        let size = match (a.pos.get(1), a.size) {
            (Some(_), Some(_)) => bail_fail(Fail::refused("give the target size either as BYTES or --size, not both")),
            (Some(s), None) => Some(match parse_size_delta(s) {
                Some((bytes, 0, false)) => bytes,
                _ => bail_fail(Fail::refused(format!("size {s:?} must be an absolute size (use 10G | 500M | plain bytes)"))),
            }),
            (None, given) => given,
        };
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
    } else {
        // 离线形式"把 FS 扩进分区"，没有尺寸参数可言
        if a.size.is_some() {
            bail_fail(Fail::refused("--size only applies to the online form — offline resizes the filesystem into its partition"));
        }
        let Some(part) = a.part else { crate::args::usage() };
        let src = open_target_owned(a).unwrap_or_else(|f| bail_fail(f));
        let (start, len) = entry_byte_range(&src, part).unwrap_or_else(|f| bail_fail(f));
        // 与 mkfs 同判据：扩 FS 是外部写入，未收尾的恢复现场必须先收拾
        refuse_if_pending_recovery(&src, "resizefs").unwrap_or_else(|f| bail_fail(f));
        let fstype = match fsid::identify(&src, start, len) {
            Ok(t) => t,
            Err(e) => bail_fail(Fail::infra(format!("identify failed: {e}"))),
        };
        match fsops::resize_fs(&src, part, fstype) {
            Ok(()) => {
                println!("resized (verify with: diskedit info {})", a.target);
                EXIT_OK
            }
            Err(e) => bail_fail(Fail::from(e)),
        }
    }
}

pub(crate) fn cmd_check(a: &Args) -> u8 {
    let Some(part) = a.part else { crate::args::usage() };
    let src = open_target_owned(a).unwrap_or_else(|f| bail_fail(f));
    let (start, len) = entry_byte_range(&src, part).unwrap_or_else(|f| bail_fail(f));
    // e2fsck -fp / ntfsfix -d 会把修复写进 FS（不经 journal）：同 mkfs 的理由
    refuse_if_pending_recovery(&src, "check").unwrap_or_else(|f| bail_fail(f));
    let fstype = fsid::identify(&src, start, len).unwrap_or_else(|e| bail_fail(Fail::infra(format!("identify failed: {e}"))));
    match fsops::check_fs(&src, part, fstype) {
        Ok(()) => {
            println!("check done on partition #{part} ({fstype})");
            EXIT_OK
        }
        Err(e) => bail_fail(Fail::from(e).context("check failed")),
    }
}

/// set 的一键入口：统一 name/label/uuid/flag 四类属性
pub(crate) fn cmd_set(a: &Args) -> u8 {
    let Some(part) = a.part else { crate::args::usage() };
    let key = a.pos.get(1).map(|s| s.as_str()).unwrap_or_else(|| crate::args::usage());
    let value = a.pos.get(2).cloned().unwrap_or_default();
    let state = a.pos.get(3).cloned().unwrap_or_default();
    let mut src = open_target_for_write(a).unwrap_or_else(|f| bail_fail(f));
    if key == "name" {
        if value.is_empty() { crate::args::usage(); }
        return match crate::table::rename_entry(&mut src, part, &value) {
            Ok(()) => table_write_done(&src, &format!("renamed partition #{part} to {value:?}")),
            Err(f) => bail_fail(f),
        };
    }
    if key == "flag" {
        if value.is_empty() { crate::args::usage(); }
        let on = match state.as_str() { "on" => true, "off" => false, _ => crate::args::usage() };
        let r = match crate::table::table_label(&src) {
            Ok("gpt") => crate::table::set_gpt_flag(&mut src, part, &value, on),
            Ok("msdos") if value == "boot" => crate::table::set_mdos_boot(&mut src, part, on),
            Ok("msdos") if value == "hidden" => crate::table::set_mdos_hidden(&mut src, part, on),
            Ok("msdos") => bail_fail(Fail::refused("msdos flags: only `boot` and `hidden` are supported".to_string())),
            Ok(other) => bail_fail(Fail::refused(format!("cannot set flag on {other} label"))),
            Err(e) => bail_fail(Fail::infra(format!("label probe failed: {e}"))),
        };
        return match r {
            Ok(()) => table_write_done(&src, &format!("flag {value}={on} on partition #{part}")),
            Err(f) => bail_fail(f),
        };
    }
    // label/uuid 需要 FS 识别
    let (start, len) = entry_byte_range(&src, part).unwrap_or_else(|f| bail_fail(f));
    let fstype = fsid::identify(&src, start, len).unwrap_or_else(|e| bail_fail(Fail::infra(format!("identify failed: {e}"))));
    let r = match key {
        "label" if !value.is_empty() => fsops::set_label(&src, part, fstype, &value),
        "uuid" => {
            let req = match (a.random, value.is_empty()) {
                (true, false) => bail_fail(Fail::refused("`uuid` takes either a value or --random, not both".to_string())),
                (true, true) => fsops::UuidRequest::NewRandom,
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
            println!("set {key} on partition #{part} ({fstype})");
            EXIT_OK
        }
        Err(e) => bail_fail(Fail::from(e).context(&format!("set {key} failed"))),
    }
}

#[cfg(test)]
mod tests {
    use super::check_uuid_request;
    use crate::fsops::{UuidRequest, UuidSupport};

    /// UUID 请求与目标能力的比对：能设值的类型只接受显式值，ntfs 只接受"生成新值"，
    /// 其余不接线。ntfs 收到具体值必须**拒**——静默丢弃用户给的值正是要防的漂移
    #[test]
    fn uuid_request_capability_matrix() {
        for f in ["ext4", "xfs", "btrfs"] {
            assert!(check_uuid_request(f, &UuidRequest::Explicit("x".into())).is_ok(), "{f}");
            assert!(check_uuid_request(f, &UuidRequest::NewRandom).is_err(), "{f}");
        }
        assert!(check_uuid_request("ntfs", &UuidRequest::NewRandom).is_ok());
        let e = check_uuid_request("ntfs", &UuidRequest::Explicit("x".into())).unwrap_err();
        assert!(e.contains("--random"), "{e}");
        assert!(check_uuid_request("vfat", &UuidRequest::Explicit("x".into())).is_err());
        // 判据同源：gate 读的是 fsops 的能力表，不是命令层另抄的一份
        assert!(matches!(crate::fsops::uuid_support("ntfs"), UuidSupport::RandomOnly));
        assert!(matches!(crate::fsops::uuid_support("ext4"), UuidSupport::Yes));
        assert!(matches!(crate::fsops::uuid_support("unknown"), UuidSupport::No(_)));
    }
}