//! diskedit — 磁盘编辑工具：镜像文件与块设备统一为按偏移读写的字节存储。
//! 命令面与退出码契约：0=完成（经读回复核，且内核视图已刷新）/10=拒绝执行（未写盘，
//! 成因在请求与现状不匹配）/20=部分完成（后置步骤未做，或表已写但内核分区视图过期）/
//! 30=基础设施失败（盘内容/环境故障且未写盘，或写盘后失败）。
//! 20 的两个成因正交：见 outcome::Applied 的 pending 与 kernel_sync；
//! 30 的两个成因也正交：见 outcome::{Outcome::Infra, Outcome::Failed}

mod dev;
mod fsid;
mod fsops;
mod gpt_policy;
#[cfg(target_os = "linux")]
mod lvm;
mod outcome;
#[cfg(target_os = "linux")]
mod online;
mod movepart;
mod table;

#[cfg(target_os = "linux")]
use std::os::unix::fs::FileTypeExt;
use dev::{FileSource, Journal, JournalRead};
use std::process::ExitCode;

// 退出码的唯一定义在 outcome 模块（与 Outcome→退出码 的映射同处），此处仅重导出，
// 避免两套常量各自漂移
pub(crate) use crate::outcome::{EXIT_INFRA, EXIT_OK, EXIT_PARTIAL, EXIT_REFUSED};

fn usage() -> ! {
    eprintln!(
        r#"diskedit — disk / image editor

  info <TARGET>                                  show partition table / FS / LVM layout
  resize <TARGET>:N <SIZE> [OPTS]                resize partition + FS (auto online/offline)
  move <TARGET>:N --start <LBA|end>              move partition
  copy <TARGET>:N --start <LBA|end> [--name S]   copy partition
  create <TARGET> [--size S] [--name S] [--fs F] create partition in free space
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
common opts: --no-fs  change the partition only, skip filesystem steps (fs grow /
                      swap rebuild / lvm chain become out of scope, so layout
                      success alone is exit 0). Shrinking is refused together
                      with --no-fs: the filesystem has to be shrunk first
exit codes:
  0   done             layout changed and every follow-up step completed (or none
                       was required — e.g. no fs inside, or --no-fs given), and the
                       kernel partition view was refreshed
  10  refused          nothing was written, and the request does not match the
                       target's current state: validation failed, no partition
                       table, partition missing, a required tool is missing, or
                       the confirmation flag absent — changing arguments may help
  20  partial          layout was written but a follow-up step is pending (remedy
                       command printed per affected partition), or the kernel
                       partition view is stale (run partprobe/partx before use)
  30  infrastructure   could not complete, for one of two reasons: the environment
                       or the on-disk data itself is at fault (I/O error, malformed
                       partition table) and nothing was written; or the failure
                       happened after writing, in which case the message says
                       on-disk state may have changed — verify with `info` before
                       retrying"#
    );
    std::process::exit(EXIT_REFUSED as i32);
}

