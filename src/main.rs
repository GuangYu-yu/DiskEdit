//! diskedit — 磁盘编辑工具：镜像文件与块设备统一为按偏移读写的字节存储。
//! 命令面与退出码契约：0=完成（经读回复核）/10=拒绝执行（未写盘）/20=部分完成/30=基础设施失败。

mod dev;
mod fsid;
mod fsops;
mod list;
#[cfg(target_os = "linux")]
mod lvm;
#[cfg(target_os = "linux")]
mod online;
mod movepart;
mod table;

#[cfg(target_os = "linux")]
use std::os::unix::fs::FileTypeExt;
use dev::{FileSource, Journal};
use std::io::Write;
use std::process::ExitCode;

pub(crate) const EXIT_OK: u8 = 0;
pub(crate) const EXIT_REFUSED: u8 = 10;
pub(crate) const EXIT_PARTIAL: u8 = 20;
pub(crate) const EXIT_INFRA: u8 = 30;

fn usage() -> ! {
    eprintln!(
        r#"diskedit — disk / image editor

  info <TARGET>                                  show partition table / FS / LVM layout
  ls <TARGET>:N [PATH] | cat <TARGET>:N <PATH>   browse filesystem
  resize <TARGET>:N <SIZE> [OPTS]                resize partition + FS (auto online/offline)
  move <TARGET>:N --start <LBA|end>              move partition
  copy <TARGET>:N --start <LBA|end> [--name S]   copy partition
  create <TARGET> [--size B] [--name S] [--fs F] create partition in free space
  delete <TARGET>:N --yes                        delete partition entry
  set <TARGET>:N name S | label S | uuid U | flag F on|off
  check <TARGET>:N                               check filesystem
  mkfs <TARGET>:N <FS> --yes                     create filesystem
  resizefs <TARGET>:N | <MOUNTPOINT> [BYTES] --online
                                                 resize filesystem
  undo <TARGET> --yes                            undo this tool's writes (journal)

  new / add / del / resize-part / plan / apply   low-level

  diskedit help <CMD>                            details for one command

target: image path or block device; :N = partition number (1-based)
exit codes: 0=done 10=refused 20=partial 30=infrastructure failure"#
    );
    std::process::exit(EXIT_REFUSED as i32);
}

/// 单命令详助（diskedit help <CMD> / <CMD> --help）。未知主题 → 顶层 usage
fn help_cmd(name: &str) -> ! {
    let text: &str = match name {
        "info" => r#"diskedit info <TARGET> [--sector-size N]

  Show partition table, per-partition filesystem identification and LVM
  layout. Read-only. --sector-size N overrides the 512B default (raw images)."#,
        "ls" | "cat" => r#"diskedit ls <TARGET>:N [PATH]
diskedit cat <TARGET>:N <PATH>

  Browse filesystem contents (streaming backend, works on large partitions).
  ls lists a directory; cat writes a file to stdout."#,
        "resize" => r#"diskedit resize <TARGET>:N <SIZE> [OPTIONS]

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
    are checkpointed and resumed by re-running the same command.

  Notes: PV shrink is refused (use the lvreduce/pvresize chain)."#,
        "move" => r#"diskedit move <TARGET>:N --start <LBA|end>

  Move a partition, data follows (chunked copy, resumable via checkpoint).

  LOCATION:
    <LBA>     new start LBA (aligned per --align, default 1MiB)
    end       tail-pack to the last possible position"#,
        "copy" => r#"diskedit copy <TARGET>:N --start <LBA|end> [--name S]

  Byte-wise copy a partition to a new location; the source is untouched.
  --start end packs the copy against the end of the usable range."#,
        "create" => r#"diskedit create <TARGET> [--size BYTES] [--name S] [--fs F]

  Create a partition in free space. With --size, the first aligned gap that
  fits; without, the largest aligned gap. --fs formats after creation."#,
        "delete" | "del" => r#"diskedit delete <TARGET>:N --yes
diskedit del <TARGET>:N --yes

  Delete a partition entry (the data area is not wiped)."#,
        "set" => r#"diskedit set <TARGET>:N name S | label S | uuid U | flag F on|off

  Set a partition property.
    name          GPT partition name
    label / uuid  filesystem label / UUID (FS-aware)
    flag          GPT: esp|boot|hidden|required ; MBR: boot|hidden"#,
        "check" => r#"diskedit check <TARGET>:N

  Check filesystem consistency (tool-specific; ext runs e2fsck -fp and
  ntfs runs ntfsfix -d, both may write repairs to the filesystem)."#,
        "mkfs" => r#"diskedit mkfs <TARGET>:N <FS> --yes

  Create a filesystem. Destroys all data on the partition; stale signatures
  are erased first. Supported: ext2/3/4, xfs, btrfs, f2fs, vfat, exfat,
  ntfs, swap."#,
        "resizefs" => r#"diskedit resizefs <TARGET>:N
diskedit resizefs <MOUNTPOINT> [BYTES] --online

  Resize a filesystem. Offline form grows the FS into its partition.
  Online form operates on a mounted partition: grow only (btrfs also
  shrinks); BYTES is the absolute target in bytes."#,
        "undo" => r#"diskedit undo <TARGET> --yes

  Replay the journal to undo this tool's direct writes (partition table and
  relocated data). Writes made by external FS tools are not undone."#,
        "new" => r#"diskedit new <TARGET> [--table gpt|msdos] --yes

  Create a fresh partition table; overwrites any existing one. Default gpt."#,
        "add" => r#"diskedit add <TARGET> --start LBA --end LBA [--name S] [--type T]

  Add a partition over [start, end] LBA. GPT --type takes a standard GUID
  text (default: Linux filesystem data); MBR --type takes 0xXX (default 0x83)."#,
        "resize-part" => r#"diskedit resize-part <TARGET>:N --start LBA (--end LBA | --grow-to-end)

  Low-level grow/shrink/move in one: repartition [start, end] with data
  relocation. --grow-to-end pins end at last_usable_lba (fill semantics);
  --align mib|cyl|none and --chunk-size MiB control placement and copy
  granularity."#,
        "plan" | "apply" => r#"diskedit plan <TARGET> --grow N
diskedit apply <TARGET> --grow N [--chunk-size MiB]

  Grow partition N into all following free space, relocating intervening
  partitions tail-packed (manual multi-step form of `resize grow`).
  plan prints the operations without touching the disk; apply executes
  them and resumes from its checkpoint if re-run."#,
        _ => usage(),
    };
    println!("{text}");
    std::process::exit(EXIT_OK as i32);
}

fn bail(code: u8, msg: String) -> ! {
    eprintln!("{msg}");
    std::process::exit(code as i32);
}

struct Args {
    target: String,
    part: Option<u32>,
    fstype: Option<String>,
    path: Option<String>,
    state: Option<String>,
    grow: Option<u32>,
    start: Option<u64>,
    end: Option<u64>,
    size: Option<u64>,
    fs: Option<String>,
    name: Option<String>,
    type_guid: Option<String>,
    table: Option<String>,
    yes: bool,
    online: bool,
    sector_size: Option<u64>,
    align: String,
    chunk_mib: u64,
    grow_to_end: bool,
    allow_move: bool,
    grow_lv: bool,
    lv: Option<String>,
    start_end: bool,
    pos: Vec<String>,
}

fn parse_args() -> (String, Args) {
    let mut it = std::env::args().skip(1);
    let cmd = it.next().unwrap_or_else(|| usage());
    let mut a = Args {
        target: String::new(), part: None, fstype: None, path: None, state: None, grow: None,
        start: None, end: None, size: None, fs: None, name: None, type_guid: None, table: None,
        yes: false, online: false, sector_size: None,
        align: "mib".to_string(),
        chunk_mib: 4,
        grow_to_end: false,
        allow_move: false,
        grow_lv: false,
        lv: None,
        start_end: false,
        pos: Vec::new(),
    };
    let mut positional = Vec::new();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--yes" => a.yes = true,
            "--online" => a.online = true,
            "--sector-size" => {
                let v = it.next().unwrap_or_else(|| std::process::exit(EXIT_REFUSED as i32));
                a.sector_size = Some(v.parse().unwrap_or_else(|_| std::process::exit(EXIT_REFUSED as i32)));
            }
            "--grow" => {
                let v = it.next().unwrap_or_else(|| std::process::exit(EXIT_REFUSED as i32));
                a.grow = Some(v.parse().unwrap_or_else(|_| std::process::exit(EXIT_REFUSED as i32)));
            }
            "--size" => {
                let v = it.next().unwrap_or_else(|| std::process::exit(EXIT_REFUSED as i32));
                a.size = Some(v.parse().unwrap_or_else(|_| std::process::exit(EXIT_REFUSED as i32)));
            }
            "--fs" => a.fs = Some(it.next().unwrap_or_else(|| std::process::exit(EXIT_REFUSED as i32))),
            "--start" => {
                let v = it.next().unwrap_or_else(|| std::process::exit(EXIT_REFUSED as i32));
                if v.eq_ignore_ascii_case("end") {
                    a.start_end = true; // 尾部打包：挪到 last_usable 内最后位置
                } else {
                    a.start = Some(v.parse().unwrap_or_else(|_| std::process::exit(EXIT_REFUSED as i32)));
                }
            }
            "--end" => {
                let v = it.next().unwrap_or_else(|| std::process::exit(EXIT_REFUSED as i32));
                a.end = Some(v.parse().unwrap_or_else(|_| std::process::exit(EXIT_REFUSED as i32)));
            }
            "--align" => a.align = it.next().unwrap_or_else(|| std::process::exit(EXIT_REFUSED as i32)),
            "--chunk-size" => {
                let v = it.next().unwrap_or_else(|| std::process::exit(EXIT_REFUSED as i32));
                a.chunk_mib = v.parse().unwrap_or_else(|_| std::process::exit(EXIT_REFUSED as i32));
            }
            "--grow-to-end" => a.grow_to_end = true,
            "--allow-move" => a.allow_move = true,
            "--grow-lv" => a.grow_lv = true,
            "--lv" => a.lv = Some(it.next().unwrap_or_else(|| std::process::exit(EXIT_REFUSED as i32))),
            "--name" => a.name = Some(it.next().unwrap_or_else(|| std::process::exit(EXIT_REFUSED as i32))),
            "--type" => a.type_guid = Some(it.next().unwrap_or_else(|| std::process::exit(EXIT_REFUSED as i32))),
            "--table" => a.table = Some(it.next().unwrap_or_else(|| std::process::exit(EXIT_REFUSED as i32))),
            // <CMD> --help：positional 为空时以当前命令为主题
            "--help" | "-h" => {
                let topic = positional.first().cloned().unwrap_or(cmd);
                help_cmd(&topic);
            }
            _ => positional.push(arg),
        }
    }
    // 无 target（含裸调用/未知命令缺参）时打印帮助而非静默退出
    let target = positional.first().cloned().unwrap_or_else(|| usage());
    let (target, part) = dev::parse_target(&target);
    a.target = target;
    a.part = part;
    a.pos = positional.clone();
    a.fstype = positional.get(1).cloned();
    a.path = positional.get(1).cloned();
    a.state = positional.get(2).cloned();
    (cmd, a)
}

