//! info：只读布局报告（分区表 + FS 识别 + 容器格式提示），JSON 输出。

use crate::support::*;
use crate::args::Args;
use crate::dev::FileSource;
use crate::{fsid, table};

pub(crate) const HELP: &str = r#"diskedit info <TARGET> [--sector-size N]

  Show partition table, per-partition filesystem identification and LVM
  layout. Read-only. --sector-size N overrides the 512B default (raw images)."#;

pub(crate) fn cmd_info(a: &Args) -> u8 {
    let src = match open_target_ro(a) {
        Ok(s) => s,
        Err((_, msg)) => {
            eprintln!("{msg}");
            return EXIT_INFRA;
        }
    };
    let mut out = String::from("{\"label\":");
    let mut stale_notes: Vec<String> = Vec::new();
    let gpt = match table::load_gpt(&src) {
        Ok(g) => g,
        // 解析层的几何自洽性失败必须显式报错，不能静默降级成 "none"。
        // 结构化变体在此分类输出（细节仍由 GptError 的 Display 单一渲染）
        Err(e) => {
            let kind = match &e {
                table::GptError::InvalidEntry { .. } | table::GptError::BeyondUsable { .. } => "invalid GPT entries",
                table::GptError::BeyondContainer { .. } => "GPT geometry beyond container",
                // 与 InvalidHeader 分开：这两个是字节层面的损伤（CRC 不符），
                // 而 InvalidHeader 是头部自述结构的语义非法（MyLBA、usable 上下界等）
                table::GptError::HeaderCorrupt { .. } => "GPT header damaged (both copies unusable)",
                // 走到这里说明两份副本都没给出可用的表——单份损伤会在 load_gpt 里被备份救回
                table::GptError::EntryArrayCorrupt { .. } => "GPT entry array damaged (both copies unusable)",
                table::GptError::InvalidHeader(_) => "invalid GPT header",
                table::GptError::Io(_) => "I/O error",
            };
            eprintln!("parse failed ({kind}): {e}");
            return EXIT_INFRA;
        }
    };
    if let Some(g) = gpt {
        // 结构可识别但需修复的状态：只报告、不修改（修复由写入路径执行）
        match g.state {
            table::GptState::NeedsRepair { cause: table::HeaderIssue::BackupLbaStale { expected, actual } } => {
                stale_notes.push(format!(
                    "note: backup GPT header is stale — found at LBA {actual}, expected at device end LBA {expected}; \
                     any write command (or sgdisk -e) relocates it"
                ))
            }
            table::GptState::NeedsRepair { cause: table::HeaderIssue::PrimaryUnreadable } => stale_notes.push(
                "note: the primary GPT copy is unusable (header or entry array) — this table was recovered from the \
                 backup copy at the device end; any write command rewrites both copies"
                    .to_string(),
            ),
            table::GptState::Valid => {}
        }
        match g.pmbr {
            table::PmbrSize::NeedsRepair { cause } => stale_notes.push(match cause {
                table::PmbrIssue::Stale => "note: protective MBR SizeInLBA is stale (smaller than this container) — any write command rewrites it".to_string(),
                // 非规范但可修复：UEFI 2.10 §5.2.3 的 SizeInLBA 以逻辑块计，512 字节口径是别的工具的历史写法
                table::PmbrIssue::Compat512 => "note: protective MBR SizeInLBA uses the 512-byte-sector convention instead of this device's logical-block value (UEFI 2.10 §5.2.3) — any write command normalizes it".to_string(),
            }),
            table::PmbrSize::Inconsistent => stale_notes.push(
                "note: protective MBR SizeInLBA exceeds this container — refusing auto-repair (use sgdisk/parted)".to_string(),
            ),
            table::PmbrSize::Normal => {}
        }
        out.push_str("\"gpt\",\"sector_size\":");
        out.push_str(&g.ss.to_string());
        out.push_str(",\"size_bytes\":");
        out.push_str(&src.size.to_string());
        out.push_str(",\"disk_guid\":\"");
        out.push_str(&hex_guid(&g.header.disk_guid));
        out.push_str("\",\"partitions\":[");
        let mut first = true;
        for (i, e) in g.entries.iter().enumerate() {
            if e.ending_lba == 0 && e.starting_lba == 0 {
                continue;
            }
            if !first {
                out.push(',');
            }
            first = false;
            let fs = fsid::identify(&src, e.starting_lba * g.ss, (e.ending_lba - e.starting_lba + 1) * g.ss).unwrap_or("error");
            out.push_str(&format!(
                "{{\"num\":{},\"first_lba\":{},\"last_lba\":{},\"size_bytes\":{},\"type\":\"{}\",\"fs\":\"{}\",\"name\":\"{}\"}}",
                i + 1,
                e.starting_lba,
                e.ending_lba,
                (e.ending_lba - e.starting_lba + 1) * g.ss,
                hex_guid(&e.partition_type_guid),
                fs,
                json_escape(e.partition_name.as_str())
            ));
        }
        out.push_str("]}");
    } else {
        // 只读探测的 io 失败不得降级为 "none"：那会把"读不出来"报成"没有表"，
        // 与 GPT 分支的判据不一致（缺表 = 现状不匹配，读不出来 = 盘内容/环境故障）
        let mbr = match table::parse_mbr(&src) {
            Ok(m) => m,
            Err(e) => {
                eprintln!("parse failed: {e}");
                return EXIT_INFRA;
            }
        };
        match mbr {
            // 仅签名、零记录也判 mbr（`new --table msdos` 的合法初始态）
            Some(mbr) => {
                out.push_str("\"mbr\",\"sector_size\":");
                out.push_str(&src.sector_size.to_string());
                out.push_str(",\"size_bytes\":");
                out.push_str(&src.size.to_string());
                out.push_str(",\"partitions\":[");
                let parts: Vec<String> = mbr.iter().map(|p| {
                    let fs = if p.is_container { "container".to_string() }
                        else { fsid::identify(&src, p.start_lba as u64 * src.sector_size, p.size_lba as u64 * src.sector_size).unwrap_or("error").to_string() };
                    format!(
                        "{{\"num\":{},\"type\":\"0x{:02X}\",\"first_lba\":{},\"last_lba\":{},\"size_bytes\":{},\"fs\":\"{}\"}}",
                        p.num, p.os_type, p.start_lba, p.start_lba + p.size_lba.saturating_sub(1),
                        p.size_lba as u64 * src.sector_size, fs
                    )
                }).collect();
                out.push_str(&parts.join(","));
                out.push_str("]}");
            }
            None => out.push_str(&format!("\"none\",\"sector_size\":{},\"size_bytes\":{}}}", src.sector_size, src.size)),
        }
    }
    println!("{out}");
    for n in &stale_notes {
        eprintln!("{n}");
    }
    // 非 raw 容器格式识别（qcow2/VMDK/VDI/VHD/VHDX 魔数，qemu docs block-drivers）：字节直译
    // 假设不成立（guest LBA 经容器内分配表间接映射），本工具无法处理，改用 qemu-nbd 映射为
    // 块设备（modprobe nbd max_part / qemu-nbd -c/-d，见 qemu 官方工具文档）
    if let Some(fmt) = container_format(&src) {
        eprintln!(
            "note: {} looks like a {fmt} container image (not raw); map it to a block device first:\n  \
             modprobe nbd max_part=8\n  \
             qemu-nbd -c /dev/nbd0 {}\n  \
             diskedit info /dev/nbd0   # then operate on /dev/nbd0 as usual\n  \
             qemu-nbd -d /dev/nbd0     # when done; writes go back to {} live",
            a.target, a.target, a.target
        );
    }
    EXIT_OK
}

