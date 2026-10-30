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
  diskedit resize /dev/sdb1 +2G
  diskedit resize disk.img grow --allow-move --yes
  diskedit undo /dev/sdb --yes

everyday:
  info <TARGET>                                     show partition table / FS identification
  resize <TARGET>[:N] <SIZE> [OPTIONS]              resize partition + FS (auto online/offline)
  move <TARGET>:N --start <LBA|end>                 move partition
  copy <TARGET>:N --start <LBA|end> [--name S] [--keep-partuuid]
                                                    copy partition
  create <TARGET> [--size SIZE] [--name S] [--fs F]
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
  resize-part <TARGET>:N --start LBA (--end LBA | --grow-to-end)
                                                    repartition one partition
  plan <TARGET> --grow N|last                       print the relocation plan
  apply <TARGET> --grow N|last [--chunk-size MiB] [--no-fs]
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
  --yes             acknowledge a destructive or unrequested-relocation action:
                    required by delete, mkfs, new, undo, abandon, and by resize
                    when it has to relocate partitions not named on the command
                    line. Commands whose documented job is the relocation — move,
                    copy, apply, resize-part — do not ask
  --flag=VALUE is the same as --flag VALUE. `--` ends option parsing: every
  later argument is positional, which is how a target or name starting with
  `-` is passed
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
/// 这里只在两种写法之间取其一，各臂不必各自判断
fn take_value(inline: &mut Option<String>, it: &mut impl Iterator<Item = String>, flag: &str) -> String {
    match inline.take() {
        Some(v) => v,
        None => it.next().unwrap_or_else(|| miss_arg(flag)),
    }
}

pub(crate) struct Args {
    pub(crate) target: String,
    pub(crate) part: Option<PartSelector>,
    pub(crate) grow: Option<PartSelector>,
    pub(crate) start: Option<u64>,
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
    pub(crate) start_end: bool,
    pub(crate) pos: Vec<String>,
    /// 用户显式给出的旗标（按出现顺序）：fail-closed 校验的消费对账依据
    pub(crate) seen: Vec<&'static str>,
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

pub(crate) fn parse_args() -> (String, Args) {
    // `env::args()` 遇非 UTF-8 参数会 panic（退出码 101，越出 0/10/20/30 契约）。
    // 换成 args_os 后逐个转 &str：转不动就是一次可读的用法拒绝，而不是崩溃
    let mut it = std::env::args_os().skip(1).map(|arg| {
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
        start_end: false,
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
        // 同一旗标重复给出即拒绝。`seen` 本来就记着每一次出现，具备判重能力；
        // 不判的后果是 last-wins——生效值与用户第一意图相反却没有任何回显
        if a.seen.contains(&arg.as_str()) {
            bail_fail(Fail::refused(format!(
                "{arg} is given more than once — refusing rather than letting the last one win silently"
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
                // 与 resize 的 SIZE 同一单位语法（b/k/m/g/t，1024 进制），但只接受绝对值
                a.size = Some(match parse_size_delta(&v) {
                    Some((bytes, 0, false)) => bytes,
                    Some(_) => bad_arg("--size", &v, " (absolute size only: use 32M, not +32M/-32M/32%)"),
                    None => bad_arg("--size", &v, " (use 10G | 500M | plain bytes, e.g. 33554432)"),
                });
            }
            "--fs" => { a.seen.push("--fs"); a.fs = Some(take_value(&mut inline, &mut it, "--fs")); }
            "--start" => {
                a.seen.push("--start");
                let v = take_value(&mut inline, &mut it, "--start");
                if v.eq_ignore_ascii_case("end") {
                    a.start_end = true; // 尾部打包：挪到 last_usable 内最后位置
                } else {
                    a.start = Some(v.parse().unwrap_or_else(|_| bad_arg("--start", &v, " (LBA, e.g. 2048)")));
                }
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
            // 前缀（`set img:1 name --bogs` 会把分区名改成 `--bogs`）。`-10G` 这类相对尺寸
            // 是唯一合法的例外，判据与 SIZE 语法同源；其余以 `-` 开头的值须写在 `--` 之后
            arg if arg.starts_with('-') && parse_size_delta(arg).is_none() => {
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
    // `--version` 与 `--help` 同样先解析完整条命令行：白名单对账仍然生效，
    // 它不能成为绕过参数校验的旁路。帮助更具体，故排在版本之前
    if version {
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
        None => usage(),
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
/// 不在参数层自拼前缀绕开唯一映射）。报出旗标名，避免静默 exit 使用户无从排查
fn miss_arg(flag: &str) -> ! {
    bail_fail(Fail::refused(format!("{flag} requires a value (see diskedit help)")))
}

fn bad_arg(flag: &str, v: &str, hint: &str) -> ! {
    bail_fail(Fail::refused(format!("bad value {v:?} for {flag}{hint}")))
}

/// SIZE 字符串 →（数值, 类别 0=绝对/1=扩/-1=缩, 是否百分号）。
/// 单位 b/k/m/g/t（1024 进制，大小写均可），无单位 = 字节；"+10%/-10%" 为锚定当前
/// 分区字节数的百分比增量；绝对形式 "10%" 有歧义故不支持；"grow" 由调用方先行处理
pub(crate) fn parse_size_delta(s: &str) -> Option<(u64, i8, bool)> {
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

#[cfg(test)]
mod tests {
    use super::parse_size_delta;

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
}
