//! 参数层：命令行解析、每命令旗标消费契约（fail-closed）与帮助文本入口。

use crate::dev::PartSelector;
use crate::support::{bail_fail, Fail, EXIT_REFUSED};

/// 顶层用法文本。错误用法打到 stderr 退 10（refused：请求无效、未写盘）；
/// `help` / `--help` / `-h` 这类**主动请求帮助**打到 stdout 退 0（请求本身有效，
/// 按 GNU 惯例帮助出口 0）
const USAGE_TEXT: &str = r#"diskedit — disk / image editor

usage:
  diskedit <COMMAND> [ARGS]                details for one command: diskedit help <CMD>
  diskedit --version

examples:
  diskedit info /dev/sdb
  diskedit resize /dev/sdb:1 +2G
  diskedit resize disk.img grow --allow-move --yes
  diskedit undo /dev/sdb --yes

everyday:
  info <TARGET>                                     show partition table / FS identification
  resize <TARGET>[:N] <SIZE> [OPTIONS]              resize partition + FS (auto online/offline)
  move <TARGET>:N --start <LBA|end> [--yes]        move partition
  copy <TARGET>:N --start <LBA|end> [--name S] [--keep-partuuid] [--yes]
                                                    copy partition
  create <TARGET> [--size SIZE] [--name S] [--fs F] [--yes]
                                                    create partition in free space
  delete <TARGET>:N --yes                           delete partition entry
  set <TARGET>:N name|label|uuid|flag|type ...      set a partition property
                                                    (details: help set)
  check <TARGET>:N                                  check filesystem
  mkfs <TARGET>:N <FS> --yes                        create filesystem
  resizefs <TARGET>:N | <MOUNTPOINT> [BYTES]        resize a filesystem only — the
                                                    partition entry stays put
  undo <TARGET> --yes                               roll back the writes this tool
                                                    completed (replays its journal)
  abandon <TARGET> --yes                            drop an unfinished operation's
                                                    recovery files; no rollback

low-level (one step per run; long jobs stay resumable):
  new <TARGET> [--table gpt|msdos] --yes [--overwrite-partition-data]
                                                    create a fresh partition table
  add <TARGET> --start LBA --end LBA [--name S] [--type T]
                                                    add a partition over [start, end]
  resize-part <TARGET>:N --start LBA (--end LBA | --grow-to-end) [--yes]
                                                    repartition one partition
  plan <TARGET> --grow N|last                       print the relocation plan
  apply <TARGET> --grow N|last [--chunk-size MiB] [--no-fs] [--yes]
                                                    carry out the relocation

target: image path or block device; :N = partition number (1-based). :last = the last
        partition a command can act on. On MBR an extended container does not count
        as one; a target without a partition table has no :last. `--grow` accepts
        `last` for the same partition

options (availability: help <CMD>):
  --align A         placement alignment for add / move / copy / resize-part:
                    mib|cyl|none (default mib = 1MiB; none uses the LBAs as given)
  --chunk-size N    recovery granularity in MiB (default 4); an interrupted run
                    must repeat the same value
  --no-fs           skip the filesystem step of the grown or moved partition —
                    details, limits and refusals: help resize / help move
  --sector-size N   logical sector size override for raw images (power of two,
                    512..=65536)
  --yes             acknowledge a destructive action before it starts: required by
                    delete, mkfs, new, undo, abandon, copy, create --fs, resize
                    on a shrink, and by move / resize-part / apply / resize when
                    their geometry relocates partition data; resize-part also
                    asks when its new end shrinks the partition
  --flag=VALUE is the same as --flag VALUE, except that a value starting with
  `-` needs the equals form. `--` ends option parsing: every later argument is
  positional, which is how a target or name starting with `-` is passed
exit status:
  0   done             layout changed and the kernel partition view refreshed.
                       Every follow-up step completed, or none was needed: no
                       growable filesystem in the region, a PV left to the
                       pvresize chain, or --no-fs given
  10  refused          nothing was written, and the request does not match the
                       target's current state: validation failed, no partition
                       table, partition missing, or the confirmation flag
                       absent — changing arguments may help
  20  partial          layout was written but a follow-up step is pending (remedy
                       command printed per affected partition), or the kernel
                       partition view is stale (run partprobe/partx before use)
  30  infrastructure   could not complete, for one of two reasons: the environment
                       or the on-disk data itself is at fault (I/O error, malformed
                       partition table) and nothing was written; or the failure
                       happened after writing, in which case the message says
                       on-disk state may have changed — verify with `info` before
                       retrying"#;

pub(crate) fn usage() -> ! {
    eprintln!("{USAGE_TEXT}");
    std::process::exit(EXIT_REFUSED as i32);
}

/// 主动请求帮助（`help` 无主题、`--help`、`-h`）的出口：请求有效，退 0
pub(crate) fn usage_help() -> ! {
    println!("{USAGE_TEXT}");
    std::process::exit(crate::support::EXIT_OK as i32);
}