/// 容器格式魔数探测（仅提示，不做解析）：qcow2 @0 "QFI\xfb"；VDI @0 "<<< Oracle VM VirtualBox
/// Disk Image >>>"（VirtualBox 官方 VDI 格式）；VMDK sparse @0 "KDMV"（'VMDK' LE）或描述符
/// 文本；VHDX @0 "vhdxfile"。
/// VHD：@0 "conectix" 是 dynamic/differencing 的 footer 副本，fixed 的 footer 只在文件末尾
/// → 先查 @0，未命中再仿 qemu block/vpc.c 的 fallback 读 EOF−512（footer 全大端、
/// cookie@0 = "conectix"、type@60 = VHD_FIXED(2)）
fn container_format(src: &FileSource) -> Option<&'static str> {
    let mut buf = [0u8; 64];
    src.read_at(0, &mut buf).ok()?;
    let b = &buf;
    let starts = |m: &[u8]| b.len() >= m.len() && &b[..m.len()] == m;
    if starts(b"QFI\xfb".as_slice()) {
        Some("qcow2")
    } else if starts(b"conectix".as_slice()) {
        Some("VHD")
    } else if starts(b"<<< Oracle VM".as_slice()) {
        Some("VDI")
    } else if starts(b"KDMV".as_slice()) || starts(b"# Disk DescriptorFile".as_slice()) {
        Some("VMDK")
    } else if starts(b"vhdxfile".as_slice()) {
        Some("VHDX")
    } else if vhd_fixed_footer(src) {
        Some("VHD")
    } else {
        None
    }
}

