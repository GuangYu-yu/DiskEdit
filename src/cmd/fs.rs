//! 文件系统类命令：mkfs / resizefs / check / set（FS 属性部分）。

use crate::support::*;
use crate::args::{parse_size_delta, Args};
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

pub(crate) const HELP_SET: &str = r#"diskedit set <TARGET>:N name S | label S | uuid U | flag F on|off

  Set a partition property.
    name          GPT partition name
    label / uuid  filesystem label / UUID (FS-aware)
    flag          GPT: esp|boot|hidden|required ; MBR: boot|hidden"#;

pub(crate) fn cmd_mkfs(a: &Args) -> u8 {
    let (Some(part), Some(fstype)) = (a.part, a.fstype.clone()) else { crate::args::usage() };
    if !a.yes {
        eprintln!("refused: mkfs destroys all data on partition {part}; pass --yes to confirm");
        EXIT_REFUSED
    } else {
        let src = open_target(a).unwrap_or_else(|(c, m)| bail(c, m));
        if let Err(f) = entry_byte_range(&src, part) {
            bail_fail(f);
        }
        match fsops::mkfs(&src, part, &fstype) {
            Ok(()) => EXIT_OK,
            Err(e) => { eprintln!("mkfs failed: {e}"); EXIT_INFRA }
        }
    }
}

pub(crate) fn cmd_resizefs(a: &Args) -> u8 {
    if a.online {
        // 在线路径：positional[0] = 挂载点（非 :N），positional[1] = 可选绝对字节数
        if a.part.is_some() {
            bail(EXIT_REFUSED, "--online takes a mountpoint, not <target>:N".to_string());
        }
        // 目标尺寸的两种给法（BYTES 与 --size 同一单位语法，只接受绝对值）二选一
        let size = match (a.fstype.as_ref(), a.size) {
            (Some(_), Some(_)) => bail(EXIT_REFUSED, "refused: give the target size either as BYTES or --size, not both".to_string()),
            (Some(s), None) => Some(match parse_size_delta(s) {
                Some((bytes, 0, false)) => bytes,
                _ => bail(EXIT_REFUSED, format!("size {s:?} must be an absolute size (use 10G | 500M | plain bytes)")),
            }),
            (None, given) => given,
        };
        #[cfg(target_os = "linux")]
        {
            let o = crate::online::resize_online(std::path::Path::new(&a.target), size);
            if o.exit_code() == EXIT_OK {
                println!("resized online (verify with: diskedit info {})", a.target);
            } else {
                o.report();
            }
            o.exit_code()
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = size;
            bail(EXIT_INFRA, "online resize requires Linux".to_string());
        }
    } else {
        // 离线形式"把 FS 扩进分区"，没有尺寸参数可言
        if a.size.is_some() {
            bail(EXIT_REFUSED, "refused: --size only applies to the online form — offline resizes the filesystem into its partition".to_string());
        }
        let Some(part) = a.part else { crate::args::usage() };
        let src = open_target(a).unwrap_or_else(|(c, m)| bail(c, m));
        let (start, len) = entry_byte_range(&src, part).unwrap_or_else(|f| bail_fail(f));
        let fstype = match fsid::identify(&src, start, len) {
            Ok(t) => t,
            Err(e) => bail(EXIT_INFRA, format!("identify failed: {e}")),
        };
        match fsops::resize_fs(&src, part, fstype) {
            Ok(()) => {
                println!("resized (verify with: diskedit info {})", a.target);
                EXIT_OK
            }
            Err(e) => { eprintln!("resizefs failed: {e}"); EXIT_INFRA }
        }
    }
}

pub(crate) fn cmd_check(a: &Args) -> u8 {
    let Some(part) = a.part else { crate::args::usage() };
    let src = open_target(a).unwrap_or_else(|(c, m)| bail(c, m));
    let (start, len) = entry_byte_range(&src, part).unwrap_or_else(|f| bail_fail(f));
    let fstype = fsid::identify(&src, start, len).unwrap_or_else(|e| bail(EXIT_INFRA, format!("identify failed: {e}")));
    match fsops::check_fs(&src, part, fstype) {
        Ok(()) => { println!("check done on partition #{part} ({fstype})"); EXIT_OK }
        Err(e) => { eprintln!("check failed: {e}"); EXIT_INFRA }
    }
}

/// set 的一键入口：统一 name/label/uuid/flag 四类属性
pub(crate) fn cmd_set(a: &Args) -> u8 {
    let Some(part) = a.part else { crate::args::usage() };
    let key = a.pos.get(1).map(|s| s.as_str()).unwrap_or_else(|| crate::args::usage());
    let value = a.pos.get(2).cloned().unwrap_or_default();
    let state = a.pos.get(3).cloned().unwrap_or_default();
    let mut src = open_target_for_write(a).unwrap_or_else(|(c, m)| bail(c, m));
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
            Ok("msdos") => bail(EXIT_REFUSED, "msdos flags: only `boot` and `hidden` are supported".to_string()),
            Ok(other) => bail(EXIT_REFUSED, format!("cannot set flag on {other} label")),
            Err(e) => bail(EXIT_INFRA, format!("label probe failed: {e}")),
        };
        return match r {
            Ok(()) => table_write_done(&src, &format!("flag {value}={on} on partition #{part}")),
            Err(f) => bail_fail(f),
        };
    }
    // label/uuid 需要 FS 识别
    let (start, len) = entry_byte_range(&src, part).unwrap_or_else(|f| bail_fail(f));
    let fstype = fsid::identify(&src, start, len).unwrap_or_else(|e| bail(EXIT_INFRA, format!("identify failed: {e}")));
    let r = match key {
        "label" if !value.is_empty() => fsops::set_label(&src, part, fstype, &value),
        "uuid" if !value.is_empty() => fsops::set_uuid(&src, part, fstype, &value),
        "label" | "uuid" => crate::args::usage(),
        _ => crate::args::usage(),
    };
    match r {
        Ok(()) => { println!("set {key} on partition #{part} ({fstype})"); EXIT_OK }
        Err(e) => { eprintln!("set {key} failed: {e}"); EXIT_INFRA }
    }
}