/// 单命令详助（diskedit help <CMD> / <CMD> --help）。文本归各命令模块所有
/// （crate::cmd 的 HELP 常量），此处只做路由；未知主题 → 顶层 usage
pub(crate) fn help_cmd(name: &str) -> ! {
    match crate::cmd::help_text(name) {
        Some(text) => println!("{text}"),
        None => usage(),
    }
    std::process::exit(crate::support::EXIT_OK as i32);
}

/// `--version`：GNU 约定的一行输出（程序名 + 版本），随后成功退出
pub(crate) fn version_help() -> ! {
    println!("diskedit {}", env!("CARGO_PKG_VERSION"));
    std::process::exit(crate::support::EXIT_OK as i32);
}

/// 带参旗标的取值入口：`--flag=value` 的等号部分已在分派前拆出，
/// 这里只在两种写法之间取其一，各臂不必各自判断。空格写法要求下一个 token
/// 不是选项（以 `-` 开头）：把它当值会静默改写请求的语义，而缺值是一次
/// 可改正的用法错误；`-` 开头的值只能写成等号形式
fn take_value(inline: &mut Option<String>, it: &mut impl Iterator<Item = String>, flag: &str) -> String {
    match inline.take() {
        Some(v) => v,
        None => match it.next() {
            Some(v) if !v.starts_with('-') => v,
            _ => miss_arg(flag),
        },
    }
}

/// 一个命令行旗标。`value` 是带值旗标的占位符（错误文案与帮助对账共用），
/// `None` 表示布尔旗标
pub(crate) struct FlagSpec {
    pub(crate) name: &'static str,
    pub(crate) value: Option<&'static str>,
}

/// 命令级旗标的唯一注册表：解析器判重、缺值文案、命令白名单的守护测试都读这一份。
/// 加旗标时只改这里与它的解析臂，不再在任何别处另抄一份清单
pub(crate) const FLAGS: &[FlagSpec] = &[
    FlagSpec { name: "--align", value: Some("A") },
    FlagSpec { name: "--allow-move", value: None },
    FlagSpec { name: "--chunk-size", value: Some("N") },
    FlagSpec { name: "--end", value: Some("LBA") },
    FlagSpec { name: "--fs", value: Some("F") },
    FlagSpec { name: "--grow", value: Some("N|last") },
    FlagSpec { name: "--grow-lv", value: None },
    FlagSpec { name: "--grow-to-end", value: None },
    FlagSpec { name: "--keep-partuuid", value: None },
    FlagSpec { name: "--lv", value: Some("NAME") },
    FlagSpec { name: "--name", value: Some("S") },
    FlagSpec { name: "--no-fs", value: None },
    FlagSpec { name: "--overwrite-partition-data", value: None },
    FlagSpec { name: "--random", value: None },
    FlagSpec { name: "--sector-size", value: Some("N") },
    FlagSpec { name: "--size", value: Some("SIZE") },
    FlagSpec { name: "--start", value: Some("LBA|end") },
    FlagSpec { name: "--table", value: Some("gpt|msdos") },
    FlagSpec { name: "--type", value: Some("T") },
    FlagSpec { name: "--yes", value: None },
];

pub(crate) fn flag_spec(name: &str) -> Option<&'static FlagSpec> {
    FLAGS.iter().find(|f| f.name == name)
}

/// `--start` 的两种给法。合成一个字段后"数值起点与 `end` 同时出现"不再是可构造的
/// 状态，消费点也就不必各自判一次互斥
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StartSelector {
    /// `--start <LBA>`
    Lba(u64),
    /// `--start end`：贴着可用区尾部打包（起点钉在 `last_usable_lba - len + 1`）
    PinnedToEnd,
}

pub(crate) struct Args {
    pub(crate) target: String,
    pub(crate) part: Option<PartSelector>,
    pub(crate) grow: Option<PartSelector>,
    pub(crate) start: Option<StartSelector>,
    pub(crate) end: Option<u64>,
    pub(crate) size: Option<u64>,
    pub(crate) fs: Option<String>,
    pub(crate) name: Option<String>,
    /// --keep-partuuid：copy 沿用源分区的 PARTUUID（默认生成新的 v4 GUID）
    pub(crate) keep_partuuid: bool,
    pub(crate) type_guid: Option<String>,
    pub(crate) table: Option<crate::table::TableKind>,
    pub(crate) yes: bool,
    /// --overwrite-partition-data：`new --table gpt` 在旧分区落入新表元数据写入区时，
    /// 明确授权这次写入覆盖那些字节（`--yes` 只确认"重建表"，不含这项）
    pub(crate) overwrite_partition_data: bool,
    /// --random：`set uuid` 要求目标生成新随机值（唯一支持者是 ntfs）
    pub(crate) random: bool,
    pub(crate) sector_size: Option<u64>,
    pub(crate) align: String,
    pub(crate) chunk_mib: u64,
    pub(crate) grow_to_end: bool,
    pub(crate) allow_move: bool,
    /// --no-fs：grow 目标的 FS 步骤不属后置条件（布局成功即 exit 0）
    pub(crate) no_fs: bool,
    pub(crate) grow_lv: bool,
    pub(crate) lv: Option<String>,
    pub(crate) pos: Vec<String>,
    /// 用户显式给出的旗标（按出现顺序）：fail-closed 校验的消费对账依据
    pub(crate) seen: Vec<&'static str>,
}