/// 单命令详助（diskedit help <CMD> / <CMD> --help）。未知主题 → 顶层 usage
fn help_cmd(name: &str) -> ! {
    let text: &str = match name {
        "info" => r#"diskedit info <TARGET> [--sector-size N]

  Show partition table, per-partition filesystem identification and LVM
  layout. Read-only. --sector-size N overrides the 512B default (raw images)."#,
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

  Notes: PV shrink is refused (use the lvreduce/pvresize chain). --no-fs skips
  the filesystem steps, so it cannot shrink: the FS has to be shrunk first."#,
        "move" => r#"diskedit move <TARGET>:N --start <LBA|end>

  Move a partition, data follows (chunked copy, resumable via checkpoint).

  LOCATION:
    <LBA>     new start LBA (aligned per --align, default 1MiB)
    end       tail-pack to the last possible position"#,
        "copy" => r#"diskedit copy <TARGET>:N --start <LBA|end> [--name S]

  Byte-wise copy a partition to a new location; the source is untouched.
  --start end packs the copy against the end of the usable range."#,
        "create" => r#"diskedit create <TARGET> [--size SIZE] [--name S] [--fs F]

  Create a partition in free space. With --size, the first aligned gap that
  fits; without, the largest aligned gap. --fs formats after creation.

  SIZE: absolute size (units b/k/m/g/t, 1024 base, e.g. 32M | 2G | bytes)"#,
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
  shrinks); BYTES is the absolute target (units b/k/m/g/t, 1024 base)."#,
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

/// 规划/执行失败的出口：报告文字与退出码都取自 outcome（唯一措辞与唯一映射），
/// 调用点不得自行拼装
fn bail_fail(f: crate::outcome::Fail) -> ! {
    let o = crate::outcome::finish(Err(f), Vec::new());
    o.report();
    std::process::exit(o.exit_code() as i32);
}

/// 参数缺值/坏值的统一拒绝出口：报出旗标名，避免静默 exit 使用户无从排查
fn miss_arg(flag: &str) -> ! {
    eprintln!("refused: {flag} requires a value (see diskedit help)");
    std::process::exit(EXIT_REFUSED as i32);
}

fn bad_arg(flag: &str, v: &str, hint: &str) -> ! {
    eprintln!("refused: bad value {v:?} for {flag}{hint}");
    std::process::exit(EXIT_REFUSED as i32);
}

struct Args {
    target: String,
    part: Option<u32>,
    fstype: Option<String>,
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
    /// --no-fs：只改分区布局，FS 扩展不属后置条件（布局成功即 exit 0）
    no_fs: bool,
    grow_lv: bool,
    lv: Option<String>,
    start_end: bool,
    pos: Vec<String>,
}

fn parse_args() -> (String, Args) {
    let mut it = std::env::args().skip(1);
    let cmd = it.next().unwrap_or_else(|| usage());
    let mut a = Args {
        target: String::new(), part: None, fstype: None, grow: None,
        start: None, end: None, size: None, fs: None, name: None, type_guid: None, table: None,
        yes: false, online: false, no_fs: false, sector_size: None,
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
                let v = it.next().unwrap_or_else(|| miss_arg("--sector-size"));
                a.sector_size = Some(v.parse().unwrap_or_else(|_| bad_arg("--sector-size", &v, " (bytes, e.g. 4096)")));
            }
            "--grow" => {
                let v = it.next().unwrap_or_else(|| miss_arg("--grow"));
                a.grow = Some(v.parse().unwrap_or_else(|_| bad_arg("--grow", &v, " (partition number, e.g. 1)")));
            }
            "--size" => {
                let v = it.next().unwrap_or_else(|| miss_arg("--size"));
                // 与 resize 的 SIZE 同一单位语法（b/k/m/g/t，1024 进制），但只接受绝对值
                a.size = Some(match parse_size_delta(&v) {
                    Some((bytes, 0, false)) => bytes,
                    Some(_) => bad_arg("--size", &v, " (absolute size only: use 32M, not +32M/-32M/32%)"),
                    None => bad_arg("--size", &v, " (use 10G | 500M | plain bytes, e.g. 33554432)"),
                });
            }
            "--fs" => a.fs = Some(it.next().unwrap_or_else(|| miss_arg("--fs"))),
            "--start" => {
                let v = it.next().unwrap_or_else(|| miss_arg("--start"));
                if v.eq_ignore_ascii_case("end") {
                    a.start_end = true; // 尾部打包：挪到 last_usable 内最后位置
                } else {
                    a.start = Some(v.parse().unwrap_or_else(|_| bad_arg("--start", &v, " (LBA, e.g. 2048)")));
                }
            }
            "--end" => {
                let v = it.next().unwrap_or_else(|| miss_arg("--end"));
                a.end = Some(v.parse().unwrap_or_else(|_| bad_arg("--end", &v, " (LBA, e.g. 67583)")));
            }
            "--align" => a.align = it.next().unwrap_or_else(|| miss_arg("--align")),
            "--chunk-size" => {
                let v = it.next().unwrap_or_else(|| miss_arg("--chunk-size"));
                a.chunk_mib = v.parse().unwrap_or_else(|_| bad_arg("--chunk-size", &v, " (MiB, e.g. 4)"));
            }
            "--grow-to-end" => a.grow_to_end = true,
            "--allow-move" => a.allow_move = true,
            "--no-fs" => a.no_fs = true,
            "--grow-lv" => a.grow_lv = true,
            "--lv" => a.lv = Some(it.next().unwrap_or_else(|| miss_arg("--lv"))),
            "--name" => a.name = Some(it.next().unwrap_or_else(|| miss_arg("--name"))),
            "--type" => a.type_guid = Some(it.next().unwrap_or_else(|| miss_arg("--type"))),
            "--table" => a.table = Some(it.next().unwrap_or_else(|| miss_arg("--table"))),
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
    let (target, part) =
        dev::parse_target(&target).unwrap_or_else(|e| bail(EXIT_REFUSED, format!("refused: {e}")));
    a.target = target;
    a.part = part;
    a.pos = positional.clone();
    a.fstype = positional.get(1).cloned();
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
            best_effort_log_write(f, &format!("[{ts}] {msg}"));
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

/// 尽力创建目录：失败不在此处报错——真正的失败会在随后打开文件时以更具体的
/// 错误（完整路径 + 原因）暴露，比这里笼统的 EACCES 更有诊断价值
#[allow(clippy::let_underscore_must_use)] // 有意忽略：失败在打开文件时以更具体错误暴露
fn best_effort_mkdir(dir: &std::path::Path) {
    let _ = std::fs::create_dir_all(dir);
}

/// 尽力写日志行：诊断设施失败不改变业务结论（数据与布局不受影响），故忽略。
/// 命名表达"可失败且无副作用"的意图，便于静态审计区分"有意忽略"与"忘了处理"
#[allow(clippy::let_underscore_must_use)] // 有意忽略：诊断设施失败不改变业务结论
fn best_effort_log_write(f: &mut std::fs::File, line: &str) {
    use std::io::Write;
    let _ = writeln!(f, "{line}");
}

/// undo journal 路径的唯一推导：镜像 = `<名>.diskedit.journal`；
/// 块设备 = /var/lib/diskedit/<devname>.diskedit.journal。
/// 块设备用 devname 而非 disk_guid：`new` 前后均可用，代价是设备名漂移时需手动定位 journal
fn journal_path(target: &std::path::Path, is_block: bool) -> std::path::PathBuf {
    if is_block {
        let dir = std::path::Path::new("/var/lib/diskedit");
        best_effort_mkdir(dir);
        let name = target.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "dev".into());
        dir.join(format!("{name}.diskedit.journal"))
    } else {
        let mut p = target.to_path_buf().into_os_string();
        p.push(".diskedit.journal");
        std::path::PathBuf::from(p)
    }
}

/// 破坏性命令的打开方式：附带 undo journal（镜像/块设备一致）
fn open_target_for_write(a: &Args) -> Result<FileSource, (u8, String)> {
    let mut src = open_target(a)?;
    let p = journal_path(&src.path, src.is_block);
    src.journal = Some(Journal::create(&p).map_err(|e| (EXIT_INFRA, format!("journal open failed: {e}")))?);
    Ok(src)
}

/// 表类写命令成功后通知内核重读分区表（BLKRRPART）。
/// 返回 false = 内核视图未跟进：盘上表已写成功 ≠ 内核 partition view 已同步，
/// 后续不得依赖旧的内核几何（实际状态以 /sys/block/<dev>/<part>/size 为准）。
/// 只探测不打印——"内核视图过期"的措辞由 Outcome::report 一处输出
#[cfg(target_os = "linux")]
fn kernel_resync(src: &FileSource) -> bool {
    if !src.is_block {
        return true;
    }
    use std::os::fd::AsRawFd;
    const BLKRRPART: u64 = 0x125F; // _IO(0x12, 95)（include/uapi/linux/fs.h）
    // SAFETY: fd 来自已打开且存活的 FileSource；BLKRRPART 无用户参数、内核不写回内存
    let r = unsafe { libc::ioctl(src.file.as_raw_fd() as libc::c_int, BLKRRPART as libc::Ioctl, 0u32) };
    r >= 0
}

#[cfg(not(target_os = "linux"))]
fn kernel_resync(src: &FileSource) -> bool {
    let _ = src;
    true
}

/// 布局类命令的统一收尾：内核重读分区表 → 把内核视图状态记入结果 → 报告 → 返回结果。
/// 顺序不可换：report 必须在 resync 之后，否则打不出"内核视图过期"这一项；
/// 未写盘的命令不做重读（没东西要同步）
fn settle_layout(mut o: crate::outcome::Outcome, src: &FileSource) -> crate::outcome::Outcome {
    if o.is_applied() && !kernel_resync(src) {
        o.mark_kernel_stale();
    }
    o.report();
    o
}

/// 表类写命令的成功收尾：内核重读 + 报告，仅当退出码为 0 时才打印成功字样
/// （内核视图过期时退 20，此时打"成功"会与退出码矛盾）
fn table_write_done(src: &FileSource, ok_msg: &str) -> u8 {
    let o = settle_layout(crate::outcome::Outcome::applied_with(Vec::new()), src);
    if o.exit_code() == EXIT_OK {
        println!("{ok_msg}");
    }
    o.exit_code()
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

/// 目标分区的**字节区间**（`:N` 命中核验，off-by-one 防线）。返回 (起始字节, 长度字节)。
/// 返回字节而非 LBA 是刻意的：LBA 的单位取决于它来自哪张表——GPT 条目以**表自身的** ss 计
/// （4Kn 镜像未加 --sector-size 时 `g.ss != src.sector_size`，按容器 ss 换算会整体错位），
/// MBR 条目以容器 ss 计。换算在读到表的一处完成，下游（fsid::identify 按字节区间工作）不必知道
/// 单位是谁的
/// 返回 Fail 而不是 (码, 文案)：前缀与码必须同源——否则调用点会各自拼 "refused: " 前缀，
/// 碰上 30 就自相矛盾（mkfs 曾打出 "refused: parse failed: ..." 却是 30）。
/// 按标签分派：GPT 与 MBR 的条目形状不同（MBR 只有主分区槽位 1..=4）
fn entry_byte_range(src: &FileSource, part: u32) -> Result<(u64, u64), crate::outcome::Fail> {
    // 无表 = 请求与目标现状不匹配(10)；表在但结构非法 = 盘内容故障(30)。
    // 与 resize/info 的 parse failed / no partition table 同一判据
    match table::load_gpt(src) {
        Err(e) => Err(crate::outcome::Fail::infra(format!("parse failed: {e}"))),
        Ok(Some(g)) => {
            let e = g.entries.get((part - 1) as usize)
                .ok_or_else(|| crate::outcome::Fail::refused(format!("partition {part} not found")))?;
            if e.ending_lba == 0 {
                return Err(crate::outcome::Fail::refused(format!("partition {part} is empty")));
            }
            Ok((e.starting_lba * g.ss, (e.ending_lba - e.starting_lba + 1) * g.ss))
        }
        // 无 GPT → 按 MBR 解析。不这么做的话真 MBR 盘在这里被一律当成"无表"，
        // mkfs / set label|uuid 在 MBR 上完全不可用
        Ok(None) => match table::parse_mbr(src).map_err(|e| crate::outcome::Fail::infra(format!("parse failed: {e}")))? {
            None => Err(crate::outcome::Fail::refused("no partition table on target")),
            Some(mbr) => {
                let p = mbr.iter().find(|p| p.num == part).ok_or_else(|| {
                    crate::outcome::Fail::refused(format!("partition {part} not found (MBR covers primary slots 1..=4)"))
                })?;
                // 扩展容器是逻辑分区的壳，不是可承载文件系统的分区
                if p.is_container {
                    return Err(crate::outcome::Fail::refused(format!(
                        "partition {part} is an extended container (logical partitions are out of scope)"
                    )));
                }
                Ok((p.start_lba as u64 * src.sector_size, p.size_lba as u64 * src.sector_size))
            }
        },
    }
}

/// 只读命令的打开：块设备只读（RW+O_EXCL 在盘被 claim 时会被内核拒绝，分区被占用会连同
/// 整盘一起被 claim），镜像文件照常。info 与 plan 共用——两者都不写盘，却都可能被用来
/// 查看一块正被使用的盘（挂载中、有活动分区），此时 O_EXCL 会让它们连读都读不成
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
        // 解析层的几何自洽性失败必须显式报错，不能静默降级成 "none"。
        // 结构化变体在此分类输出（细节仍由 GptError 的 Display 单一渲染）
        Err(e) => {
            let kind = match &e {
                table::GptError::InvalidEntry { .. } | table::GptError::BeyondUsable { .. } => "invalid GPT entries",
                table::GptError::BeyondContainer { .. } => "GPT geometry beyond container",
                // 与 InvalidHeader 分开：这是数组的数据损伤（CRC 层面的坏）。
                // 走到这里说明两份副本都没给出可用的表——单份损伤会在 load_gpt 里被备份救回
                table::GptError::EntryArrayCorrupt { .. } => "GPT entry array damaged (both copies unusable)",
                table::GptError::InvalidHeader(_) => "invalid GPT header",
                table::GptError::Io(_) => "I/O error",
            };
            eprintln!("parse failed ({kind}): {e}");
            return EXIT_INFRA as i32;
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
                return EXIT_INFRA as i32;
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

/// 计算用几何：表属"可修复的 stale"（设备扩容后备份头/PMBR 未更新，见 GptState::NeedsRepair）时
/// 按修复后的 last_usable_lba 计算；plan 不写盘，实际修复由写入路径的 ensure_geometry 完成。
/// 返回 `Fail` 而不是字符串：这里的失败全部是盘/容器自身不自洽（PMBR 越出容器、容器装不下
/// 备份数组、分区越出修复后的可用区），属"未写盘的盘内容故障"，与调用点自己那一堆校验拒绝
/// 是两回事，不能压成同一个码
fn effective_last_usable(src: &FileSource, g: &table::RawGpt) -> Result<u64, crate::outcome::Fail> {
    let file_last = src.size / g.ss - 1;
    match gpt_policy::classify_repair(g, file_last) {
        // 动作自带修复后的 last_usable（决策期已算好），无需调用点再推导一次
        Ok(action) => Ok(action.new_last_usable().unwrap_or(g.header.last_usable_lba)),
        Err(e) => Err(crate::outcome::Fail::infra(e.to_string())),
    }
}

/// 目标分区右侧连续空闲扇区数（到下一分区起点或可用区上界为止，GPT）。
/// 上界是显式入参而非就地取 `g.header.last_usable_lba`：设备扩容后表头里的该字段是过期值，
/// 按它算会把整段新增空间误判成"不可用"（有效上界见 effective_last_usable）
fn free_right_gpt(g: &table::RawGpt, part: u32, last_usable: u64) -> u64 {
    let e = &g.entries[(part - 1) as usize];
    let mut bound = last_usable + 1; // 排他上界
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

/// 列出各分区的搬移（plan 命令与 apply 前的计划打印共用）。
/// 头行不共用：两处要给出的数不同——写入前只需扩容终点，`plan` 还要额外给出
/// 尾部打包的上界 last_usable_lba
fn print_moves(plan: &movepart::Plan) {
    for m in &plan.moves {
        let tag = if m.is_swap { " [swap: recreate, no data move]" } else { "" };
        println!("move part {} : {}..{} → +{} sectors ({} bytes){}",
            m.part_num, m.first_lba, m.first_lba + m.len_lba - 1, m.delta_lba, m.delta_lba * plan.ss, tag);
    }
}

fn print_plan(plan: &movepart::Plan) -> std::io::Result<()> {
    // 实际扩容终点由 grow_end_for 判定（与 apply 同一实现）
    let new_end = movepart::grow_end_for(plan)?;
    println!("plan: grow partition {} → end LBA {} (blockers relocated)", plan.grow_part, new_end);
    print_moves(plan);
    Ok(())
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

/// resize 请求里**与表类型无关**的部分：写盘前的事实快照 + 目标尺寸。
/// 表类型特有的差异收敛成一件事实——右侧连续空闲（GPT 按修复后的 last_usable，
/// MBR 按 32 位上限与后继条目），故由各自的几何函数算好 `free_right_lba` 填入。
/// 这样在线路径与 `check_pv_intent` 只写一份，不会各自演化
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
        .unwrap_or_else(|| bail(EXIT_REFUSED, format!("cannot derive disk name from {}", a.target)));
    refuse_swap_active(&dn, t.part);

    // PV：分区层必须先按新尺寸出现在内核里，pvresize 才能吸收；活跃 LV 经 dm 持有分区使
    // BLKRRPART 返回 EBUSY，故走 sfdisk+partx 同步路径
    if t.is_pv {
        let new_len = if t.grow_to_end {
            if t.free_right_lba == 0 {
                bail(EXIT_REFUSED, "refused: no free space to the right — a PV cannot relocate blocking partitions while LVs may be active".to_string());
            }
            t.cur_bytes + t.free_right_lba * t.ss
        } else {
            t.target.unwrap_or(t.cur_bytes) / t.ss * t.ss // 扇区下取整，与离线路径同规则
        };
        if new_len != t.cur_bytes {
            let o = online::resize_pv_online(&dn, t.part, new_len);
            if o.exit_code() != EXIT_OK {
                o.report();
                return Some(o.exit_code());
            }
        }
        return Some(resize_done(a, true, true, t.cur_bytes));
    }

    // 非 PV：仅挂载中的分区能在线扩（在线不能搬移，只吃连续空闲）
    let mnt = online::find_mountpoint(&dn, t.part)?;
    // 无右侧空闲 = 分区已吃满 → None 让 FS 工具扩满现分区
    let size = if t.grow_to_end {
        let free = t.free_right_lba * t.ss;
        (t.cur_bytes + free != t.cur_bytes).then_some(t.cur_bytes + free)
    } else {
        t.target
    };
    let o = online::resize_online(&mnt, size);
    if o.exit_code() == EXIT_OK {
        println!("resized online (verify with: diskedit info {})", a.target);
    } else {
        o.report();
    }
    Some(o.exit_code())
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
    // resize 只改大小、不移动。--start 属 move/resize-part 的语义，静默忽略会让用户
    // 误以为分区被移动过 —— 显式拒绝
    if a.start.is_some() || a.start_end {
        bail(EXIT_REFUSED, "refused: resize does not relocate partitions — use `move` or `resize-part --start`".to_string());
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
    // 有效几何：设备扩容后表头里的 last_usable_lba 可能仍是旧值，"右侧还剩多少空间"
    // 一律按修复后的上界算，否则新增的整段空间会被当成不可用
    let last_usable = effective_last_usable(&src, &g).unwrap_or_else(|f| bail_fail(f));
    // 上一轮 plan 型搬移作业是否尚未收尾（右侧"已空"可能正是搬了一半的结果）
    let resuming = movepart::has_pending_relocation(&src, part);
    let cur_bytes = (end - start + 1) * ss;
    let fstype = fsid::identify(&src, start * ss, (end - start + 1) * ss).unwrap_or_else(|e| bail(EXIT_INFRA, format!("identify failed: {e}")));
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
        free_right_lba: free_right_gpt(&g, part, last_usable),
        target,
        grow_to_end,
    }) {
        return code;
    }

    // 离线路径
    let is_block = src.is_block;
    let mut src = open_target_for_write(a).unwrap_or_else(|(c, m)| bail(c, m));
    if grow_to_end {
        let free = free_right_gpt(&g, part, last_usable);
        // 右侧有空闲且没有未收尾的搬移作业 → 纯扩容。若作业未收尾，则"右侧已空"很可能
        // 正是搬了一半的结果，走普通 resize_part 会跳过剩余搬移与 swap 重建等收尾
        if free > 0 && !resuming {
            let (chunk, mut logger) = chunk_logger(a, &src);
            let o = settle_layout(movepart::resize_part(&mut src, part, start, end + free, chunk, a.no_fs, &mut |m| logger.log(m)), &src);
            return finish_resize(a, o, is_pv, is_block, cur_bytes);
        }
        // 右侧被挡：自动搬移挡路分区（plan 打印 → --allow-move 放行 → --yes 确认）
        if !a.allow_move {
            bail(EXIT_REFUSED, "refused: right side is occupied — pass --allow-move to relocate the blocking partitions (plan will be printed; --yes confirms)".to_string());
        }
        let plan = match movepart::make_plan_resuming(&mut src, part) {
            Ok(p) => p,
            Err(f) => bail_fail(f),
        };
        print_plan(&plan).unwrap_or_else(|e| bail(EXIT_REFUSED, format!("plan failed: {e}")));
        if !a.yes {
            eprintln!("refused: this resizes by relocating the partitions listed above — review and re-run with --yes");
            return EXIT_REFUSED;
        }
        let (chunk, mut logger) = chunk_logger(a, &src);
        let o = settle_layout(movepart::apply(&mut src, &plan, chunk, a.no_fs, &mut |m| logger.log(m)), &src);
        finish_resize(a, o, is_pv, is_block, cur_bytes)
    } else {
        // SIZE：字节 → 扇区（下取整）；扩须右侧空闲足够，缩由 resize_part 内部 FS 先缩 + 守卫
        let Some(bytes) = target else { usage() };
        if bytes < ss {
            bail(EXIT_REFUSED, format!("refused: size {bytes} < one sector ({ss})"));
        }
        let new_end = start + bytes / ss - 1;
        if new_end > last_usable {
            bail(EXIT_REFUSED, format!("refused: size {bytes} exceeds usable range (partition would end past last_usable_lba {last_usable})"));
        }
        // 扩容需要的位移量只在扩的时候有定义：缩容的新末端更靠左，右侧只会更空，
        // 不存在搬移需求（此时 new_end < end，直接相减会回绕）
        let shift = (new_end > end).then(|| new_end - end);
        // 未收尾的搬移作业 ⇒ 必须走 resume 路径（即使几何上 free_right 已足够——swap 等
        // 收尾步骤可能尚未执行，普通扩容会跳过它们）
        if resuming || shift.is_some_and(|s| s > free_right_gpt(&g, part, last_usable)) {
            // 右侧连续空闲不足：--allow-move 时按最小位移搬移挡路分区，
            // 与 grow 路径同一确认流（plan 打印 → --yes 确认）
            if !a.allow_move {
                bail(EXIT_REFUSED, "refused: not enough contiguous free space to the right — pass --allow-move to relocate the blocking partitions (plan will be printed; --yes confirms)".to_string());
            }
            let plan = match movepart::make_plan_shift_resuming(&mut src, part, shift) {
                Ok(p) => p,
                Err(f) => bail_fail(f),
            };
            print_plan(&plan).unwrap_or_else(|e| bail(EXIT_REFUSED, format!("plan failed: {e}")));
            if !a.yes {
                eprintln!("refused: this resizes by relocating the partitions listed above — review and re-run with --yes");
                return EXIT_REFUSED;
            }
            let (chunk, mut logger) = chunk_logger(a, &src);
            let o = settle_layout(movepart::apply(&mut src, &plan, chunk, a.no_fs, &mut |m| logger.log(m)), &src);
            return finish_resize(a, o, is_pv, is_block, cur_bytes);
        }
        let (chunk, mut logger) = chunk_logger(a, &src);
        let o = settle_layout(movepart::resize_part(&mut src, part, start, new_end, chunk, a.no_fs, &mut |m| logger.log(m)), &src);
        finish_resize(a, o, is_pv, is_block, cur_bytes)
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
    let fstype = fsid::identify(src_ro, 0, cur_bytes)
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
            "unknown" | "lvm2_pv" => {}
            // swap：内容可弃，表项已扩 → mkswap 重建使新空间生效（UUID/卷标保持；
            // 离线路径仅镜像，块设备走在线路径且 active swap 已被守卫拒绝）
            "swap" => {
                let ident = movepart::read_swap_identity(src, p.start_lba as u64, p.size_lba as u64, ss);
                if let Err(e) = fsops::recreate_swap(src, part, ident) {
                    pending.push(crate::outcome::Pending::new(
                        part,
                        crate::outcome::PendingKind::Swap,
                        e.to_string(),
                        fsops::rescue_hint("swap", &dev::part_dev_hint(src, part, p.start_lba as u64 * ss), false),
                    ));
                }
            }
            _ => {
                if let Err(e) = fsops::resize_fs(src, part, fstype) {
                    pending.push(crate::outcome::Pending::new(
                        part,
                        crate::outcome::PendingKind::Fs,
                        e.to_string(),
                        fsops::rescue_hint(fstype, &dev::part_dev_hint(src, part, p.start_lba as u64 * ss), false),
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
    let fstype = fsid::identify(src_ro, p.start_lba as u64 * ss, p.size_lba as u64 * ss)
        .unwrap_or_else(|e| bail(EXIT_INFRA, format!("identify failed: {e}")));
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
        free_right_lba: free_right_msdos(&mbr, p, total_sectors),
        target,
        grow_to_end,
    }) {
        return code;
    }

    // 离线路径
    let mut src = open_target_for_write(a).unwrap_or_else(|(c, m)| bail(c, m));
    if grow_to_end {
        let free = free_right_msdos(&mbr, p, total_sectors);
        let mut table_written = false;
        if free > 0 {
            let new_size_lba = p.size_lba as u64 + free;
            if new_size_lba > u32::MAX as u64 {
                bail(EXIT_REFUSED, format!("refused: new size {new_size_lba} sectors exceeds MBR 32-bit LBA limit"));
            }
            table::resize_mdos_entry(&mut src, part, new_size_lba as u32)
                .unwrap_or_else(|f| bail_fail(f));
            table_written = true;
        }
        // free == 0：分区已吃满右侧，表不动，FS 工具直接扩满现分区（与 GPT 路径同语义）
        mbr_grow_finish(a, &src, p, fstype, table_written, is_pv, is_block)
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
                bail(EXIT_REFUSED, "refused: --no-fs cannot shrink: the filesystem has to be shrunk first, otherwise the new partition end would cut into filesystem metadata".to_string());
            }
            // FS 收缩的前置检查与 GPT 路径同一处（fsops::check_shrink）：能否缩 / 类型是否
            // 认得 / 工具是否齐备只写一份
            fsops::check_shrink(fstype)
                .unwrap_or_else(|e| bail(EXIT_REFUSED, format!("refused: {e}")));
            if let Some(min) = fsops::fs_min_bytes(&src, part, fstype)
                .unwrap_or_else(|e| bail(EXIT_INFRA, format!("min-size probe failed: {e}")))
                && bytes < min
            {
                bail(EXIT_REFUSED, format!("refused: target size {bytes} < minimum FS size {min} bytes (resize2fs -P)"));
            }
            fsops::shrink_fs(&src, part, fstype, new_size_lba * ss)
                .unwrap_or_else(|e| bail(EXIT_INFRA, format!("FS shrink failed: {e}")));
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
    if o.exit_code() != EXIT_OK && !is_pv {
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
        // 只打开一次：下面读"实际新尺寸"与给 LVM 链取分区节点用的是同一份盘上现状
        let src = open_target_ro(a).unwrap_or_else(|(c, m)| bail(c, m));
        // 表项重读按 label 分派（MBR resize 也走本收尾）
        let new_bytes = if let Ok(Some(g)) = table::load_gpt(&src) {
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
        };
        let delta = new_bytes.saturating_sub(old_bytes);
        let part = a.part.unwrap_or(0);
        let r = if is_block {
            lvm_grow_chain(&part_dev_path(&a.target, part), delta, a.grow_lv, a.lv.as_deref())
        } else {
            // offset+sizelimit 映射出的 loop 设备 = 该分区的整块设备，PV 整设备语义下
            // pvresize/lvextend 直接可用，无需 -P partscan
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
        effective_last_usable(&src, &g).unwrap_or_else(|f| bail_fail(f))
            .checked_sub(len - 1)
            .unwrap_or_else(|| bail(EXIT_REFUSED, "refused: partition longer than usable range".to_string()))
    } else {
        align_start(a, start_opt.unwrap(), g.ss)
    };
    // checked：start 来自 CLI 原始输入（--align none 时无上界），回绕会骗过 resize_part 的边界校验
    let end = start.checked_add(len - 1)
        .unwrap_or_else(|| bail(EXIT_REFUSED, "refused: end LBA overflows address space".to_string()));
    let (chunk, mut logger) = chunk_logger(a, &src);
    let o = settle_layout(movepart::resize_part(&mut src, part, start, end, chunk, a.no_fs, &mut |m| logger.log(m)), &src);
    if o.exit_code() == EXIT_OK {
        println!("moved (verify with: diskedit info {})", a.target);
    }
    o.exit_code()
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
    // 一次探测同时取"表类型 + 空闲区"：后面选槽写表要用的是同一个 label，
    // 再探一次等于重解析一遍表（且可能读到与前面不同的结果）
    let (label, gaps) = match table::table_label(&src) {
        Ok("gpt") => {
            let g = match table::load_gpt(&src) {
                Ok(Some(g)) => g,
                Ok(None) => bail(EXIT_REFUSED, "refused: no GPT on target — run `new` first".to_string()),
                Err(e) => bail(EXIT_INFRA, format!("parse failed: {e}")),
            };
            let used: Vec<(u64, u64)> = g.entries.iter()
                .filter(|e| !(e.starting_lba == 0 && e.ending_lba == 0))
                .map(|e| (e.starting_lba, e.ending_lba)).collect();
            let last_usable = effective_last_usable(&src, &g).unwrap_or_else(|f| bail_fail(f));
            ("gpt", aligned_gaps(&used, g.header.first_usable_lba, last_usable, unit))
        }
        Ok("msdos") => {
            let mbr = match table::parse_mbr(&src) {
                Ok(Some(m)) => m,
                Ok(None) => bail(EXIT_REFUSED, "refused: no partition table on target — run `new` first".to_string()),
                Err(e) => bail(EXIT_INFRA, format!("parse failed: {e}")),
            };
            let used: Vec<(u64, u64)> = mbr.iter().map(|p| (p.start_lba as u64, p.start_lba as u64 + p.size_lba as u64 - 1)).collect();
            let disk_last = src.size / ss - 1;
            ("msdos", aligned_gaps(&used, unit, disk_last, unit))
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
    // swap 声明落进类型 GUID：movepart 靠它识别 swap 挡路者（不搬数据、mkswap 重建）
    let is_swap = a.fs.as_deref() == Some("swap");
    let r = if label == "gpt" {
        let guid = if is_swap { table::SWAP_TYPE_GUID } else { table::LINUX_FS_TYPE_GUID };
        table::add_entry(&mut src, start, end, a.name.as_deref().unwrap_or(""), guid)
    } else {
        table::add_mdos_entry(&mut src, start, end, if is_swap { 0x82 } else { 0x83 })
    };
    let num = match r {
        Ok(n) => n,
        // 表写入失败的性质由 table 层判定：事前校验拒绝(10) / 写盘后失败(30，
        // commit_gpt 是四段提交，失败时盘上可能停在中间态)。调用点无从区分
        Err(f) => bail_fail(f),
    };
    let mut o = crate::outcome::Outcome::applied_with(Vec::new());
    if !kernel_resync(&src) {
        o.mark_kernel_stale();
    }
    if let Some(fstype) = &a.fs
        && let Err(e) = fsops::mkfs(&src, num, fstype)
    {
        eprintln!("partition #{num} created but mkfs failed: {e}");
        o.report(); // 表已写（可能内核未同步）须一并报告
        return EXIT_PARTIAL;
    }
    o.report();
    if o.exit_code() == EXIT_OK {
        println!("created partition #{num} at {start}..{end} (verify with: diskedit info {})", a.target);
    }
    o.exit_code()
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
            Ok(()) => table_write_done(&src, &format!("renamed partition #{part} to {value:?}")),
            Err(f) => bail_fail(f),
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
        "mkfs" => {
            let (Some(part), Some(fstype)) = (a.part, a.fstype.clone()) else { usage() };
            if !a.yes {
                eprintln!("refused: mkfs destroys all data on partition {part}; pass --yes to confirm");
                EXIT_REFUSED
            } else {
                let src = open_target(&a).unwrap_or_else(|(c, m)| bail(c, m));
                if let Err(f) = entry_byte_range(&src, part) {
                    bail_fail(f);
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
                // BYTES 与 --size 同一单位语法，只接受绝对值
                let size = a.fstype.as_ref().map(|s| match parse_size_delta(s) {
                    Some((bytes, 0, false)) => bytes,
                    _ => bail(EXIT_REFUSED, format!("size {s:?} must be an absolute size (use 10G | 500M | plain bytes)")),
                });
                #[cfg(target_os = "linux")]
                {
                    let o = online::resize_online(std::path::Path::new(&a.target), size);
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
                let Some(part) = a.part else { usage() };
                let src = open_target(&a).unwrap_or_else(|(c, m)| bail(c, m));
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
                    Ok(()) => table_write_done(&src, &format!("created {} table (verify with: diskedit info {})", a.table.as_deref().unwrap_or("gpt"), a.target)),
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
                        Ok(num) => table_write_done(&src, &format!("added partition #{num} (verify with: diskedit info {})", a.target)),
                        Err(f) => bail_fail(f),
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
                        Ok(num) => table_write_done(&src, &format!("added partition #{num} (verify with: diskedit info {})", a.target)),
                        Err(f) => bail_fail(f),
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
                    Ok(()) => table_write_done(&src, &format!("deleted partition #{part} (verify with: diskedit info {})", a.target)),
                    Err(f) => bail_fail(f),
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
                    Ok(Some(g)) => effective_last_usable(&src, &g).unwrap_or_else(|f| bail_fail(f)),
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
            let o = settle_layout(movepart::resize_part(&mut src, part, start, end, chunk, a.no_fs, &mut |m| logger.log(m)), &src);
            if o.exit_code() == EXIT_OK {
                println!("resize-part complete (verify with: diskedit info {})", a.target);
            }
            o.exit_code()
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
                let last_usable = effective_last_usable(&src, &g).unwrap_or_else(|f| bail_fail(f));
                last_usable
                    .checked_sub(len - 1)
                    .unwrap_or_else(|| bail(EXIT_REFUSED, "refused: partition longer than usable range".to_string()))
            } else {
                align_start(&a, start_opt.unwrap(), src.sector_size)
            };
            let (chunk, mut logger) = chunk_logger(&a, &src);
            match movepart::copy_part(&mut src, part, start, a.name.as_deref().unwrap_or(""), chunk, &mut |m| logger.log(m)) {
                Ok(num) => table_write_done(&src, &format!("copied to partition #{num} (verify with: diskedit info {})", a.target)),
                Err(f) => bail_fail(f),
            }
        }
        "undo" => {
            if !a.yes {
                eprintln!("refused: `undo` overwrites current bytes from journal; pass --yes to confirm");
                EXIT_REFUSED
            } else {
                let mut src = open_target(&a).unwrap_or_else(|(c, m)| bail(c, m));
                let p = journal_path(&src.path, src.is_block);
                let (entries, tail_incomplete) = match Journal::read_entries(&p) {
                    Ok(JournalRead::Complete(v)) => (v, false),
                    // 尾部未完成的记录：append-only 下那次追加没走完，它对应的写入也就没发生，
                    // 丢弃安全；前面的完整前缀照常回放（严格契约仍守：每条都过了 CRC）
                    Ok(JournalRead::TruncatedTail(v)) => (v, true),
                    Err(e) => bail(EXIT_REFUSED, format!("no usable journal: {e}")),
                };
                if entries.is_empty() {
                    bail(EXIT_REFUSED, "nothing to undo (journal is empty)".to_string());
                }
                let n = entries.len();
                if tail_incomplete {
                    eprintln!(
                        "warning: the journal ends with an incomplete record (an interrupted append, or that record was damaged) — \
                         replaying the {n} complete record(s) before it; anything recorded after that point cannot be undone"
                    );
                }
                // 含搬移的 journal 不可回滚：数据字节按设计不入 journal（前向恢复、无回滚），
                // 只回滚表项会留下表与数据不一致的布局，必须显式拒绝而非给出假回滚
                if entries.iter().any(|(off, _)| *off == Journal::MOVED_MARKER) {
                    bail(EXIT_REFUSED, "refused: journal covers a partition relocation — moved/copied data is not journaled by design, so undo cannot revert it (re-run the original command to resume, or restore from backup)".to_string());
                }
                for (off, data) in entries.iter().rev() {
                    if let Err(e) = src.write_at(*off, data) {
                        // journal 保留在原地：可重试 undo
                        bail(EXIT_INFRA, format!("undo write failed at offset {off}: {e} (journal kept, retry)"));
                    }
                }
                // undo 的契约是"盘确定回到写入前状态"：sync 失败意味着回滚可能未落盘，
                // 不能报成功——那会让用户以为已经回滚
                src.sync_all().unwrap_or_else(|e| {
                    bail(EXIT_INFRA, format!("undo wrote the journal back but sync failed: {e} — rollback may not be durable, verify before retrying"))
                });
                dev::warn_if_remove_failed(&p);
                table_write_done(&src, &format!("undone {n} journal entries (verify with: diskedit info {})", a.target))
            }
        }
        "check" => {
            let Some(part) = a.part else { usage() };
            let src = open_target(&a).unwrap_or_else(|(c, m)| bail(c, m));
            let (start, len) = entry_byte_range(&src, part).unwrap_or_else(|f| bail_fail(f));
            let fstype = fsid::identify(&src, start, len).unwrap_or_else(|e| bail(EXIT_INFRA, format!("identify failed: {e}")));
            match fsops::check_fs(&src, part, fstype) {
                Ok(()) => { println!("check done on partition #{part} ({fstype})"); EXIT_OK }
                Err(e) => { eprintln!("check failed: {e}"); EXIT_INFRA }
            }
        }
        "plan" | "apply" => {
            let Some(grow) = a.grow else { usage() };
            // 这一段只读：plan 不写盘，apply 的写由 apply_cmd 用 open_target_for_write 另开一次。
            // 故按只读命令打开——否则一块正被使用的盘上连 `plan` 都跑不出计划
            let mut src = open_target_ro(&a).unwrap_or_else(|(c, m)| bail(c, m));
            // 恢复感知：盘上有未收尾的搬移作业时，`plan` 要打印、`apply` 要执行的
            // 都是那份 ckpt 里的计划（现算的 delta 与 ckpt 不一致，会撞上恢复校验）
            let plan = match movepart::make_plan_resuming(&mut src, grow) {
                Ok(p) => p,
                Err(f) => bail_fail(f),
            };
            if cmd == "plan" {
                if movepart::has_pending_relocation(&src, plan.grow_part) {
                    println!("[resume] an unfinished relocation job is on the disk — this is the plan it resumes with");
                }
                if let Some(what) = plan.repair.describe() {
                    // 修复动作只记录、不执行；apply 会先做这一步再搬数据
                    println!("[repair] {what}");
                }
                // 终点与实际写入一致（grow_end_for 是 apply 用的同一实现）；
                // last_usable_lba 单独列出：它是尾部打包的上界，不等于本次扩容终点
                let grow_end = movepart::grow_end_for(&plan)
                    .unwrap_or_else(|e| bail(EXIT_REFUSED, format!("plan failed: {e}")));
                println!(
                    "grow partition {} → end LBA {} (last usable {})",
                    plan.grow_part, grow_end, plan.last_usable_lba
                );
                print_moves(&plan);
                EXIT_OK
            } else {
                apply_cmd(&a, plan)
            }
        }
        _ => usage(),
    };
    // 成功完成 ⇒ 撤销窗口已关闭，删除 undo journal（失败/中断时保留，供续传或回滚）。
    // 仅限会创建 journal 的破坏性命令：只读命令成功时若也删，会把先前失败操作
    // 留下的 journal 误清掉
    if code == EXIT_OK && is_destructive_cmd(&cmd) {
        drop_journal(&a);
    }
    ExitCode::from(code)
}

/// 是否属于经 open_target_for_write 写入（因而会创建 undo journal）的命令。
/// mkfs 与 undo 走的是 open_target（不建 journal）：前者只重做 FS 元数据，
/// 后者以 journal 为输入、成功时自行删除——它们若也进这个集合，
/// 成功时会顺手清掉先前某次失败操作留下的 journal。
/// resize 的块设备在线路径（online::resize_online/resize_pv_online）同样不经
/// open_target_for_write，故本判定只是"可能创建"，删除动作须容忍文件不存在
fn is_destructive_cmd(cmd: &str) -> bool {
    matches!(cmd, "new" | "add" | "del" | "delete" | "set" | "resize" | "resize-part" | "move" | "create" | "copy" | "apply")
}

/// 成功路径删除 undo journal（路径推导与 open_target_for_write 同源）。
/// 不存在即无残留、无告警；删除真失败则由 dev::warn_if_remove_failed 告警
fn drop_journal(a: &Args) {
    let p = journal_path(std::path::Path::new(&a.target), is_block_device(&a.target));
    dev::warn_if_remove_failed(&p);
}

/// 目标是否为块设备（决定 journal 落点）
fn is_block_device(path: &str) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileTypeExt;
        std::fs::metadata(path).map(|m| m.file_type().is_block_device()).unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        false
    }
}

fn apply_cmd(a: &Args, plan: movepart::Plan) -> u8 {
    let mut src = match open_target_for_write(a) { Ok(s) => s, Err((c, m)) => { eprintln!("{m}"); return c; } };
    // 表的可解析性由 apply_inner 在写盘前判定（无表 → refused 10，表非法 → infra 30）：
    // 同一事实不设第二判据——两处判据迟早会在某个入口分叉，且自判拒绝时还不报原因
    let chunk = match movepart::chunk_bytes(a.chunk_mib) {
        Ok(c) => c,
        Err(e) => { eprintln!("refused: {e}"); return EXIT_REFUSED; }
    };
    let mut logger = Logger::open(&src);
    let o = movepart::apply(&mut src, &plan, chunk, a.no_fs, &mut |m| logger.log(m));
    // 失败时日志里也留一份：apply 出问题后用户常回看日志
    if let crate::outcome::Outcome::Failed { cause } = &o {
        logger.log(&format!("apply failed: {cause}"));
    }
    settle_layout(o, &src).exit_code()
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
            target: String::new(), part: None, fstype: None, grow: None,
            start: None, end: None, size: None, fs: None, name: None, type_guid: None, table: None,
            yes: false, online: false, no_fs: false, sector_size: None, align: "mib".to_string(), chunk_mib: 4,
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

    /// 分区 LBA 的单位是**表自身**的 ss，不是容器 ss：4Kn 表放在 512B 口径的容器里
    /// （4Kn 镜像未加 --sector-size，或经 512e 转接写入的 4Kn 盘）时，按容器 ss 换算会整体差
    /// 8 倍，于是 info/resize 认到的文件系统与 mkfs/check 认到的区间不是同一段字节。
    /// entry_byte_range 是这处单位换算的唯一出口，故在此守住
    #[test]
    fn part_byte_range_uses_table_sector_size() {
        let data = vec![0u8; 8 * 1024 * 1024];
        let mut src = src_from("ss4k", &data); // 容器 ss = 512
        table::create_gpt(&mut src, 4096, None).unwrap();
        table::add_entry_at(&mut src, 256, 511, "p1", table::LINUX_FS_TYPE_GUID, [0x22; 16]).unwrap();
        // 表以 4096B 逻辑块自述（LBA1 落在 offset 4096），load_gpt 的候选 ss 会选出 4096
        assert_eq!(table::load_gpt(&src).unwrap().unwrap().ss, 4096);
        assert_eq!(entry_byte_range(&src, 1).unwrap(), (256 * 4096, 256 * 4096));
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

    /// 右侧连续空闲：取"下一个分区起点"与"可用区上界+1"的较小者
    #[test]
    fn free_right_bounds() {
        // 分区 1 右侧紧邻分区 2 → 无空闲
        let g = raw_gpt(1000, &[(100, 199), (200, 299), (0, 0)]);
        assert_eq!(free_right_gpt(&g, 1, 1000), 0);
        assert_eq!(free_right_gpt(&g, 2, 1000), 1000 + 1 - 300);
        // 右侧隔着空隙 → 以邻分区起点为界
        let g = raw_gpt(1000, &[(100, 199), (300, 399), (0, 0)]);
        assert_eq!(free_right_gpt(&g, 1, 1000), 300 - 200);
        assert_eq!(free_right_gpt(&g, 2, 1000), 1000 + 1 - 400);
        // 左侧分区不计入（起点小于本分区末端的都被忽略）
        let g = raw_gpt(1000, &[(50, 99), (100, 199), (0, 0)]);
        assert_eq!(free_right_gpt(&g, 2, 1000), 1000 + 1 - 200);
        // 上界是入参而非表头字段：stale 表按修复后的上界算，才看得到扩容新增的空间
        let stale = raw_gpt(500, &[(100, 199), (0, 0), (0, 0)]);
        assert_eq!(free_right_gpt(&stale, 1, 500), 500 + 1 - 200);
        assert_eq!(free_right_gpt(&stale, 1, 1000), 1000 + 1 - 200);
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