/// fixed VHD 探测：footer 在文件末尾 512 字节（qemu block/vpc.c:289-320 读 offset −
/// sizeof(VHDFooter) = 512），要求 creator@0 = "conectix"、type@60（大端 u32）= VHD_FIXED(2)
/// （vpc.c:314-315）。本处只做格式识别，不校验 footer checksum（qemu 在随后一步校验）
fn vhd_fixed_footer(src: &FileSource) -> bool {
    if src.size < 512 {
        return false;
    }
    let mut f = [0u8; 512];
    if src.read_at(src.size - 512, &mut f).is_err() {
        return false;
    }
    &f[0..8] == b"conectix" && u32::from_be_bytes([f[60], f[61], f[62], f[63]]) == 2
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::support::src_from;

    /// VHD footer 512 字节且字段全大端（qemu block/vpc.c vhd_footer：creator@0 8 字节、
    /// type@60 be32；VHD_FIXED = 2 / VHD_DYNAMIC = 3 / VHD_DIFFERENCING = 4）
    fn vhd_footer(disk_type: u32, cookie: &[u8; 8]) -> [u8; 512] {
        let mut f = [0u8; 512];
        f[0..8].copy_from_slice(cookie);
        f[60..64].copy_from_slice(&disk_type.to_be_bytes());
        f
    }

    /// fixed VHD：footer 只在 EOF、@0 无 conectix，走 vhd_fixed_footer 的 EOF 探测分支
    #[test]
    fn fixed_vhd_detected_by_eof_footer() {
        let mut data = vec![0u8; 8192];
        data[8192 - 512..].copy_from_slice(&vhd_footer(2, b"conectix"));
        let src = src_from("vhd_ok", &data);
        assert!(vhd_fixed_footer(&src));
        assert_eq!(container_format(&src), Some("VHD"));

        // EOF 处 cookie 损坏
        let mut bad = vec![0u8; 8192];
        bad[8192 - 512..].copy_from_slice(&vhd_footer(2, b"conectiX"));
        assert!(!vhd_fixed_footer(&src_from("vhd_cookie", &bad)));
        assert_eq!(container_format(&src_from("vhd_cookie2", &bad)), None);

        // DiskType 不是 VHD_FIXED（dynamic=3）
        let mut dyn_type = vec![0u8; 8192];
        dyn_type[8192 - 512..].copy_from_slice(&vhd_footer(3, b"conectix"));
        assert!(!vhd_fixed_footer(&src_from("vhd_type", &dyn_type)));

        // 截断：文件短于 footer 起点 → 读不到完整 512 字节，不得误判
        let truncated = &data[..data.len() - 1];
        assert!(!vhd_fixed_footer(&src_from("vhd_trunc", truncated)));

        // 文件不足 512 字节
        assert!(!vhd_fixed_footer(&src_from("vhd_tiny", &[0u8; 100])));
    }

    /// dynamic VHD 走 @0 分支（与 fixed 的 EOF 分支分工不同，两者不得互相干扰）
    #[test]
    fn dynamic_vhd_matched_at_offset_zero() {
        let mut data = vec![0u8; 8192];
        data[0..512].copy_from_slice(&vhd_footer(3, b"conectix"));
        let src = src_from("vhd_dyn", &data);
        assert_eq!(container_format(&src), Some("VHD"));
        // @0 已是 VHD 时不会走到 EOF 分支：EOF 放一个 fixed footer 也不改变结论
        let mut both = data.clone();
        both[8192 - 512..].copy_from_slice(&vhd_footer(2, b"conectix"));
        assert_eq!(container_format(&src_from("vhd_both", &both)), Some("VHD"));
    }
}