/// 日志：镜像 = `<名>.diskedit.log`；块设备 = /var/lib/diskedit/<GUID>.diskedit.log，
/// 无 GPT（MBR/裸盘）时用 <devname>.diskedit.log，与 journal 命名策略对称
struct Logger {
    file: Option<std::fs::File>,
}

impl Logger {
    fn open(src: &FileSource) -> Self {
        let path = if src.is_block {
            let dir = std::path::Path::new("/var/lib/diskedit");
            std::fs::create_dir_all(dir).ok().and_then(|()| {
                table::load_gpt(src).ok().flatten().map(|g| {
                    let hex: String = g.header.disk_guid.iter().map(|b| format!("{b:02X}")).collect();
                    dir.join(format!("{hex}.diskedit.log"))
                }).or_else(|| {
                    // 无 GPT（MBR/裸盘）：devname 是无表场景唯一稳定标识
                    let name = src.path.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "dev".into());
                    Some(dir.join(format!("{name}.diskedit.log")))
                })
            })
        } else {
            let mut p = src.path.clone().into_os_string();
            p.push(".diskedit.log");
            Some(std::path::PathBuf::from(p))
        };
        let file = path.and_then(|p| std::fs::OpenOptions::new().create(true).append(true).open(p).ok());
        if file.is_none() {
            // 落盘失败不静默：用户需知日志只进 stdout
            eprintln!("warning: persistent log unavailable — output only goes to stdout");
        }
        Logger { file }
    }

    fn log(&mut self, msg: &str) {
        let ts = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        println!("{msg}");
        if let Some(f) = &mut self.file {
            let _ = writeln!(f, "[{ts}] {msg}");
        }
    }
}

/// chunk 大小 + 持久日志的成对构造（搬移/拷贝类命令共用）
fn chunk_logger(a: &Args, src: &FileSource) -> (u64, Logger) {
    let chunk = movepart::chunk_bytes(a.chunk_mib).unwrap_or_else(|e| bail(EXIT_REFUSED, format!("refused: {e}")));
    (chunk, Logger::open(src))
}

/// 磁盘字节序 16 字节 → 标准文本 GUID（前 3 字段小端重排；内核 efi.h EFI_GUID 宏的逆变换）
fn hex_guid(b: &[u8; 16]) -> String {
    let d1 = u32::from_le_bytes(b[0..4].try_into().unwrap());
    let d2 = u16::from_le_bytes(b[4..6].try_into().unwrap());
    let d3 = u16::from_le_bytes(b[6..8].try_into().unwrap());
    format!(
        "{d1:08X}-{d2:04X}-{d3:04X}-{:02X}{:02X}-{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}",
        b[8], b[9], b[10], b[11], b[12], b[13], b[14], b[15]
    )
}

/// JSON 字符串转义：引号、反斜杠、换行/回车/制表符及全部 <0x20 控制字符（RFC 8259）
fn json_escape(s: &str) -> String {
    s.chars().map(|c| match c {
        '"' => "\\\"".to_string(),
        '\\' => "\\\\".to_string(),
        '\n' => "\\n".to_string(),
        '\r' => "\\r".to_string(),
        '\t' => "\\t".to_string(),
        c if (c as u32) < 0x20 => format!("\\u{:04x}", c as u32),
        c => c.to_string(),
    }).collect()
}

/// 标准文本 GUID（8-4-4-4-12，连字符可省略，其余字符一律拒绝）→ 磁盘字节序
fn parse_guid(s: &str) -> Option<[u8; 16]> {
    let bare = s.len() == 32 && s.bytes().all(|b| b.is_ascii_hexdigit());
    let hyphened = s.len() == 36
        && s.match_indices('-').map(|(i, _)| i).collect::<Vec<_>>() == vec![8, 13, 18, 23]
        && s.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-');
    if !bare && !hyphened {
        return None;
    }
    let hex: String = s.chars().filter(|&c| c != '-').collect();
    let mut text = [0u8; 16];
    for i in 0..16 {
        text[i] = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).ok()?;
    }
    // 文本序 → 磁盘序：前 3 字段小端（字段 1=u32、字段 2/3=u16），字段 4 原样
    let mut out = [0u8; 16];
    out[0] = text[3]; out[1] = text[2]; out[2] = text[1]; out[3] = text[0];
    out[4] = text[5]; out[5] = text[4];
    out[6] = text[7]; out[7] = text[6];
    out[8..16].copy_from_slice(&text[8..16]);
    Some(out)
}

fn open_target(a: &Args) -> Result<FileSource, (u8, String)> {
    FileSource::open(std::path::Path::new(&a.target), a.sector_size)
        .map_err(|e| (EXIT_INFRA, format!("open failed: {e}")))
}

/// undo journal 路径：镜像 = `<名>.diskedit.journal`；块设备 = /var/lib/diskedit/<devname>.diskedit.journal。
/// 块设备用 devname 而非 disk_guid：`new` 前后均可用，代价是设备名漂移时需手动定位 journal
fn journal_file(src: &FileSource) -> std::path::PathBuf {
    if src.is_block {
        let dir = std::path::Path::new("/var/lib/diskedit");
        let _ = std::fs::create_dir_all(dir);
        let name = src.path.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "dev".into());
        dir.join(format!("{name}.diskedit.journal"))
    } else {
        let mut p = src.path.clone().into_os_string();
        p.push(".diskedit.journal");
        std::path::PathBuf::from(p)
    }
}

/// 破坏性命令的打开方式：附带 undo journal（镜像/块设备一致）
fn open_target_for_write(a: &Args) -> Result<FileSource, (u8, String)> {
    let mut src = open_target(a)?;
    let p = journal_file(&src);
    src.journal = Some(Journal::create(&p).map_err(|e| (EXIT_INFRA, format!("journal open failed: {e}")))?);
    Ok(src)
}

/// 表类写命令成功后通知内核重读分区表（BLKRRPART）。失败仅警告不失败：
/// 盘上表已写成功 ≠ 内核 partition view 已同步，后续不得依赖旧的内核几何，
/// 实际状态以 /sys/block/<dev>/<part>/size 为准
#[cfg(target_os = "linux")]
fn kernel_resync(src: &FileSource) {
    if !src.is_block {
        return;
    }
    use std::os::fd::AsRawFd;
    const BLKRRPART: u64 = 0x125F; // _IO(0x12, 95)（include/uapi/linux/fs.h）
    let r = unsafe { libc::ioctl(src.file.as_raw_fd() as libc::c_int, BLKRRPART as libc::Ioctl, 0u32) };
    if r < 0 {
        eprintln!(
            "warning: partition table is on disk but the kernel did not re-read it (device busy?) — \
             kernel partition view is stale; run `partprobe {}` before relying on it",
            src.path.display()
        );
    }
}

#[cfg(not(target_os = "linux"))]
fn kernel_resync(src: &FileSource) {
    let _ = src;
}

/// 在线路径前置守卫：活动 swap 拒绝（run swapoff 后重试）
#[cfg(target_os = "linux")]
fn refuse_swap_active(dn: &str, part: u32) {
    if online::swap_active(dn, part) {
        bail(EXIT_REFUSED, format!("refused: partition {part} is active swap — run swapoff first"));
    }
}

/// 对齐（选项名同 parted --align）：mib = 1MiB 边界；cyl = 255 头 × 63 扇区 = 16065 逻辑
/// 扇区/柱面（BIOS INT 13h 虚拟几何，即 fdisk 的 "cylinders of 16065 * 512"）。
/// start 上取整、end 下取整；对齐后区间为空即拒绝；none 跳过
fn align_range(a: &Args, start: u64, end: u64, ss: u64) -> (u64, u64) {
    let unit: u64 = match a.align.as_str() {
        "none" => return (start, end),
        "mib" => 1024 * 1024 / ss,
        "cyl" => 16065,
        other => bail(EXIT_REFUSED, format!("invalid --align {other:?} (mib|cyl|none)")),
    };
    let s = start.div_ceil(unit) * unit;
    let e1 = end.saturating_add(1) / unit * unit;
    if e1 == 0 || s > e1 - 1 {
        bail(EXIT_REFUSED, format!("refused: range {start}..{end} is empty after {} alignment", a.align));
    }
    if s != start || e1 - 1 != end {
        eprintln!("aligned to {}: {start}..{end} -> {s}..{}", a.align, e1 - 1);
    }
    (s, e1 - 1)
}

/// 起点上取整（copy 的 --start 只有起点语义，长度继承源分区）
fn align_start(a: &Args, start: u64, ss: u64) -> u64 {
    let unit: u64 = match a.align.as_str() {
        "none" => return start,
        "mib" => 1024 * 1024 / ss,
        "cyl" => 16065,
        other => bail(EXIT_REFUSED, format!("invalid --align {other:?} (mib|cyl|none)")),
    };
    let s = start.div_ceil(unit) * unit;
    if s != start {
        eprintln!("aligned to {}: {start} -> {s}", a.align);
    }
    s
}

/// 目标分区存在且非空（`:N` 命中核验，off-by-one 防线）
fn get_entry(src: &FileSource, part: u32) -> Result<(u64, u64), (u8, String)> {
    let g = table::load_gpt(src).map_err(|e| (EXIT_REFUSED, format!("parse failed: {e}")))?
        .ok_or((EXIT_REFUSED, "no GPT on target".to_string()))?;
    let e = g.entries.get((part - 1) as usize)
        .ok_or((EXIT_REFUSED, format!("partition {part} not found")))?;
    if e.ending_lba == 0 {
        return Err((EXIT_REFUSED, format!("partition {part} is empty")));
    }
    Ok((e.starting_lba, e.ending_lba - e.starting_lba + 1))
}

/// info 专用打开：块设备只读（RW+O_EXCL 在盘被 claim 时会被内核拒绝，分区被占用会连同
/// 整盘一起被 claim），镜像文件照常
fn open_target_ro(a: &Args) -> Result<FileSource, (u8, String)> {
    #[cfg(target_os = "linux")]
    if let Ok(meta) = std::fs::metadata(&a.target)
        && meta.file_type().is_block_device()
    {
        return FileSource::open_read_only(std::path::Path::new(&a.target))
            .map_err(|e| (EXIT_INFRA, format!("open failed: {e}")));
    }
    open_target(a)
}