impl Args {
    /// 用户是否显式给出了该旗标。`seen` 是唯一记录：默认值（`align` 的 `mib`、
    /// `chunk_mib` 的 4）与"用户给了这个值"在字段上分不开，判断"给没给"只查这里
    pub(crate) fn given(&self, flag: &str) -> bool {
        self.seen.contains(&flag)
    }
}

/// 消费对账：命令收到自己不消费的旗标即拒绝。不做此检查的后果不是报错
/// 就是静默忽略——后者意味着命令的实际行为与用户请求不一致却仍报成功。
/// 白名单取自命令表（`cmd::flags_for`），与分派同源
pub(crate) fn refuse_unconsumed_flags(cmd: &str, a: &Args) {
    let Some(allowed) = crate::cmd::flags_for(cmd) else { return };
    for f in &a.seen {
        if !allowed.contains(f) {
            bail_fail(Fail::refused(format!("{f} is not a valid option for `{cmd}` (see diskedit help {cmd})")));
        }
    }
}

/// 裸 help 请求（`help` / `--help` / `-h`，无主题）不消费任何旗标。不查的话
/// `diskedit help --yes` 会在 usage_help 直接退 0，`--bogs` 更不会被认出——
/// 帮助请求于是成了参数校验的旁路，与 [`refuse_unconsumed_flags`] 同一条判据相悖
pub(crate) fn refuse_flags_on_bare_help(a: &Args) {
    if let Some(f) = a.seen.first() {
        bail_fail(Fail::refused(format!("{f} is not a valid option for `help` (see diskedit help)")));
    }
}

/// 该命令声明的 SIZE 槽位是不是当前的 `pos` 下标。声明取自命令表（与分派同源）；
/// 表里没有的名字（`help`/`--help` 等）一律不收 `-` 开头的值
fn sized_position(cmd: &str, index: usize) -> bool {
    crate::cmd::find(cmd).is_some_and(|c| c.size_pos == Some(index))
}

pub(crate) fn parse_args() -> (String, Args) {
    parse_args_from(std::env::args_os().skip(1))
}