fn cmd_info(a: &Args) -> i32 {
    let src = match open_target_ro(a) {
        Ok(s) => s,
        Err((_, msg)) => {
            eprintln!("{msg}");
            return EXIT_INFRA as i32;
        }
    };
    let mut out = String::from("{\"label\":");
    let mut stale_notes: Vec<String> = Vec::new();
    let gpt = match table::load_gpt(&src) {
        Ok(g) => g,
        // 解析层的几何自洽性失败必须显式报错，不能静默降级成 "none"
        Err(e) => {
            eprintln!("parse failed: {e}");
            return EXIT_INFRA as i32;
        }
    };
    if let Some(g) = gpt {
        // 结构可识别但需修复的状态：只报告、不修改（修复由写入路径执行）
        match g.state {
            table::GptState::Stale { backup_lba, file_last_lba } => stale_notes.push(format!(
                "note: backup GPT header is stale — found at LBA {backup_lba}, expected at device end LBA {file_last_lba}; \
                 any write command (or sgdisk -e) relocates it"
            )),
            table::GptState::Valid => {}
        }
        match g.pmbr {
            table::PmbrSize::Stale => stale_notes.push(
                "note: protective MBR SizeInLBA is stale (smaller than this container) — any write command rewrites it".to_string(),
            ),
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
            let fs = fsid::identify(&src, e.starting_lba, e.ending_lba - e.starting_lba + 1).unwrap_or("error");
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
    } else if let Ok(Some(mbr)) = table::parse_mbr(&src) {
        // 仅签名、零记录也判 mbr（`new --table msdos` 的合法初始态）
        out.push_str("\"mbr\",\"sector_size\":");
        out.push_str(&src.sector_size.to_string());
        out.push_str(",\"size_bytes\":");
        out.push_str(&src.size.to_string());
        out.push_str(",\"partitions\":[");
        let parts: Vec<String> = mbr.iter().map(|p| {
            let fs = if p.is_container { "container".to_string() }
                else { fsid::identify(&src, p.start_lba as u64, p.size_lba as u64).unwrap_or("error").to_string() };
            format!(
                "{{\"num\":{},\"type\":\"0x{:02X}\",\"first_lba\":{},\"last_lba\":{},\"size_bytes\":{},\"fs\":\"{}\"}}",
                p.num, p.os_type, p.start_lba, p.start_lba + p.size_lba.saturating_sub(1),
                p.size_lba as u64 * src.sector_size, fs
            )
        }).collect();
        out.push_str(&parts.join(","));
        out.push_str("]}");
    } else {
        out.push_str(&format!("\"none\",\"sector_size\":{},\"size_bytes\":{}}}", src.sector_size, src.size));
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
    EXIT_OK as i32
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

// ---------- 用户级自动命令：resize / move / create / set ----------

/// 计算用几何：表属"可修复的 stale"（设备扩容后备份头/PMBR 未更新，见 GptState::Stale）时
/// 按修复后的 last_usable_lba 计算；plan 不写盘，实际修复由写入路径的 ensure_geometry 完成
fn effective_last_usable(src: &FileSource, g: &table::RawGpt) -> Result<u64, String> {
    let file_last = src.size / g.ss - 1;
    match movepart::classify_repair(g, file_last) {
        Ok(Some(r)) if r.backup_stale => movepart::repaired_last_usable(g, file_last).map_err(|e| e.to_string()),
        Ok(_) => Ok(g.header.last_usable_lba),
        Err(e) => Err(e.to_string()),
    }
}

/// 目标分区右侧连续空闲扇区数（到下一分区起点或 last_usable+1 为止，GPT）
fn free_right_gpt(g: &table::RawGpt, part: u32) -> u64 {
    let e = &g.entries[(part - 1) as usize];
    let mut bound = g.header.last_usable_lba + 1; // 排他上界
    for (i, o) in g.entries.iter().enumerate() {
        if (i + 1) as u32 == part || (o.starting_lba == 0 && o.ending_lba == 0) {
            continue;
        }
        if o.starting_lba > e.ending_lba {
            bound = bound.min(o.starting_lba);
        }
    }
    bound.saturating_sub(e.ending_lba + 1)
}

/// 已用区间列表 → 1MiB 对齐后的空闲区间 [start,end]（含端点）；
/// 溢出用 saturating，对齐后为空的区间丢弃
fn aligned_gaps(used: &[(u64, u64)], lo: u64, hi: u64, unit: u64) -> Vec<(u64, u64)> {
    let mut sorted: Vec<(u64, u64)> = used.iter().copied().filter(|&(s, e)| e >= s).collect();
    sorted.sort();
    let mut free = Vec::new();
    let mut cur = lo;
    for (s, e) in sorted {
        if s > cur {
            free.push((cur, s - 1));
        }
        cur = cur.max(e.saturating_add(1));
    }
    if cur <= hi {
        free.push((cur, hi));
    }
    free.into_iter()
        .filter_map(|(s, e)| {
            let s2 = s.div_ceil(unit) * unit;
            let e2 = (e.saturating_add(1) / unit * unit).checked_sub(1)?;
            (s2 <= e2).then_some((s2, e2))
        })
        .collect()
}

fn print_plan(plan: &movepart::Plan) {
    println!("plan: grow partition {} → last_usable_lba {} (blockers relocated)", plan.grow_part, plan.last_usable_lba);
    for m in &plan.moves {
        let tag = if m.is_swap { " [swap: recreate, no data move]" } else { "" };
        println!("move part {} : {}..{} → +{} sectors ({} bytes){}",
            m.part_num, m.first_lba, m.first_lba + m.len_lba - 1, m.delta_lba, m.delta_lba * plan.ss, tag);
    }
}

/// SIZE 字符串 →（数值, 类别 0=绝对/1=扩/-1=缩, 是否百分号）。
/// 单位 b/k/m/g/t（1024 进制，大小写均可），无单位 = 字节；"+10%/-10%" 为锚定当前
/// 分区字节数的百分比增量；绝对形式 "10%" 有歧义故不支持；"grow" 由调用方先行处理
fn parse_size_delta(s: &str) -> Option<(u64, i8, bool)> {
    let (kind, rest) = match s.as_bytes().first()? {
        b'+' => (1i8, &s[1..]),
        b'-' => (-1i8, &s[1..]),
        _ => (0i8, s),
    };
    if rest.is_empty() {
        return None;
    }
    // 符号只允许出现在最前、且只出现一次：数字部分再带 +/- 一律拒绝
    // （否则会依赖整数解析器恰好接受前导 '+' 这一实现细节，语法不确定）
    if rest.starts_with('+') || rest.starts_with('-') {
        return None;
    }
    if let Some(num) = rest.strip_suffix('%') {
        if kind == 0 {
            return None;
        }
        return Some((num.parse().ok()?, kind, true));
    }
    let mult = match rest.as_bytes().last()? {
        b'b' | b'B' => 1u64,
        b'k' | b'K' => 1024,
        b'm' | b'M' => 1024 * 1024,
        b'g' | b'G' => 1024 * 1024 * 1024,
        b't' | b'T' => 1024u64 * 1024 * 1024 * 1024,
        b'0'..=b'9' => return rest.parse::<u64>().ok().map(|v| (v, kind, false)),
        _ => return None,
    };
    let n: u64 = rest[..rest.len() - 1].parse().ok()?;
    Some((n.checked_mul(mult)?, kind, false))
}

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
                .unwrap_or_else(|| bail(EXIT_REFUSED, format!("refused: bad SIZE {s:?} (use 10G | +2G | -500M | +10% | grow; see diskedit help resize)")));
            if pct {
                // 百分比增量：锚定当前分区字节数，先乘后除（u128 防溢出）避免丢余数，
                // 再向下取整到 1MiB，保证结果落在扇区/对齐界内
                let raw = (cur_bytes as u128).checked_mul(v as u128)
                    .and_then(|x| x.checked_div(100))
                    .filter(|x| *x <= u64::MAX as u128)
                    .unwrap_or_else(|| bail(EXIT_REFUSED, format!("refused: {s} overflows partition size"))) as u64;
                let delta = raw / (1024 * 1024) * (1024 * 1024);
                target = Some(match kind {
                    1 => cur_bytes.checked_add(delta).unwrap_or_else(|| bail(EXIT_REFUSED, format!("refused: {s} overflows partition size"))),
                    _ => cur_bytes.checked_sub(delta).unwrap_or_else(|| bail(EXIT_REFUSED, format!("refused: {s} exceeds current size {cur_bytes}"))),
                });
            } else {
                target = Some(match kind {
                    0 => v,
                    1 => cur_bytes.checked_add(v).unwrap_or_else(|| bail(EXIT_REFUSED, format!("refused: {s} overflows partition size"))),
                    _ => cur_bytes.checked_sub(v).unwrap_or_else(|| bail(EXIT_REFUSED, format!("refused: {s} exceeds current size {cur_bytes}"))),
                });
            }
        }
    }
    if target.is_none() && !grow_to_end {
        bail(EXIT_REFUSED, "refused: specify a SIZE (e.g. +20G, -500M, 10G, grow; see diskedit help resize)".to_string());
    }
    (target, grow_to_end)
}

/// 块设备的分区节点路径：盘名以数字结尾时分区号加 "p" 前缀
/// （/dev/sda→sda3、/dev/nvme0n1→nvme0n1p3；util-linux 与内核通用命名惯例）
#[cfg(target_os = "linux")]
fn part_dev_path(target: &str, part: u32) -> String {
    let base = target.trim_end_matches('/');
    let sep = if base.chars().last().is_some_and(|c| c.is_ascii_digit()) { "p" } else { "" };
    format!("{base}{sep}{part}")
}

/// 分区扩容成功后的 LVM 链（仅块设备）：pvresize 吸收全部新增空间；--grow-lv 把本次新增
/// 传给目标 LV（--lv 指定或该 PV 上唯一顶层 LV），向下取整到 VG extent，不消费原有空闲。
/// lvextend 按容量扩，分配源由 LVM 决定，不限于本 PV
#[cfg(target_os = "linux")]
fn lvm_grow_chain(part_dev: &str, delta_bytes: u64, grow_lv: bool, want_lv: Option<&str>) -> Result<(), String> {
    lvm::pv_resize(part_dev)?;
    println!("pvresize {part_dev} done");
    if !grow_lv {
        return Ok(());
    }
    let vg = match lvm::vg_of(part_dev) {
        Ok(Some(vg)) => vg,
        Ok(None) => return Err(format!("pvresize done but {part_dev} is a PV outside any VG — nothing to extend")),
        Err(e) => return Err(e),
    };
    let lvs = lvm::lvs_on_pv(&vg, part_dev)?;
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
    let ext = lvm::vg_extent_size(&vg)?;
    let n = delta_bytes / ext;
    if n == 0 {
        return Err(format!("added {delta_bytes} bytes < one VG extent ({ext} bytes) — LV left unchanged"));
    }
    lvm::lv_extend(&path, n)?;
    println!("lvextend -l +{n} -r {path} done (lv {name})");
    Ok(())
}