/// 解析一段参数列表（不含程序名）。用法错误经 [`bail_fail`] 直接结束进程
fn parse_args_from(argv: impl Iterator<Item = std::ffi::OsString>) -> (String, Args) {
    // `env::args()` 遇非 UTF-8 参数会 panic（退出码 101，越出 0/10/20/30 契约）。
    // 换成 args_os 后逐个转 &str：转不动就是一次可读的用法拒绝，而不是崩溃
    let mut it = argv.map(|arg| {
        arg.into_string().unwrap_or_else(|bad| {
            bail_fail(Fail::refused(format!(
                "argument {:?} is not valid UTF-8 — diskedit takes its arguments as text",
                bad.to_string_lossy()
            )))
        })
    });
    let cmd = it.next().unwrap_or_else(|| usage());
    let mut a = Args {
        target: String::new(), part: None, grow: None,
        start: None, end: None, size: None, fs: None, name: None, keep_partuuid: false, type_guid: None, table: None,
        yes: false, overwrite_partition_data: false, random: false, no_fs: false, sector_size: None,
        align: "mib".to_string(),
        chunk_mib: 4,
        grow_to_end: false,
        allow_move: false,
        grow_lv: false,
        lv: None,
        pos: Vec::new(),
        seen: Vec::new(),
    };
    let mut positional: Vec<String> = Vec::new();
    // `--help` 只登记、不立即退出：先把整条命令行解析完，让白名单对账看到其余旗标，
    // 最后才打印帮助。帮助请求不能成为绕过参数校验的旁路（`delete img:1 --help --bogs`
    // 与 `delete img:1 --bogs` 必须同判）
    let mut help: Option<Option<String>> = None;
    let mut version = matches!(cmd.as_str(), "--version" | "-V");
    // `--` 之后的 token 一律是位置参数（含以 `-` 开头的目标名/分区名）：
    // 这是让"看起来像旗标"的值有唯一合法写法的出口
    let mut end_of_opts = false;
    while let Some(arg) = it.next() {
        if end_of_opts {
            positional.push(arg);
            continue;
        }
        if arg == "--" {
            end_of_opts = true;
            continue;
        }
        // `--flag=value` 与 `--flag value` 是同一语法：等号形式在分派前拆开，
        // 各臂只从 `take_value` 取值，不必各自判断写法
        let mut inline: Option<String> = None;
        let arg = match arg.split_once('=') {
            Some((flag, v)) if flag.starts_with("--") => {
                inline = Some(v.to_string());
                flag.to_string()
            }
            _ => arg,
        };
        // 同名重复：值型拒绝（两个值互相冲突，last-wins 会让生效值与用户第一次
        // 意图相反且无回显）；布尔幂等（`--yes --yes` 与 `--yes` 是同一状态，包装
        // 脚本可以无条件追加）。带不带值取自注册表，不在此另写一份旗标清单
        if let Some(spec) = flag_spec(&arg)
            && spec.value.is_some()
            && a.seen.contains(&arg.as_str())
        {
            bail_fail(Fail::refused(format!(
                "{arg} {0} is given more than once — a value option cannot take two values",
                spec.value.unwrap_or("VALUE")
            )));
        }
        match arg.as_str() {
            "--yes" => { a.seen.push("--yes"); a.yes = true; }
            "--overwrite-partition-data" => { a.seen.push("--overwrite-partition-data"); a.overwrite_partition_data = true; }
            "--random" => { a.seen.push("--random"); a.random = true; }
            "--sector-size" => {
                a.seen.push("--sector-size");
                let v = take_value(&mut inline, &mut it, "--sector-size");
                let n: u64 = v.parse().unwrap_or_else(|_| bad_arg("--sector-size", &v, " (bytes, e.g. 4096)"));
                // 取值合法性在参数层当场判：改参数有解的错误归用法拒绝（10），
                // 不留给打开阶段报成 Infra(30)——分类不因截获层不同而漂移
                if !(512..=65536).contains(&n) || !n.is_power_of_two() {
                    bad_arg("--sector-size", &v, " (must be a power of two in 512..=65536, e.g. 4096)");
                }
                a.sector_size = Some(n);
            }
            "--grow" => {
                a.seen.push("--grow");
                let v = take_value(&mut inline, &mut it, "--grow");
                // `last` 与数字号并列（大小写不敏感，同 --start end 的先例）：它落到具体号
                // 要等读到分区表，此处只判定形式
                if v.eq_ignore_ascii_case("last") {
                    a.grow = Some(PartSelector::Last);
                } else {
                    let n: u32 = v.parse().unwrap_or_else(|_| bad_arg("--grow", &v, " (partition number or `last`, e.g. 1)"));
                    // 分区号是 1-based：0 会让下游的 (n-1) 下溢，在解析层就挡住
                    if n == 0 {
                        bad_arg("--grow", &v, " (partition number is 1-based)");
                    }
                    a.grow = Some(PartSelector::Number(n));
                }
            }
            "--size" => {
                a.seen.push("--size");
                let v = take_value(&mut inline, &mut it, "--size");
                // 与 resize 的 SIZE 同一单位语法（k/m/g/t，1024 进制，无单位 = 字节，可带小数），
                // 只接受绝对值
                a.size = Some(match parse_size_delta(&v) {
                    Some(SizeArg::Bytes { kind: 0, bytes }) => bytes,
                    Some(_) => bad_arg("--size", &v, " (absolute size only: use 32M, not +32M/-32M/32%)"),
                    None => bad_arg("--size", &v, " (use 10G | 500M | 1.5G | plain bytes, e.g. 33554432)"),
                });
            }
            "--fs" => { a.seen.push("--fs"); a.fs = Some(take_value(&mut inline, &mut it, "--fs")); }
            "--start" => {
                a.seen.push("--start");
                let v = take_value(&mut inline, &mut it, "--start");
                a.start = Some(if v.eq_ignore_ascii_case("end") {
                    StartSelector::PinnedToEnd // 尾部打包：挪到 last_usable 内最后位置
                } else {
                    StartSelector::Lba(v.parse().unwrap_or_else(|_| bad_arg("--start", &v, " (LBA, e.g. 2048)")))
                });
            }
            "--end" => {
                a.seen.push("--end");
                let v = take_value(&mut inline, &mut it, "--end");
                a.end = Some(v.parse().unwrap_or_else(|_| bad_arg("--end", &v, " (LBA, e.g. 67583)")));
            }
            "--align" => {
                a.seen.push("--align");
                let v = take_value(&mut inline, &mut it, "--align");
                // 取值合法性在参数层当场判（与 --sector-size / --chunk-size 同理）：
                // 留到开盘之后才判，会被"目标上有未收尾现场"的 busy(30) 抢在前面，
                // 把一条纯用法错误（应为 10 + invalid --align）报成环境/现状错误
                if !matches!(v.as_str(), "mib" | "cyl" | "none") {
                    bad_arg("--align", &v, " (mib|cyl|none)");
                }
                a.align = v;
            }
            "--chunk-size" => {
                a.seen.push("--chunk-size");
                let v = take_value(&mut inline, &mut it, "--chunk-size");
                let n: u64 = v.parse().unwrap_or_else(|_| bad_arg("--chunk-size", &v, " (MiB, e.g. 4)"));
                // 取值合法性在参数层当场判（与 --sector-size 同理：分类不因截获层不同
                // 而漂移）；chunk_bytes 作为 pub 领域入口保留同一判据自守
                if !(1..=1024).contains(&n) {
                    bad_arg("--chunk-size", &v, " (must be in 1..=1024 MiB, e.g. 4)");
                }
                a.chunk_mib = n;
            }
            "--grow-to-end" => { a.seen.push("--grow-to-end"); a.grow_to_end = true; }
            "--allow-move" => { a.seen.push("--allow-move"); a.allow_move = true; }
            "--no-fs" => { a.seen.push("--no-fs"); a.no_fs = true; }
            "--grow-lv" => { a.seen.push("--grow-lv"); a.grow_lv = true; }
            "--lv" => { a.seen.push("--lv"); a.lv = Some(take_value(&mut inline, &mut it, "--lv")); }
            "--name" => { a.seen.push("--name"); a.name = Some(take_value(&mut inline, &mut it, "--name")); }
            "--keep-partuuid" => { a.seen.push("--keep-partuuid"); a.keep_partuuid = true; }
            "--type" => { a.seen.push("--type"); a.type_guid = Some(take_value(&mut inline, &mut it, "--type")); }
            "--table" => {
                a.seen.push("--table");
                let v = take_value(&mut inline, &mut it, "--table");
                a.table = Some(crate::table::TableKind::parse(&v).unwrap_or_else(|| bad_arg("--table", &v, " (gpt|msdos)")));
            }
            "--version" | "-V" => version = true,
            "--help" | "-h" => {
                // 主题：`--help <CMD>` 这种命令位本身就是 help 的写法取位置参数；
                // `<CMD> --help` 的主题是命令名本身（取位置参数会让详助退化成顶层 usage）
                if help.is_none() {
                    help = Some(if matches!(cmd.as_str(), "help" | "--help" | "-h") {
                        positional.first().cloned()
                    } else {
                        Some(cmd.clone())
                    });
                }
            }
            // 未识别的以 `-` 开头的 token 是旗标笔误，不能降级成位置参数：值槽全程不校验
            // 前缀（`set img:1 name --bogs` 会把分区名改成 `--bogs`）。相对尺寸（`-10`、
            // `-10G`）是唯一合法的例外，且只在这个命令声明的 SIZE 槽位放行——放宽到任何
            // 位置，`info -10` 会把一次旗标笔误变成打开失败；其余须写在 `--` 之后
            arg if arg.starts_with('-')
                && (parse_size_delta(arg).is_none() || !sized_position(&cmd, positional.len())) =>
            {
                bail_fail(Fail::refused(format!(
                    "unknown flag `{arg}` — run `diskedit help` for the accepted flags (a value starting with `-` is \
                     accepted as a relative size, e.g. -10G, or after `--`)"
                )))
            }
            // 位置参数：等号拆分只发生在 `--` 开头的 token 上，所以这里没有
            // 未消费的等号值可查，本轮到此结束
            _ => {
                positional.push(arg);
                continue;
            }
        }
        // 等号右侧的值只归带参旗标消费：布尔旗标带上 `=value` 是笔误，
        // 静默忽略会让用户以为那项设置生效了
        if let Some(v) = inline {
            bail_fail(Fail::refused(format!(
                "{arg} takes no value — write it as a bare flag (got {v:?})"
            )));
        }
    }
    // 帮助在整条命令行解析完之后处理：走到这里说明没有未知旗标（那个分支已退出），
    // 再按命令的白名单对账一次，帮助与执行看到同一套参数判据
    if let Some(topic) = help {
        match topic {
            Some(t) => {
                refuse_unconsumed_flags(&t, &a);
                help_cmd(&t);
            }
            None => {
                refuse_flags_on_bare_help(&a);
                usage_help()
            }
        }
    }
    // `--version` 与 `--help` 同样先解析完整条命令行：版本是主动请求，但不是参数
    // 校验的旁路——未消费的旗标与多余位置参数一律拒绝，而不是静默丢弃。
    // 帮助更具体，故排在版本之前
    if version {
        if let Some(f) = a.seen.first() {
            bail_fail(Fail::refused(format!("`--version` takes no other options (got {f})")));
        }
        if let Some(t) = positional.first() {
            bail_fail(Fail::refused(format!("`--version` takes no other arguments (got {t})")));
        }
        version_help();
    }
    // 无 target（含裸调用/未知命令缺参）时打印帮助而非静默退出；但命令位本身就是
    // help 请求（`help` / `--help` / `-h` 且无主题）时是主动求助，退 0
    let target = match positional.first() {
        Some(t) => t.clone(),
        None if matches!(cmd.as_str(), "help" | "--help" | "-h") => {
            refuse_flags_on_bare_help(&a);
            usage_help()
        }
        // 已知命令缺目标：定向说缺什么；未知命令（`diskedit bogs`）仍打整份用法，
        // 那才是"没有这条命令"该有的答复
        None => match crate::cmd::find(cmd.as_str()) {
            None => usage(),
            Some(_) => bail_fail(Fail::refused(format!(
                "`{cmd}` needs a target (an image path or block device) — see diskedit help {cmd}"
            ))),
        },
    };
    let (target, part) =
        crate::dev::parse_target(&target).unwrap_or_else(|e| bail_fail(Fail::refused(e)));
    a.target = target;
    a.part = part;
    a.pos = positional.clone();
    // `--lv NAME` 指定扩哪个 LV，已经预设了"扩 LV"这一步，等价于同时给出 --grow-lv。
    // 归一化在此完成：消费点只看 grow_lv，不存在"给了 --lv 却没扩 LV"的第三种状态
    a.grow_lv |= a.lv.is_some();
    (cmd, a)
}

/// 参数缺值/坏值的统一拒绝出口：走 `bail_fail`（报告文字与退出码都取自 outcome，
/// 不在参数层自拼前缀绕开唯一映射）。报出旗标名与其占位符，避免静默 exit
/// 使用户无从排查
fn miss_arg(flag: &str) -> ! {
    let placeholder = flag_spec(flag).and_then(|s| s.value).unwrap_or("VALUE");
    bail_fail(Fail::refused(format!("{flag} {placeholder} requires a value (see diskedit help)")))
}

fn bad_arg(flag: &str, v: &str, hint: &str) -> ! {
    bail_fail(Fail::refused(format!("bad value {v:?} for {flag}{hint}")))
}

/// SIZE 数值。`Bytes` 已按单位折成整字节（小数向下取整）；`Percent` 保留分子/
/// 分母：百分比的基数是锁下才拿得到的当前尺寸，解析层除不了
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum SizeArg {
    Bytes { kind: i8, bytes: u64 },
    Percent { kind: i8, num: u64, denom: u64 },
}

/// SIZE 字符串解析。单位 k/m/g/t（1024 进制，大小写均可），无单位 = 字节；
/// 小数按十进制位精确换算（1.5G = 1610612736 字节，不经浮点）；"+10%/-10%"
/// 为锚定当前分区字节数的百分比增量，同样接受小数（+1.5%）；绝对形式 "10%"
/// 有歧义故不受理；"grow" 由调用方先行处理
pub(crate) fn parse_size_delta(s: &str) -> Option<SizeArg> {
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
    if let Some(pct) = rest.strip_suffix('%') {
        if kind == 0 {
            return None;
        }
        let (num, denom) = parse_decimal(pct)?;
        return Some(SizeArg::Percent { kind, num, denom });
    }
    let (mult, text) = match rest.as_bytes().last()? {
        b'k' | b'K' => (1024u64, &rest[..rest.len() - 1]),
        b'm' | b'M' => (1024 * 1024, &rest[..rest.len() - 1]),
        b'g' | b'G' => (1024 * 1024 * 1024, &rest[..rest.len() - 1]),
        b't' | b'T' => (1024u64 * 1024 * 1024 * 1024, &rest[..rest.len() - 1]),
        b'0'..=b'9' | b'.' => (1u64, rest),
        _ => return None,
    };
    let (num, denom) = parse_decimal(text)?;
    // 字节数 = num * mult / denom，向下取整；超过 u64 的尺寸不存在，直接拒绝
    let bytes = (num as u128).checked_mul(mult as u128)?.checked_div(denom as u128)?;
    (bytes <= u64::MAX as u128).then_some(SizeArg::Bytes { kind, bytes: bytes as u64 })
}