/// resize 的一键入口。SIZE：`10G`=绝对值、`+2G`/`-500M`=增量、`grow`=吃满右侧可用区
/// （挡路分区须 --allow-move，搬移计划须 --yes 确认）。LVM PV 扩容自动 pvresize，
/// --grow-lv 再把新增传给目标 LV 并扩 FS；PV 缩容一律拒绝（走 lvreduce/pvresize 链）
fn cmd_resize(a: &Args) -> u8 {
    let size_arg = a.pos.get(1).cloned();
    if size_arg.is_some() && (a.size.is_some() || a.grow_to_end) {
        bail(EXIT_REFUSED, "refused: SIZE and --size/--grow-to-end are mutually exclusive".to_string());
    }
    if a.lv.is_some() && !a.grow_lv {
        bail(EXIT_REFUSED, "refused: --lv only works together with --grow-lv".to_string());
    }
    let src = open_target_ro(a).unwrap_or_else(|(c, m)| bail(c, m));
    match table::table_label(&src) {
        Ok("gpt") => {}
        Ok("msdos") => {
            let Some(part) = a.part else { usage() };
            return cmd_resize_msdos(a, part, size_arg.as_deref(), &src);
        }
        // superfloppy：无分区表，FS 即整盘，无表可写——纯 FS grow
        Ok("none") => return cmd_resize_superfloppy(a, size_arg.as_deref(), &src),
        Ok(other) => bail(EXIT_REFUSED, format!("refused: resize requires a GPT or MBR target (label: {other})")),
        Err(e) => bail(EXIT_INFRA, format!("parse failed: {e}")),
    }
    let Some(part) = a.part else { usage() };
    let g = match table::load_gpt(&src) {
        Ok(Some(g)) => g,
        Ok(None) => bail(EXIT_REFUSED, "refused: resize requires a GPT target".to_string()),
        Err(e) => bail(EXIT_INFRA, format!("parse failed: {e}")),
    };
    let Some(e) = g.entries.get((part - 1) as usize) else {
        bail(EXIT_REFUSED, format!("partition {part} not found"));
    };
    if e.ending_lba == 0 {
        bail(EXIT_REFUSED, format!("partition {part} is empty"));
    }
    let (start, end, ss) = (e.starting_lba, e.ending_lba, g.ss);
    let cur_bytes = (end - start + 1) * ss;
    let fstype = fsid::identify(&src, start, end - start + 1).unwrap_or_else(|e| bail(EXIT_INFRA, format!("identify failed: {e}")));
    let is_pv = fstype == "lvm2_pv";

    // SIZE → 绝对目标字节数 / grow 标记
    let (target, grow_to_end) = resolve_size_request(a, size_arg.as_deref(), cur_bytes);
    let shrinking = target.is_some_and(|t| t < cur_bytes);

    if is_pv {
        if shrinking {
            bail(EXIT_REFUSED, "refused: shrinking an LVM PV needs the lvreduce/pvresize chain — do it manually (see pvresize(8))".to_string());
        }
    } else if a.grow_lv {
        bail(EXIT_REFUSED, format!("refused: partition {part} is not an LVM PV (identified as {fstype}) — --grow-lv needs a PV"));
    }

    // 块设备 PV：在线链 partition → pvresize →（--grow-lv）lvextend -r → LV 内 FS。
    // 活动 LV 经 dm 持有分区使 BLKRRPART EBUSY，分区层必须走 sfdisk+partx 同步路径
    #[cfg(target_os = "linux")]
    if src.is_block && is_pv {
        let Some(dn) = std::path::Path::new(&a.target).file_name().map(|s| s.to_string_lossy().into_owned()) else {
            bail(EXIT_REFUSED, format!("cannot derive disk name from {}", a.target));
        };
        refuse_swap_active(&dn, part);
        let new_len = if grow_to_end {
            let free = free_right_gpt(&g, part);
            if free == 0 {
                bail(EXIT_REFUSED, "refused: no free space to the right — a PV cannot relocate blocking partitions while LVs may be active".to_string());
            }
            cur_bytes + free * ss
        } else {
            let bytes = target.unwrap_or(cur_bytes);
            bytes / ss * ss // 扇区下取整，与离线路径同规则
        };
        if new_len != cur_bytes {
            online::resize_pv_online(&dn, part, new_len).unwrap_or_else(|(c, m)| bail(c, m));
        }
        return resize_done(a, true, true, cur_bytes);
    }

    // 块设备：挂载/swap 探测 → 在线路径（在线不能搬移，只吃连续空闲）
    #[cfg(target_os = "linux")]
    if src.is_block {
        let cur = cur_bytes;
        if let Some(dn) = std::path::Path::new(&a.target).file_name().map(|s| s.to_string_lossy().into_owned()) {
            refuse_swap_active(&dn, part);
            if let Some(mnt) = online::find_mountpoint(&dn, part) {
                let size = if grow_to_end {
                    let free = free_right_gpt(&g, part) * ss;
                    (cur + free != cur).then_some(cur + free)
                    // 无右侧空闲 = 分区已吃满 → None 让 FS 工具扩满现分区
                } else {
                    target
                };
                return match online::resize_online(&mnt, size) {
                    Ok(()) => { println!("resized online (verify with: diskedit info {})", a.target); EXIT_OK }
                    Err((c, m)) => { eprintln!("resize failed: {m}"); c }
                };
            }
        }
    }

    // 离线路径
    let is_block = src.is_block;
    let mut src = open_target_for_write(a).unwrap_or_else(|(c, m)| bail(c, m));
    if grow_to_end {
        let free = free_right_gpt(&g, part);
        if free > 0 {
            let (chunk, mut logger) = chunk_logger(a, &src);
            match movepart::resize_part(&mut src, part, start, end + free, chunk, &mut |m| logger.log(m)) {
                Ok(()) => {}
                Err(e) => { eprintln!("resize failed: {e}"); return EXIT_REFUSED; }
            }
            kernel_resync(&src);
            return resize_done(a, is_pv, is_block, cur_bytes);
        }
        // 右侧被挡：自动搬移挡路分区（plan 打印 → --allow-move 放行 → --yes 确认）
        if !a.allow_move {
            bail(EXIT_REFUSED, "refused: right side is occupied — pass --allow-move to relocate the blocking partitions (plan will be printed; --yes confirms)".to_string());
        }
        let plan = movepart::make_plan(&mut src, part).unwrap_or_else(|e| bail(EXIT_REFUSED, format!("plan failed: {e}")));
        print_plan(&plan);
        if !a.yes {
            eprintln!("refused: this resizes by relocating the partitions listed above — review and re-run with --yes");
            return EXIT_REFUSED;
        }
        let (chunk, mut logger) = chunk_logger(a, &src);
        match movepart::apply(&mut src, &plan, chunk, &mut |m| logger.log(m)) {
            Ok(()) => {
                kernel_resync(&src);
                resize_done(a, is_pv, is_block, cur_bytes)
            }
            Err(e) => { eprintln!("resize failed: {e}"); EXIT_PARTIAL }
        }
    } else {
        // SIZE：字节 → 扇区（下取整）；扩须右侧空闲足够，缩由 resize_part 内部 FS 先缩 + 守卫
        let Some(bytes) = target else { usage() };
        if bytes < ss {
            bail(EXIT_REFUSED, format!("refused: size {bytes} < one sector ({ss})"));
        }
        let new_end = start + bytes / ss - 1;
        if new_end > g.header.last_usable_lba {
            bail(EXIT_REFUSED, format!("refused: size {bytes} exceeds usable range (partition would end past last_usable_lba {})", g.header.last_usable_lba));
        }
        if new_end > end && new_end - end > free_right_gpt(&g, part) {
            bail(EXIT_REFUSED, "refused: not enough contiguous free space to the right — `grow` with --allow-move can relocate blockers".to_string());
        }
        let (chunk, mut logger) = chunk_logger(a, &src);
        match movepart::resize_part(&mut src, part, start, new_end, chunk, &mut |m| logger.log(m)) {
            Ok(()) => {}
            Err(e) => { eprintln!("resize failed: {e}"); return EXIT_REFUSED; }
        }
        kernel_resync(&src);
        resize_done(a, is_pv, is_block, cur_bytes)
    }
}

/// superfloppy resize（无分区表，FS 即整盘）：无表可写，唯一有意义的是 FS grow
/// 到盘/镜像末端——缩无处可缩（无分区边界），显式 SIZE 只接受等于当前值。
/// 镜像/盘须已是大尺寸（dd 后或 truncate 预扩），本命令不负责扩文件本身
fn cmd_resize_superfloppy(a: &Args, size_arg: Option<&str>, src_ro: &FileSource) -> u8 {
    if let Some(n) = a.part {
        bail(EXIT_REFUSED, format!("refused: target has no partition table — drop :{n} (the FS occupies the whole device)"));
    }
    if a.grow_lv || a.lv.is_some() {
        bail(EXIT_REFUSED, "refused: --grow-lv/--lv needs an LVM PV — superfloppy has no partitions".to_string());
    }
    let ss = src_ro.sector_size;
    let cur_bytes = src_ro.size;
    let (target, grow_to_end) = resolve_size_request(a, size_arg, cur_bytes);
    if let Some(t) = target {
        if t < cur_bytes {
            bail(EXIT_REFUSED, "refused: superfloppy cannot shrink — the FS occupies the whole device, there is no partition boundary to shrink to".to_string());
        }
        if t > cur_bytes {
            bail(EXIT_REFUSED, "refused: target exceeds device/image size — extend the image or replace the disk first (this tool does not resize the container)".to_string());
        }
        // SIZE == 当前值：与 grow 等价（FS 可能仍小于盘）
    } else if !grow_to_end {
        unreachable!("resolve_size_request guarantees a target or grow_to_end");
    }
    let fstype = fsid::identify(src_ro, 0, cur_bytes / ss)
        .unwrap_or_else(|e| bail(EXIT_INFRA, format!("identify failed: {e}")));
    if matches!(fstype, "lvm2_pv" | "swap" | "unknown") {
        bail(EXIT_REFUSED, format!("refused: whole-device {fstype} is not a resizable filesystem (no partition table on target)"));
    }
    // FS grow 本身不改分区表，无 kernel_resync 必要
    let src = open_target_for_write(a).unwrap_or_else(|(c, m)| bail(c, m));
    fsops::resize_fs_whole(&src, fstype).unwrap_or_else(|e| bail(EXIT_INFRA, format!("FS grow failed: {e}")));
    println!("superfloppy: {fstype} grown to full device ({} bytes) — verify with: diskedit info {}", cur_bytes, a.target);
    EXIT_OK
}

/// MBR 分区右侧连续空闲扇区数（到下一表项起点或盘尾为止；扩展容器起点同样
/// 构成边界——逻辑分区藏在容器内，不可侵入）。MBR 无 usable 区概念，上界=盘尾
fn free_right_msdos(mbr: &[table::MbrPartition], p: &table::MbrPartition, total_sectors: u64) -> u64 {
    let end = p.start_lba as u64 + p.size_lba as u64; // 排他
    let mut bound = total_sectors;
    for o in mbr {
        if o.num == p.num || (o.start_lba as u64) < end {
            continue;
        }
        bound = bound.min(o.start_lba as u64);
    }
    bound.saturating_sub(end)
}