/// 十进制字面量 →（分子, 分母），整数写法分母为 1。小数位直接进位到分母，
/// 全程整数运算：1.5G 必须正好是 1610612736 字节，浮点会在边界上给出相邻值
fn parse_decimal(text: &str) -> Option<(u64, u64)> {
    if text.is_empty() || text.ends_with('.') {
        return None;
    }
    let (int_part, frac_part) = match text.split_once('.') {
        Some((i, f)) => (i, f),
        None => (text, ""),
    };
    if (int_part.is_empty() && frac_part.is_empty())
        || !int_part.bytes().all(|b| b.is_ascii_digit())
        || !frac_part.bytes().all(|b| b.is_ascii_digit())
    {
        return None;
    }
    // 分母 10^f 必须落在 u64：小数位上限 18
    if frac_part.len() > 18 {
        return None;
    }
    let denom = 10u64.checked_pow(frac_part.len() as u32)?;
    let int: u64 = if int_part.is_empty() { 0 } else { int_part.parse().ok()? };
    let frac: u64 = if frac_part.is_empty() { 0 } else { frac_part.parse().ok()? };
    let num = (int as u128).checked_mul(denom as u128)?.checked_add(frac as u128)?;
    (num <= u64::MAX as u128).then_some((num as u64, denom))
}

#[cfg(test)]
#[path = "../tests/cli/flag_probes.rs"]
mod flag_probes;

#[cfg(test)]
mod tests {
    use super::{parse_size_delta, SizeArg};

    fn bytes(kind: i8, v: u64) -> Option<SizeArg> {
        Some(SizeArg::Bytes { kind, bytes: v })
    }

    fn percent(kind: i8, num: u64, denom: u64) -> Option<SizeArg> {
        Some(SizeArg::Percent { kind, num, denom })
    }

    /// 探针与注册表一一对应：集合相等、双方无重名、形态一致、值型有样例且布尔
    /// 无样例。新增第 21 个旗标却忘记写探针，会在这里失败，而不是让集成测试
    /// 悄悄少覆盖一条
    #[test]
    fn flag_probes_cover_the_registry() {
        use super::flag_probes::{Kind, PROBES};
        use super::FLAGS;

        let mut registered = std::collections::BTreeSet::new();
        for f in FLAGS {
            assert!(registered.insert(f.name), "duplicate registry entry: {}", f.name);
        }
        let mut probed = std::collections::BTreeSet::new();
        for p in PROBES {
            assert!(probed.insert(p.name), "duplicate probe: {}", p.name);
            let spec = FLAGS
                .iter()
                .find(|f| f.name == p.name)
                .unwrap_or_else(|| panic!("probe for unregistered flag: {}", p.name));
            match p.kind {
                Kind::Value => {
                    assert!(spec.value.is_some(), "{} is a boolean in the registry", p.name);
                    assert!(p.sample.is_some(), "value flag {} needs a sample", p.name);
                }
                Kind::Bool => {
                    assert!(spec.value.is_none(), "{} takes a value in the registry", p.name);
                    assert!(p.sample.is_none(), "boolean flag {} must not carry a sample", p.name);
                }
            }
            // 探针点名的命令必须存在，且该旗标在它的白名单里
            let allowed = crate::cmd::flags_for(p.command)
                .unwrap_or_else(|| panic!("probe names unknown command: {}", p.command));
            assert!(allowed.contains(&p.name), "`{}` does not accept {}", p.command, p.name);
        }
        assert_eq!(probed, registered, "probe set and registry must match exactly");
    }

    /// SIZE 解析（单位 k/m/g/t = 1024 进制、无单位 = 字节、小数、+/- 增量、+N%/-N%）
    #[test]
    fn size_delta_parsing() {
        const G: u64 = 1024 * 1024 * 1024;
        assert_eq!(parse_size_delta("1024"), bytes(0, 1024));
        assert_eq!(parse_size_delta("10G"), bytes(0, 10 * G));
        assert_eq!(parse_size_delta("10g"), bytes(0, 10 * G));
        assert_eq!(parse_size_delta("512"), bytes(0, 512));
        assert_eq!(parse_size_delta("+2k"), bytes(1, 2048));
        assert_eq!(parse_size_delta("-500M"), bytes(-1, 500 * 1024 * 1024));
        assert_eq!(parse_size_delta("1T"), bytes(0, 1024 * G));
        // 小数：十进制位精确换算，1.5G 正好是 1610612736 字节；不进位、不取近似
        assert_eq!(parse_size_delta("1.5G"), bytes(0, 1610612736));
        assert_eq!(parse_size_delta("+1.5G"), bytes(1, 1610612736));
        assert_eq!(parse_size_delta(".5G"), bytes(0, 1024 * 1024 * 1024 / 2));
        assert_eq!(parse_size_delta("0.5M"), bytes(0, 1024 * 1024 / 2));
        // 字节数不整除时向下取整：1.1G = 1181116006.4 → 1181116006
        assert_eq!(parse_size_delta("1.1G"), bytes(0, 1181116006));
        // 百分比同样接受小数：基数在调用方（锁下当前尺寸），这里保留分子/分母
        assert_eq!(parse_size_delta("+10%"), percent(1, 10, 1));
        assert_eq!(parse_size_delta("-10%"), percent(-1, 10, 1));
        assert_eq!(parse_size_delta("+1.5%"), percent(1, 15, 10));
        // u64 边界与乘法溢出：溢出返回 None，绝不回绕
        assert_eq!(parse_size_delta("18446744073709551615"), bytes(0, u64::MAX));
        assert_eq!(parse_size_delta("18446744073709551615G"), None);
        assert_eq!(parse_size_delta("18446744073709551616"), None);
        // 绝对百分比有歧义（盘的 10% 还是分区的 10%），不支持；其余非法输入
        for bad in ["", "+", "-", "%", "10%", "+%", "G", "+G", "10KB", "10 G", "0x10", " 1G", "1b", "512B",
                    "1.G", ".", "1..5G", "1.5.5G", "1.5%", "+.G"] {
            assert_eq!(parse_size_delta(bad), None, "{bad:?} must be rejected");
        }
        // 符号只在最前出现一次：+10G/-10G 合法，重复/混用符号一律非法
        assert_eq!(parse_size_delta("+10G"), bytes(1, 10 * G));
        assert_eq!(parse_size_delta("-10G"), bytes(-1, 10 * G));
        for bad in ["++10G", "--10G", "+++10G", "+-10G", "-+10G", "++10%", "+ 10G"] {
            assert_eq!(parse_size_delta(bad), None, "{bad:?} must be rejected");
        }
    }

    /// USAGE 的 examples 段是接口的一部分：命令名必须真实存在；把分区选择器写在
    /// 目标后缀上的命令，块设备目标必须带 `:N`——否则示例在 Linux 上会被"目标须为
    /// 整盘"的拒绝档拦下，用户照抄得到的是失败
    #[test]
    fn usage_examples_follow_the_command_syntax() {
        let examples: Vec<&str> = super::USAGE_TEXT
            .lines()
            .skip_while(|l| !l.starts_with("examples:"))
            .skip(1)
            .take_while(|l| l.starts_with("  "))
            .map(str::trim)
            .collect();
        assert!(!examples.is_empty(), "USAGE must keep an examples block");
        // resize 的整盘镜像（superfloppy）不带后缀，故本规则只约束 `/dev/` 下的块设备路径
        const PART_SUFFIX_REQUIRED: &[&str] = &["resize", "move", "copy", "delete", "set", "check", "mkfs"];
        for line in examples {
            let mut it = line.split_whitespace();
            assert_eq!(it.next(), Some("diskedit"), "example must start with the binary name: {line}");
            let cmd = it.next().unwrap_or_else(|| panic!("example has no command: {line}"));
            assert!(crate::cmd::find(cmd).is_some(), "example names unknown command {cmd}: {line}");
            let Some(target) = it.next() else { continue };
            if PART_SUFFIX_REQUIRED.contains(&cmd) && target.starts_with("/dev/") {
                let suffix = target.rsplit_once(':').map(|(_, p)| p).unwrap_or("");
                assert!(
                    (suffix.chars().all(|c| c.is_ascii_digit()) && !suffix.is_empty())
                        || suffix.eq_ignore_ascii_case("last"),
                    "{cmd}: block-device target {target} has no partition selector :N: {line}"
                );
            }
        }
    }