/// resize 的 MBR 分支（仅主分区 1..4；逻辑分区与扩展容器不支持）。原位纯扩缩：扩须右侧
/// 空闲足够（MBR 无搬移能力），缩走与 GPT 相同的"FS 先缩 → 写表"守卫链。块设备复用在线
/// 路径（基于 sysfs + sfdisk，与表类型无关）
fn cmd_resize_msdos(a: &Args, part: u32, size_arg: Option<&str>, src_ro: &FileSource) -> u8 {
    let mbr = table::parse_mbr(src_ro)
        .unwrap_or_else(|e| bail(EXIT_INFRA, format!("parse failed: {e}")))
        .unwrap_or_else(|| bail(EXIT_REFUSED, "refused: no MBR on target".to_string()));
    let p = match mbr.iter().find(|p| p.num == part) {
        Some(p) => p,
        None => bail(EXIT_REFUSED, format!("partition {part} not found (MBR resize covers primary partitions 1..4 only)")),
    };
    if p.is_container {
        bail(EXIT_REFUSED, "refused: extended partition container cannot be resized (logical partitions are out of scope)".to_string());
    }
    let ss = src_ro.sector_size;
    let total_sectors = src_ro.size / ss;
    let cur_bytes = p.size_lba as u64 * ss;
    let fstype = fsid::identify(src_ro, p.start_lba as u64, p.size_lba as u64)
        .unwrap_or_else(|e| bail(EXIT_INFRA, format!("identify failed: {e}")));
    let is_pv = fstype == "lvm2_pv";

    let (target, grow_to_end) = resolve_size_request(a, size_arg, cur_bytes);
    let shrinking = target.is_some_and(|t| t < cur_bytes);

    if is_pv && shrinking {
        bail(EXIT_REFUSED, "refused: shrinking an LVM PV needs the lvreduce/pvresize chain — do it manually (see pvresize(8))".to_string());
    }
    if !is_pv && a.grow_lv {
        bail(EXIT_REFUSED, format!("refused: partition {part} is not an LVM PV (identified as {fstype}) — --grow-lv needs a PV"));
    }
    let is_block = src_ro.is_block;

    // 块设备：swap/挂载探测 → 在线路径（PV 或挂载中分区；写表经 sfdisk，与表类型无关）
    #[cfg(target_os = "linux")]
    if is_block
        && let Some(dn) = std::path::Path::new(&a.target).file_name().map(|s| s.to_string_lossy().into_owned())
    {
        refuse_swap_active(&dn, part);
        if is_pv {
            let new_len = if grow_to_end {
                let free = free_right_msdos(&mbr, p, total_sectors);
                if free == 0 {
                    bail(EXIT_REFUSED, "refused: no free space to the right — a PV cannot relocate blocking partitions while LVs may be active".to_string());
                }
                cur_bytes + free * ss
            } else {
                target.unwrap_or(cur_bytes) / ss * ss
            };
            if new_len != cur_bytes {
                online::resize_pv_online(&dn, part, new_len).unwrap_or_else(|(c, m)| bail(c, m));
            }
            return resize_done(a, true, true, cur_bytes);
        }
        if let Some(mnt) = online::find_mountpoint(&dn, part) {
            let size = if grow_to_end {
                let free = free_right_msdos(&mbr, p, total_sectors) * ss;
                (cur_bytes + free != cur_bytes).then_some(cur_bytes + free)
            } else {
                target
            };
            return match online::resize_online(&mnt, size) {
                Ok(()) => { println!("resized online (verify with: diskedit info {})", a.target); EXIT_OK }
                Err((c, m)) => { eprintln!("resize failed: {m}"); c }
            };
        }
    }

    // 离线路径
    let mut src = open_target_for_write(a).unwrap_or_else(|(c, m)| bail(c, m));
    if grow_to_end {
        let free = free_right_msdos(&mbr, p, total_sectors);
        if free > 0 {
            let new_size_lba = p.size_lba as u64 + free;
            if new_size_lba > u32::MAX as u64 {
                bail(EXIT_REFUSED, format!("refused: new size {new_size_lba} sectors exceeds MBR 32-bit LBA limit"));
            }
            table::resize_mdos_entry(&mut src, part, new_size_lba as u32)
                .unwrap_or_else(|e| bail(EXIT_INFRA, format!("resize failed: {e}")));
            if is_block {
                kernel_resync(&src);
            }
        }
        // free == 0：分区已吃满右侧，表不动，FS 工具直接扩满现分区（与 GPT 路径同语义）
        match fstype {
            "unknown" | "lvm2_pv" => {}
            // swap：内容可弃，表项已扩 → mkswap 重建使新空间生效（UUID/卷标保持；
            // 离线路径仅镜像，块设备走在线路径且 active swap 已被守卫拒绝）
            "swap" => {
                let ident = movepart::read_swap_identity(&src, p.start_lba as u64, p.size_lba as u64, ss);
                if let Err(e) = fsops::recreate_swap(&src, part, ident) {
                    eprintln!("swap {part} resized but mkswap failed: {e} — run mkswap manually (fstab UUID may need it)");
                }
            }
            _ => fsops::resize_fs(&src, part, fstype).unwrap_or_else(|e| bail(EXIT_INFRA, format!("FS grow failed: {e}"))),
        }
        resize_done(a, is_pv, is_block, cur_bytes)
    } else {
        let Some(bytes) = target else { usage() };
        if bytes < ss {
            bail(EXIT_REFUSED, format!("refused: size {bytes} < one sector ({ss})"));
        }
        let new_size_lba = bytes / ss;
        if new_size_lba > u32::MAX as u64 {
            bail(EXIT_REFUSED, format!("refused: size {new_size_lba} sectors exceeds MBR 32-bit LBA limit"));
        }
        if new_size_lba >= p.size_lba as u64 {
            // no-op（==）不触发缩容守卫链；扩（>）须右侧空闲足够
            let want = new_size_lba - p.size_lba as u64;
            if want > free_right_msdos(&mbr, p, total_sectors) {
                bail(EXIT_REFUSED, "refused: not enough contiguous free space to the right (MBR resize cannot relocate blocking partitions)".to_string());
            }
        } else {
            // 缩：与 movepart GPT 路径同守卫链——FS 先缩成功才写表
            match fstype {
                "lvm2_pv" => unreachable!("PV shrink refused above"),
                "unknown" => bail(EXIT_REFUSED, "cannot shrink: filesystem type unrecognized (shrinking the partition without resizing the FS first would corrupt data)".to_string()),
                _ if !movepart::fs_can_shrink(fstype) => bail(EXIT_REFUSED, format!("fs {fstype} cannot shrink; aborting before any write")),
                _ => {}
            }
            if let Some(min) = fsops::fs_min_bytes(&src, part, fstype)
                .unwrap_or_else(|e| bail(EXIT_INFRA, format!("min-size probe failed: {e}")))
                && bytes < min
            {
                bail(EXIT_REFUSED, format!("refused: target size {bytes} < minimum FS size {min} bytes (resize2fs -P)"));
            }
            fsops::shrink_fs(&src, part, fstype, new_size_lba * ss)
                .unwrap_or_else(|e| bail(EXIT_INFRA, format!("FS shrink failed: {e}")));
        }
        table::resize_mdos_entry(&mut src, part, new_size_lba as u32)
            .unwrap_or_else(|e| bail(EXIT_INFRA, format!("resize failed: {e}")));
        if is_block {
            kernel_resync(&src);
        }
        resize_done(a, is_pv, is_block, cur_bytes)
    }
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
        let new_bytes = {
            let src = open_target_ro(a).unwrap_or_else(|(c, m)| bail(c, m));
            // 表项重读按 label 分派（MBR resize 也走本收尾）
            if let Ok(Some(g)) = table::load_gpt(&src) {
                match g.entries.get((a.part.unwrap_or(0) as usize).checked_sub(1).unwrap_or(usize::MAX)) {
                    Some(e) if e.ending_lba != 0 => (e.ending_lba - e.starting_lba + 1) * g.ss,
                    _ => bail(EXIT_INFRA, "post-resize: partition vanished from table".to_string()),
                }
            } else if let Ok(Some(mbr)) = table::parse_mbr(&src) {
                match mbr.iter().find(|p| p.num == a.part.unwrap_or(0)) {
                    Some(p) => p.size_lba as u64 * src.sector_size,
                    None => bail(EXIT_INFRA, "post-resize: partition vanished from table".to_string()),
                }
            } else {
                bail(EXIT_INFRA, "post-resize: no partition table on target".to_string())
            }
        };
        let delta = new_bytes.saturating_sub(old_bytes);
        let part = a.part.unwrap_or(0);
        let r = if is_block {
            lvm_grow_chain(&part_dev_path(&a.target, part), delta, a.grow_lv, a.lv.as_deref())
        } else {
            // offset+sizelimit 映射出的 loop 设备 = 该分区的整块设备，PV 整设备语义下
            // pvresize/lvextend 直接可用，无需 -P partscan
            let src = open_target_ro(a).unwrap_or_else(|(c, m)| bail(c, m));
            fsops::with_partition_device(&src, part, |pv| {
                lvm_grow_chain(pv, delta, a.grow_lv, a.lv.as_deref()).map_err(std::io::Error::other)
            })
            .map_err(|e| e.to_string())
        };
        match r {
            Ok(()) => EXIT_OK,
            Err(e) => {
                eprintln!("partition resized but LVM chain failed: {e}");
                EXIT_PARTIAL
            }
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (is_block, old_bytes);
        eprintln!("partition resized but LVM chain NOT executed: pvresize/lvextend require Linux — run pvresize on the partition manually");
        EXIT_PARTIAL
    }
}

fn cmd_move(a: &Args) -> u8 {
    let (Some(part), start_opt) = (a.part, a.start) else { usage() };
    if !a.start_end && start_opt.is_none() {
        usage();
    }
    let mut src = open_target_for_write(a).unwrap_or_else(|(c, m)| bail(c, m));
    let g = match table::load_gpt(&src) {
        Ok(Some(g)) => g,
        Ok(None) => bail(EXIT_REFUSED, "refused: move requires a GPT target".to_string()),
        Err(e) => bail(EXIT_INFRA, format!("parse failed: {e}")),
    };
    let Some(e) = g.entries.get((part - 1) as usize) else {
        bail(EXIT_REFUSED, format!("partition {part} not found"));
    };
    if e.ending_lba == 0 {
        bail(EXIT_REFUSED, format!("partition {part} is empty"));
    }
    // 平移保持长度（本工具语义）：new_end = new_start + 原长度 - 1
    let len = e.ending_lba - e.starting_lba + 1;
    let start = if a.start_end {
        effective_last_usable(&src, &g).unwrap_or_else(|m| bail(EXIT_REFUSED, m))
            .checked_sub(len - 1)
            .unwrap_or_else(|| bail(EXIT_REFUSED, "refused: partition longer than usable range".to_string()))
    } else {
        align_start(a, start_opt.unwrap(), g.ss)
    };
    // checked：start 来自 CLI 原始输入（--align none 时无上界），回绕会骗过 resize_part 的边界校验
    let end = start.checked_add(len - 1)
        .unwrap_or_else(|| bail(EXIT_REFUSED, "refused: end LBA overflows address space".to_string()));
    let (chunk, mut logger) = chunk_logger(a, &src);
    match movepart::resize_part(&mut src, part, start, end, chunk, &mut |m| logger.log(m)) {
        Ok(()) => { kernel_resync(&src); println!("moved (verify with: diskedit info {})", a.target); EXIT_OK }
        Err(e) => { eprintln!("move failed: {e}"); EXIT_REFUSED }
    }
}

/// create 的一键入口：自动选空闲槽（--size 给定取首个装得下的，否则取最大者），1MiB 对齐
fn cmd_create(a: &Args) -> u8 {
    let mut src = open_target_for_write(a).unwrap_or_else(|(c, m)| bail(c, m));
    let ss = src.sector_size;
    let unit = (1024 * 1024 / ss).max(1);
    let want = a.size.map(|b| {
        if b < ss { bail(EXIT_REFUSED, format!("refused: size {b} < one sector ({ss})")); }
        b / ss
    });
    let gaps = match table::table_label(&src) {
        Ok("gpt") => {
            let g = match table::load_gpt(&src) {
                Ok(Some(g)) => g,
                Ok(None) => bail(EXIT_REFUSED, "refused: no GPT on target — run `new` first".to_string()),
                Err(e) => bail(EXIT_INFRA, format!("parse failed: {e}")),
            };
            let used: Vec<(u64, u64)> = g.entries.iter()
                .filter(|e| !(e.starting_lba == 0 && e.ending_lba == 0))
                .map(|e| (e.starting_lba, e.ending_lba)).collect();
            let last_usable = effective_last_usable(&src, &g).unwrap_or_else(|m| bail(EXIT_REFUSED, m));
            aligned_gaps(&used, g.header.first_usable_lba, last_usable, unit)
        }
        Ok("msdos") => {
            let mbr = match table::parse_mbr(&src) {
                Ok(Some(m)) => m,
                Ok(None) => bail(EXIT_REFUSED, "refused: no partition table on target — run `new` first".to_string()),
                Err(e) => bail(EXIT_INFRA, format!("parse failed: {e}")),
            };
            let used: Vec<(u64, u64)> = mbr.iter().map(|p| (p.start_lba as u64, p.start_lba as u64 + p.size_lba as u64 - 1)).collect();
            let disk_last = src.size / ss - 1;
            aligned_gaps(&used, unit, disk_last, unit)
        }
        Ok(other) => bail(EXIT_REFUSED, format!("cannot create on {other} label — run `new` first")),
        Err(e) => bail(EXIT_INFRA, format!("label probe failed: {e}")),
    };
    if gaps.is_empty() {
        bail(EXIT_REFUSED, "refused: no free space (after 1MiB alignment)".to_string());
    }
    let (start, end) = match want {
        Some(n) => match gaps.iter().find(|(s, e)| e - s + 1 >= n) {
            Some(&(s, e)) => (s, (s + n - 1).min(e)), // 区间端点落在间隙内
            None => bail(EXIT_REFUSED, format!("refused: no aligned gap fits {n} sectors; free gaps: {gaps:?}")),
        },
        None => *gaps.iter().max_by_key(|(s, e)| e - s + 1).unwrap(),
    };
    let label = table::table_label(&src).unwrap_or("");
    let r = if label == "gpt" {
        table::add_entry(&mut src, start, end, a.name.as_deref().unwrap_or(""), table::LINUX_FS_TYPE_GUID)
    } else {
        table::add_mdos_entry(&mut src, start, end, 0x83)
    };
    let num = match r {
        Ok(n) => n,
        Err(e) => { eprintln!("create failed: {e}"); return EXIT_REFUSED; }
    };
    kernel_resync(&src);
    if let Some(fstype) = &a.fs
        && let Err(e) = fsops::mkfs(&src, num, fstype)
    {
        eprintln!("partition #{num} created but mkfs failed: {e}");
        return EXIT_PARTIAL;
    }
    println!("created partition #{num} at {start}..{end} (verify with: diskedit info {})", a.target);
    EXIT_OK
}

/// set 的一键入口：统一 name/label/uuid/flag 四类属性
fn cmd_set(a: &Args) -> u8 {
    let Some(part) = a.part else { usage() };
    let key = a.pos.get(1).map(|s| s.as_str()).unwrap_or_else(|| usage());
    let value = a.pos.get(2).cloned().unwrap_or_default();
    let state = a.pos.get(3).cloned().unwrap_or_default();
    let mut src = open_target_for_write(a).unwrap_or_else(|(c, m)| bail(c, m));
    if key == "name" {
        if value.is_empty() { usage(); }
        return match table::rename_entry(&mut src, part, &value) {
            Ok(()) => { kernel_resync(&src); println!("renamed partition #{part} to {value:?}"); EXIT_OK }
            Err(e) => { eprintln!("set name failed: {e}"); EXIT_REFUSED }
        };
    }
    if key == "flag" {
        if value.is_empty() { usage(); }
        let on = match state.as_str() { "on" => true, "off" => false, _ => usage() };
        let r = match table::table_label(&src) {
            Ok("gpt") => table::set_gpt_flag(&mut src, part, &value, on),
            Ok("msdos") if value == "boot" => table::set_mdos_boot(&mut src, part, on),
            Ok("msdos") if value == "hidden" => table::set_mdos_hidden(&mut src, part, on),
            Ok("msdos") => bail(EXIT_REFUSED, "msdos flags: only `boot` and `hidden` are supported".to_string()),
            Ok(other) => bail(EXIT_REFUSED, format!("cannot set flag on {other} label")),
            Err(e) => bail(EXIT_INFRA, format!("label probe failed: {e}")),
        };
        return match r {
            Ok(()) => { kernel_resync(&src); println!("flag {value}={on} on partition #{part}"); EXIT_OK }
            Err(e) => { eprintln!("set flag failed: {e}"); EXIT_REFUSED }
        };
    }
    // label/uuid 需要 FS 识别
    let (start, len) = get_entry(&src, part).unwrap_or_else(|(c, m)| bail(c, m));
    let fstype = fsid::identify(&src, start, len).unwrap_or_else(|e| bail(EXIT_INFRA, format!("identify failed: {e}")));
    let r = match key {
        "label" if !value.is_empty() => fsops::set_label(&src, part, fstype, &value),
        "uuid" if !value.is_empty() => fsops::set_uuid(&src, part, fstype, &value),
        "label" | "uuid" => usage(),
        _ => usage(),
    };
    match r {
        Ok(()) => { println!("set {key} on partition #{part} ({fstype})"); EXIT_OK }
        Err(e) => { eprintln!("set {key} failed: {e}"); EXIT_INFRA }
    }
}