    /// README 的选项表与注册表双向对账：表里每个 `--flag` 必须已注册，取值形态与
    /// 占位符同 `FLAGS` 一致；注册表里每个旗标必须出现在表里。README 不参与解析，
    /// 却是对用户承诺能力的第二处文本，漏写或写错只能靠这条对账发现
    #[test]
    fn readme_options_match_the_registry() {
        use super::{flag_spec, FLAGS};

        const README: &str = include_str!("../README.md");
        let rows: Vec<&str> = README
            .lines()
            .skip_while(|l| l.trim() != "## 选项")
            .skip(1)
            .take_while(|l| !l.starts_with("## "))
            .collect();
        assert!(!rows.is_empty(), "README must carry a `## 选项` section");

        // 中转字符：取值列里的 `\|` 是字面竖线，直接 split('|') 会把一个单元格切成两个
        const PIPE: char = '\u{00A6}';
        let mut documented: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        for line in rows {
            let row = line.replace("\\|", &PIPE.to_string());
            let Some(rest) = row.strip_prefix('|') else { continue };
            let cells: Vec<&str> = rest.split('|').collect();
            // 只有反引号包起来的 `--flag` 才是表项：表头与分隔行（`|---|`）也以 `|`
            // 开头，但它们的第一格不是旗标写法
            let cell = cells[0].trim();
            if !cell.starts_with("`--") {
                continue;
            }
            let name = cell.trim_matches('`');
            assert!(documented.insert(name.to_string()), "README documents {name} twice");
            let spec = flag_spec(name)
                .unwrap_or_else(|| panic!("README documents an unregistered flag: {name}"));
            let value = cells[1].trim().replace(PIPE, "|");
            match spec.value {
                None => assert_eq!(
                    value, "—",
                    "{name} is a boolean in the registry, so README must give it no value"
                ),
                Some(placeholder) => {
                    assert_ne!(value, "—", "{name} takes a value but README calls it a boolean");
                    assert_eq!(value, placeholder, "{name}: README placeholder must match the registry");
                }
            }
        }
        let registered: std::collections::BTreeSet<String> =
            FLAGS.iter().map(|f| f.name.to_string()).collect();
        assert_eq!(documented, registered, "README options and the registry must match exactly");
    }

    /// 带参旗标的两种取值写法落到同一个字段；等号写法允许值以 `-` 开头，
    /// 空格写法只接受不以 `-` 开头的值
    #[test]
    fn value_flags_accept_plain_and_equals_values() {
        use std::ffi::OsString;

        fn parse(tokens: &[&str]) -> super::Args {
            let argv: Vec<OsString> = tokens.iter().copied().map(OsString::from).collect();
            super::parse_args_from(argv.into_iter()).1
        }

        let a = parse(&["copy", "img:1", "--start", "1000", "--name", "valid-name"]);
        assert_eq!(a.name.as_deref(), Some("valid-name"));
        assert_eq!(a.start, Some(super::StartSelector::Lba(1000)));

        let a = parse(&["copy", "img:1", "--start", "1000", "--name=-leading-value"]);
        assert_eq!(a.name.as_deref(), Some("-leading-value"));
    }

    /// `given` 只认用户显式给出的旗标：默认值（`align` 的 mib、`chunk_mib` 的 4）
    /// 与"显式给了同一个值"必须是两种事实，否则 `--start end --align mib` 会被
    /// 当成"没给 --align"而静默忽略
    #[test]
    fn given_reports_only_explicit_flags() {
        use std::ffi::OsString;

        fn parse(tokens: &[&str]) -> super::Args {
            let argv: Vec<OsString> = tokens.iter().copied().map(OsString::from).collect();
            super::parse_args_from(argv.into_iter()).1
        }

        let a = parse(&["move", "img:1"]);
        assert!(!a.given("--align"), "--align was not given");
        assert!(!a.given("--chunk-size"), "--chunk-size was not given");
        assert_eq!(a.align, "mib");
        assert_eq!(a.chunk_mib, 4);

        let a = parse(&["move", "img:1", "--align", "mib", "--chunk-size", "8"]);
        assert!(a.given("--align"), "explicit --align mib must be distinguishable from the default");
        assert!(a.given("--chunk-size"), "explicit --chunk-size must be recorded");
        assert_eq!(a.align, "mib");
        assert_eq!(a.chunk_mib, 8);
    }
}