fn main() -> ExitCode {
    let (cmd, a) = parse_args();
    let code: u8 = match cmd.as_str() {
        "help" | "--help" | "-h" => match a.pos.first() {
            Some(t) => help_cmd(t),
            None => usage(),
        },
        "info" => cmd_info(&a) as u8,
        "resize" => cmd_resize(&a),
        "move" => cmd_move(&a),
        "create" => cmd_create(&a),
        "set" => cmd_set(&a),
        "ls" => {
            let Some(part) = a.part else { usage() };
            // 纯浏览：只读打开（块设备 RW+O_EXCL 在盘被 claim 时会被内核拒绝）
            let src = open_target_ro(&a).unwrap_or_else(|(c, m)| bail(c, m));
            let path = a.path.as_deref().unwrap_or("/");
            match list::ls(&src.path, Some(part), path) {
                Ok(entries) => {
                    for (name, kind, size) in entries {
                        println!("{kind}\t{size}\t{name}");
                    }
                    EXIT_OK
                }
                Err(e) => { eprintln!("ls failed: {e}"); EXIT_REFUSED }
            }
        }
        "cat" => {
            let (Some(part), Some(path)) = (a.part, a.path.clone()) else { usage() };
            // 纯读取：只读打开（块设备 RW+O_EXCL 在盘被 claim 时会被内核拒绝）
            let src = open_target_ro(&a).unwrap_or_else(|(c, m)| bail(c, m));
            let stdout = std::io::stdout();
            let mut lock = stdout.lock();
            match list::cat_to(&src.path, Some(part), &path, &mut lock) {
                Ok(_) => EXIT_OK,
                Err(e) => { eprintln!("cat failed: {e}"); EXIT_REFUSED }
            }
        }
        "mkfs" => {
            let (Some(part), Some(fstype)) = (a.part, a.fstype.clone()) else { usage() };
            if !a.yes {
                eprintln!("refused: mkfs destroys all data on partition {part}; pass --yes to confirm");
                EXIT_REFUSED
            } else {
                let src = open_target(&a).unwrap_or_else(|(c, m)| bail(c, m));
                if let Err((c, m)) = get_entry(&src, part) {
                    bail(c, format!("refused: {m}"));
                }
                match fsops::mkfs(&src, part, &fstype) {
                    Ok(()) => EXIT_OK,
                    Err(e) => { eprintln!("mkfs failed: {e}"); EXIT_INFRA }
                }
            }
        }
        "resizefs" => {
            if a.online {
                // 在线路径：positional[0] = 挂载点（非 :N），positional[1] = 可选绝对字节数
                if a.part.is_some() {
                    bail(EXIT_REFUSED, "--online takes a mountpoint, not <target>:N".to_string());
                }
                let size = a.fstype.as_ref().map(|s| s.parse::<u64>().unwrap_or_else(|_| {
                    bail(EXIT_REFUSED, format!("size {s:?} must be bytes"))
                }));
                #[cfg(target_os = "linux")]
                {
                    match online::resize_online(std::path::Path::new(&a.target), size) {
                        Ok(()) => {
                            println!("resized online (verify with: diskedit info {})", a.target);
                            EXIT_OK
                        }
                        Err((c, m)) => {
                            eprintln!("resizefs failed: {m}");
                            c
                        }
                    }
                }
                #[cfg(not(target_os = "linux"))]
                {
                    let _ = size;
                    bail(EXIT_INFRA, "online resize requires Linux".to_string());
                }
            } else {
                let Some(part) = a.part else { usage() };
                let src = open_target(&a).unwrap_or_else(|(c, m)| bail(c, m));
                let (start, len) = get_entry(&src, part).unwrap_or_else(|(c, m)| bail(c, m));
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
        "new" => {
            if !a.yes {
                eprintln!("refused: `new` overwrites any existing partition table; pass --yes to confirm");
                EXIT_REFUSED
            } else {
                let mut src = open_target_for_write(&a).unwrap_or_else(|(c, m)| bail(c, m));
                let ss = src.sector_size;
                let r = match a.table.as_deref() {
                    Some("msdos") => table::create_mbr(&mut src),
                    Some("gpt") | None => table::create_gpt(&mut src, ss, None),
                    Some(other) => bail(EXIT_REFUSED, format!("unsupported table type {other:?} (gpt|msdos)")),
                };
                match r {
                    Ok(()) => {
                        kernel_resync(&src);
                        println!("created {} table (verify with: diskedit info {})", a.table.as_deref().unwrap_or("gpt"), a.target);
                        EXIT_OK
                    }
                    Err(e) => { eprintln!("new failed: {e}"); EXIT_INFRA }
                }
            }
        }
        "add" => {
            let (Some(start), Some(end)) = (a.start, a.end) else { usage() };
            let mut src = open_target_for_write(&a).unwrap_or_else(|(c, m)| bail(c, m));
            let (start, end) = align_range(&a, start, end, src.sector_size);
            match table::table_label(&src) {
                Ok("gpt") => {
                    let type_guid = match &a.type_guid {
                        Some(s) => parse_guid(s).unwrap_or_else(|| bail(EXIT_REFUSED, format!("invalid GUID {s:?} (expect standard text like C12A7328-F81F-11D2-BA4B-00A0C93EC93B, hyphens optional)"))),
                        None => table::LINUX_FS_TYPE_GUID, // Linux filesystem data（util-linux GPT_DEFAULT_ENTRY_TYPE）
                    };
                    match table::add_entry(&mut src, start, end, a.name.as_deref().unwrap_or(""), type_guid) {
                        Ok(num) => {
                            kernel_resync(&src);
                            println!("added partition #{num} (verify with: diskedit info {})", a.target);
                            EXIT_OK
                        }
                        Err(e) => { eprintln!("add failed: {e}"); EXIT_REFUSED }
                    }
                }
                Ok("msdos") => {
                    let os_type = match &a.type_guid {
                        Some(s) => {
                            let hex = s.trim_start_matches("0x");
                            u8::from_str_radix(hex, 16).unwrap_or_else(|_| bail(EXIT_REFUSED, format!("invalid MBR type {s:?} (expect 0xXX)")))
                        }
                        // 默认 Linux 数据分区（util-linux pt-mbr.h MBR_LINUX_DATA_PARTITION）
                        None => 0x83, // Linux
                    };
                    match table::add_mdos_entry(&mut src, start, end, os_type) {
                        Ok(num) => {
                            kernel_resync(&src);
                            println!("added partition #{num} (verify with: diskedit info {})", a.target);
                            EXIT_OK
                        }
                        Err(e) => { eprintln!("add failed: {e}"); EXIT_REFUSED }
                    }
                }
                Ok(other) => bail(EXIT_REFUSED, format!("cannot add on {other} label — run `new` first")),
                Err(e) => bail(EXIT_INFRA, format!("label probe failed: {e}")),
            }
        }
        "del" | "delete" => {
            let Some(part) = a.part else { usage() };
            if !a.yes {
                eprintln!("refused: `del` removes partition entry {part}; pass --yes to confirm");
                EXIT_REFUSED
            } else {
                let mut src = open_target_for_write(&a).unwrap_or_else(|(c, m)| bail(c, m));
                let r = match table::table_label(&src) {
                    Ok("gpt") => table::del_entry(&mut src, part),
                    Ok("msdos") => table::del_mdos_entry(&mut src, part),
                    Ok(other) => bail(EXIT_REFUSED, format!("cannot del on {other} label")),
                    Err(e) => bail(EXIT_INFRA, format!("label probe failed: {e}")),
                };
                match r {
                    Ok(()) => {
                        kernel_resync(&src);
                        println!("deleted partition #{part} (verify with: diskedit info {})", a.target);
                        EXIT_OK
                    }
                    Err(e) => { eprintln!("del failed: {e}"); EXIT_REFUSED }
                }
            }
        }
        "resize-part" => {
            let (Some(part), Some(start)) = (a.part, a.start) else { usage() };
            if a.grow_to_end && a.end.is_some() {
                bail(EXIT_REFUSED, "refused: --end and --grow-to-end are mutually exclusive".to_string());
            }
            let mut src = open_target_for_write(&a).unwrap_or_else(|(c, m)| bail(c, m));
            let end = if a.grow_to_end {
                // 吃满后方可用区（本工具语义）：扩到 last_usable_lba；
                // 后方有分区时由 resize_part 的重叠校验拒绝
                match table::load_gpt(&src) {
                    Ok(Some(g)) => effective_last_usable(&src, &g).unwrap_or_else(|m| bail(EXIT_REFUSED, m)),
                    Ok(None) => bail(EXIT_REFUSED, "refused: --grow-to-end requires a GPT target".to_string()),
                    Err(e) => bail(EXIT_INFRA, format!("parse failed: {e}")),
                }
            } else {
                let Some(e) = a.end else { usage() };
                e
            };
            // --grow-to-end：end 钉死 last_usable 不做下取整（吃满语义），--align 仅作用于 start；
            // 常规路径 start 上取整、end 下取整
            let (start, end) = if a.grow_to_end {
                (align_start(&a, start, src.sector_size), end)
            } else {
                align_range(&a, start, end, src.sector_size)
            };
            let (chunk, mut logger) = chunk_logger(&a, &src);
            match movepart::resize_part(&mut src, part, start, end, chunk, &mut |m| logger.log(m)) {
                Ok(()) => {
                    kernel_resync(&src);
                    println!("resize-part complete (verify with: diskedit info {})", a.target);
                    EXIT_OK
                }
                Err(e) => { eprintln!("resize-part failed: {e}"); EXIT_REFUSED }
            }
        }
        "copy" => {
            let (Some(part), start_opt) = (a.part, a.start) else { usage() };
            if !a.start_end && start_opt.is_none() {
                usage();
            }
            let mut src = open_target_for_write(&a).unwrap_or_else(|(c, m)| bail(c, m));
            let start = if a.start_end {
                let g = match table::load_gpt(&src) {
                    Ok(Some(g)) => g,
                    Ok(None) => bail(EXIT_REFUSED, "refused: --start end requires a GPT target".to_string()),
                    Err(e) => bail(EXIT_INFRA, format!("parse failed: {e}")),
                };
                let e = g.entries.get((part - 1) as usize)
                    .unwrap_or_else(|| bail(EXIT_REFUSED, format!("partition {part} not found")));
                if e.ending_lba == 0 {
                    bail(EXIT_REFUSED, format!("partition {part} is empty"));
                }
                let len = e.ending_lba - e.starting_lba + 1;
                let last_usable = effective_last_usable(&src, &g).unwrap_or_else(|m| bail(EXIT_REFUSED, m));
                last_usable
                    .checked_sub(len - 1)
                    .unwrap_or_else(|| bail(EXIT_REFUSED, "refused: partition longer than usable range".to_string()))
            } else {
                align_start(&a, start_opt.unwrap(), src.sector_size)
            };
            let (chunk, mut logger) = chunk_logger(&a, &src);
            match movepart::copy_part(&mut src, part, start, a.name.as_deref().unwrap_or(""), chunk, &mut |m| logger.log(m)) {
                Ok(num) => {
                    kernel_resync(&src);
                    println!("copied to partition #{num} (verify with: diskedit info {})", a.target);
                    EXIT_OK
                }
                Err(e) => { eprintln!("copy failed: {e}"); EXIT_REFUSED }
            }
        }
        "undo" => {
            if !a.yes {
                eprintln!("refused: `undo` overwrites current bytes from journal; pass --yes to confirm");
                EXIT_REFUSED
            } else {
                let mut src = open_target(&a).unwrap_or_else(|(c, m)| bail(c, m));
                let p = journal_file(&src);
                let entries = match Journal::read_entries(&p) {
                    Ok(v) if !v.is_empty() => v,
                    Ok(_) => bail(EXIT_REFUSED, "nothing to undo (journal is empty)".to_string()),
                    Err(e) => bail(EXIT_REFUSED, format!("no usable journal: {e}")),
                };
                let n = entries.len();
                for (off, data) in entries.iter().rev() {
                    if let Err(e) = src.write_at(*off, data) {
                        // journal 保留在原地：可重试 undo
                        bail(EXIT_INFRA, format!("undo write failed at offset {off}: {e} (journal kept, retry)"));
                    }
                }
                let _ = src.sync_all();
                let _ = std::fs::remove_file(&p);
                kernel_resync(&src);
                println!("undone {n} journal entries (verify with: diskedit info {})", a.target);
                EXIT_OK
            }
        }
        "check" => {
            let Some(part) = a.part else { usage() };
            let src = open_target(&a).unwrap_or_else(|(c, m)| bail(c, m));
            let (start, len) = get_entry(&src, part).unwrap_or_else(|(c, m)| bail(c, m));
            let fstype = fsid::identify(&src, start, len).unwrap_or_else(|e| bail(EXIT_INFRA, format!("identify failed: {e}")));
            match fsops::check_fs(&src, part, fstype) {
                Ok(()) => { println!("check done on partition #{part} ({fstype})"); EXIT_OK }
                Err(e) => { eprintln!("check failed: {e}"); EXIT_INFRA }
            }
        }
        "plan" | "apply" => {
            let Some(grow) = a.grow else { usage() };
            let mut src = open_target(&a).unwrap_or_else(|(c, m)| bail(c, m));
            let plan = movepart::make_plan(&mut src, grow).unwrap_or_else(|e| bail(EXIT_REFUSED, format!("plan failed: {e}")));
            if cmd == "plan" {
                if let Some(r) = &plan.repair {
                    // 修复动作只记录、不执行；apply 会先做这一步再搬数据
                    if r.backup_stale {
                        println!("[repair] relocate backup GPT: LBA {} → {}", r.backup_lba, r.file_last_lba);
                    } else {
                        println!("[repair] rewrite protective MBR (SizeInLBA stale)");
                    }
                }
                println!("grow partition {} → last_usable_lba {}", plan.grow_part, plan.last_usable_lba);
                for m in &plan.moves {
                    let tag = if m.is_swap { " [swap: recreate, no data move]" } else { "" };
                    println!("move part {} : {}..{} → +{} sectors ({} bytes){}",
                        m.part_num, m.first_lba, m.first_lba + m.len_lba - 1, m.delta_lba, m.delta_lba * plan.ss, tag);
                }
                EXIT_OK
            } else {
                apply_cmd(&a, plan)
            }
        }
        _ => usage(),
    };
    ExitCode::from(code)
}

fn apply_cmd(a: &Args, plan: movepart::Plan) -> u8 {
    let mut src = match open_target_for_write(a) { Ok(s) => s, Err((c, m)) => { eprintln!("{m}"); return c; } };
    let guid = match table::load_gpt(&src) {
        Ok(Some(g)) => g.header.disk_guid,
        _ => return EXIT_REFUSED,
    };
    let chunk = match movepart::chunk_bytes(a.chunk_mib) {
        Ok(c) => c,
        Err(e) => { eprintln!("refused: {e}"); return EXIT_REFUSED; }
    };
    let mut logger = Logger::open(&src);
    match movepart::apply(&mut src, &plan, chunk, &mut |m| logger.log(m)) {
        Ok(()) => {
            kernel_resync(&src);
            println!("apply complete (verify with: diskedit info {})", a.target);
            EXIT_OK
        }
        Err(e) => {
            let msg = e.to_string();
            logger.log(&format!("apply failed: {msg}"));
            eprintln!("apply failed: {msg}");
            eprintln!("manual recovery: gdisk/sgdisk -v on target shows current table state; checkpoint (if present) holds the plan");
            // checkpoint 仍在 = 已有持久变更（部分完成）；被清掉 = 拒绝
            if movepart::checkpoint_path(&src, guid).map(|p| p.exists()).unwrap_or(false) {
                EXIT_PARTIAL
            } else {
                EXIT_REFUSED
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn src_from(tag: &str, data: &[u8]) -> FileSource {
        let mut tmp = std::env::temp_dir();
        tmp.push(format!("diskedit_main_{tag}_{}.img", std::process::id()));
        std::fs::write(&tmp, data).unwrap();
        let f = std::fs::OpenOptions::new().read(true).write(true).open(&tmp).unwrap();
        FileSource { file: f, path: tmp, sector_size: 512, size: data.len() as u64, is_block: false, journal: None }
    }

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

    fn base_args() -> Args {
        Args {
            target: String::new(), part: None, fstype: None, path: None, state: None, grow: None,
            start: None, end: None, size: None, fs: None, name: None, type_guid: None, table: None,
            yes: false, online: false, sector_size: None, align: "mib".to_string(), chunk_mib: 4,
            grow_to_end: false, allow_move: false, grow_lv: false, lv: None, start_end: false, pos: Vec::new(),
        }
    }

    /// SIZE 解析（单位 b/k/m/g/t = 1024 进制、无单位 = 字节、+/- 增量、+N%/-N%）
    #[test]
    fn size_delta_parsing() {
        const G: u64 = 1024 * 1024 * 1024;
        assert_eq!(parse_size_delta("1024"), Some((1024, 0, false)));
        assert_eq!(parse_size_delta("10G"), Some((10 * G, 0, false)));
        assert_eq!(parse_size_delta("10g"), Some((10 * G, 0, false)));
        assert_eq!(parse_size_delta("512B"), Some((512, 0, false)));
        assert_eq!(parse_size_delta("+2k"), Some((2048, 1, false)));
        assert_eq!(parse_size_delta("-500M"), Some((500 * 1024 * 1024, -1, false)));
        assert_eq!(parse_size_delta("1T"), Some((1024 * G, 0, false)));
        assert_eq!(parse_size_delta("+10%"), Some((10, 1, true)));
        assert_eq!(parse_size_delta("-10%"), Some((10, -1, true)));
        // u64 边界与乘法溢出：溢出返回 None，绝不回绕
        assert_eq!(parse_size_delta("18446744073709551615"), Some((u64::MAX, 0, false)));
        assert_eq!(parse_size_delta("18446744073709551615G"), None);
        assert_eq!(parse_size_delta("18446744073709551616"), None);
        // 绝对百分比有歧义（盘的 10% 还是分区的 10%），不支持；其余非法输入
        for bad in ["", "+", "-", "%", "10%", "+%", "G", "+G", "10KB", "1.5G", "10 G", "0x10", " 1G"] {
            assert_eq!(parse_size_delta(bad), None, "{bad:?} must be rejected");
        }
        // 符号只在最前出现一次：+10G/-10G 合法，重复/混用符号一律非法
        assert_eq!(parse_size_delta("+10G"), Some((10 * G, 1, false)));
        assert_eq!(parse_size_delta("-10G"), Some((10 * G, -1, false)));
        for bad in ["++10G", "--10G", "+++10G", "+-10G", "-+10G", "++10%", "+ 10G"] {
            assert_eq!(parse_size_delta(bad), None, "{bad:?} must be rejected");
        }
    }

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

    fn raw_gpt(last_usable: u64, ents: &[(u64, u64)]) -> table::RawGpt {
        table::RawGpt {
            ss: 512,
            state: table::GptState::Valid,
            pmbr: table::PmbrSize::Normal,
            header: table::RawHeader {
                primary_lba: 1,
                backup_lba: last_usable + 1,
                first_usable_lba: 34,
                last_usable_lba: last_usable,
                disk_guid: [0; 16],
                partition_entry_lba: 2,
                number_of_partition_entries: 128,
                size_of_partition_entry: 128,
            },
            entries: ents.iter().map(|&(s, e)| gptman::GPTPartitionEntry {
                partition_type_guid: [1; 16],
                unique_partition_guid: [2; 16],
                starting_lba: s,
                ending_lba: e,
                attribute_bits: 0,
                partition_name: "".into(),
            }).collect(),
        }
    }

    /// 右侧连续空闲：取"下一个分区起点"与"last_usable+1"的较小者
    #[test]
    fn free_right_bounds() {
        // 分区 1 右侧紧邻分区 2 → 无空闲
        let g = raw_gpt(1000, &[(100, 199), (200, 299), (0, 0)]);
        assert_eq!(free_right_gpt(&g, 1), 0);
        assert_eq!(free_right_gpt(&g, 2), 1000 + 1 - 300);
        // 右侧隔着空隙 → 以邻分区起点为界
        let g = raw_gpt(1000, &[(100, 199), (300, 399), (0, 0)]);
        assert_eq!(free_right_gpt(&g, 1), 300 - 200);
        assert_eq!(free_right_gpt(&g, 2), 1000 + 1 - 400);
        // 左侧分区不计入（起点小于本分区末端的都被忽略）
        let g = raw_gpt(1000, &[(50, 99), (100, 199), (0, 0)]);
        assert_eq!(free_right_gpt(&g, 2), 1000 + 1 - 200);
    }

    /// 1MiB 对齐空闲区间：边界 + 子集穷举（覆盖性、不重叠、单位对齐）
    #[test]
    fn aligned_gaps_boundaries() {
        const U: u64 = 1024 * 1024;
        // 无 used → 整段，末端上取整后回退一字节
        assert_eq!(aligned_gaps(&[], 0, 4 * U - 1, U), vec![(0, 4 * U - 1)]);
        // 末端不在单位界上：向下取整到单位界（不越界）
        assert_eq!(aligned_gaps(&[], 0, 3 * U - 10, U), vec![(0, 2 * U - 1)]);
        // 起点在单位界之间：向上取整
        assert_eq!(aligned_gaps(&[], U + 1, 3 * U - 1, U), vec![(2 * U, 3 * U - 1)]);
        // 相邻/重叠/乱序 used 等价于排序合并
        assert_eq!(aligned_gaps(&[(0, U - 1), (U, 2 * U - 1)], 0, 2 * U - 1, U), vec![]);
        assert_eq!(
            aligned_gaps(&[(3 * U, 4 * U - 1), (0, U - 1), (U, 2 * U - 1)], 0, 8 * U - 1, U),
            vec![(2 * U, 3 * U - 1), (4 * U, 8 * U - 1)]
        );
        // lo > hi（无可用区）→ 空
        assert_eq!(aligned_gaps(&[], 4 * U, 2 * U, U), vec![]);
        // 末位取满：hi = u64::MAX 时 saturating 不回绕
        assert_eq!(aligned_gaps(&[], 0, u64::MAX, U).last().copied(), Some((0, u64::MAX / U * U - 1)));

        // 穷举 8 个单位块的 256 种子集：不变量逐条验
        for unit in [1u64, 2, 4, 8] {
            for mask in 0u32..256 {
                let used: Vec<(u64, u64)> = (0..8u64)
                    .filter(|i| mask >> i & 1 == 1)
                    .map(|i| (i * unit, (i + 1) * unit - 1))
                    .collect();
                let hi = 8 * unit - 1;
                let gaps = aligned_gaps(&used, 0, hi, unit);
                for &(s, e) in &gaps {
                    assert_eq!(s % unit, 0, "start not aligned: unit {unit} mask {mask:#x}");
                    assert_eq!((e + 1) % unit, 0, "end not aligned: unit {unit} mask {mask:#x}");
                    assert!(s <= e && e <= hi, "gap {s}..{e} out of range");
                    for &(us, ue) in &used {
                        assert!(e < us || s > ue, "gap {s}..{e} overlaps used {us}..{ue}");
                    }
                }
                for i in 0..8u64 {
                    let blk = (i * unit, (i + 1) * unit - 1);
                    let covered = gaps.iter().any(|&(s, e)| s <= blk.0 && blk.1 <= e);
                    assert_eq!(covered, mask >> i & 1 == 0, "coverage mismatch: unit {unit} mask {mask:#x} block {i}");
                }
            }
        }
    }

    /// GUID 文本 ↔ 磁盘字节序；JSON 转义覆盖引号/反斜杠/控制字符（RFC 8259）
    #[test]
    fn guid_text_and_json_escaping() {
        // 前 3 字段小端落盘：文本 C12A7328-… ↔ 内核 EFI_GUID 展开的字节
        let g = parse_guid("C12A7328-F81F-11D2-BA4B-00A0C93EC93B").unwrap();
        assert_eq!(g, table::ESP_TYPE_GUID);
        assert_eq!(parse_guid("c12a7328f81f11d2ba4b00a0c93ec93b").unwrap(), g);
        assert_eq!(hex_guid(&g), "C12A7328-F81F-11D2-BA4B-00A0C93EC93B");
        assert_eq!(hex_guid(&parse_guid(&hex_guid(&g)).unwrap()), hex_guid(&g));
        for bad in ["", "C12A7328", "C12A7328F81F11D2BA4B00A0C93EC93", "C12A7328-F81F-11D2-BA4B-00A0C93EC93BA", "C12A7328-F81F-11D2-BA4B-00A0C93EC93Z", "-12A7328-F81F-11D2-BA4B-00A0C93EC93B", "C12A7328F81F-11D2-BA4B-00A0C93EC93B"] {
            assert!(parse_guid(bad).is_none(), "{bad:?} must be rejected");
        }
        assert_eq!(json_escape(r#"a"b\c"#), r#"a\"b\\c"#);
        assert_eq!(json_escape("a\nb\tc\rd"), r"a\nb\tc\rd");
        assert_eq!(json_escape("\u{1}\u{1f}"), r"\u0001\u001f");
        // 非 ASCII 与 0x20 以上控制类字符原样保留（JSON 允许直接出现）
        assert_eq!(json_escape("卷标 é"), "卷标 é");
    }
}