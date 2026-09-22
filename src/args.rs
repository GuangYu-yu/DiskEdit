//! 参数层：命令行解析、每命令旗标消费契约（fail-closed）与帮助文本入口。

use crate::support::{bail_fail, Fail, EXIT_REFUSED};

/// 顶层用法文本。错误用法打到 stderr 退 10（refused：请求无效、未写盘）；
/// `help` / `--help` / `-h` 这类**主动请求帮助**打到 stdout 退 0（请求本身有效，
/// 按 GNU 惯例帮助出口 0）
const USAGE_TEXT: &str = r#"diskedit — disk / image editor

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
  abandon <TARGET> --yes                         give up on an unfinished operation

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

pub(crate) struct Args {
    pub(crate) target: String,
    pub(crate) part: Option<u32>,
    pub(crate) grow: Option<u32>,
    pub(crate) start: Option<u64>,
    pub(crate) end: Option<u64>,
    pub(crate) size: Option<u64>,
    pub(crate) fs: Option<String>,
    pub(crate) name: Option<String>,
    pub(crate) type_guid: Option<String>,
    pub(crate) table: Option<crate::table::TableKind>,
    pub(crate) yes: bool,
    pub(crate) online: bool,
    /// --random：`set uuid` 要求目标生成新随机值（唯一支持者是 ntfs）
    pub(crate) random: bool,
    pub(crate) sector_size: Option<u64>,
    pub(crate) align: String,
    pub(crate) chunk_mib: u64,
    pub(crate) grow_to_end: bool,
    pub(crate) allow_move: bool,
    /// --no-fs：只改分区布局，FS 扩展不属后置条件（布局成功即 exit 0）
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

pub(crate) fn parse_args() -> (String, Args) {
    let mut it = std::env::args().skip(1);
    let cmd = it.next().unwrap_or_else(|| usage());
    let mut a = Args {
        target: String::new(), part: None, grow: None,
        start: None, end: None, size: None, fs: None, name: None, type_guid: None, table: None,
        yes: false, online: false, random: false, no_fs: false, sector_size: None,
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
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--yes" => { a.seen.push("--yes"); a.yes = true; }
            "--online" => { a.seen.push("--online"); a.online = true; }
            "--random" => { a.seen.push("--random"); a.random = true; }
            "--sector-size" => {
                a.seen.push("--sector-size");
                let v = it.next().unwrap_or_else(|| miss_arg("--sector-size"));
                a.sector_size = Some(v.parse().unwrap_or_else(|_| bad_arg("--sector-size", &v, " (bytes, e.g. 4096)")));
            }
            "--grow" => {
                a.seen.push("--grow");
                let v = it.next().unwrap_or_else(|| miss_arg("--grow"));
                let n: u32 = v.parse().unwrap_or_else(|_| bad_arg("--grow", &v, " (partition number, e.g. 1)"));
                // 分区号是 1-based：0 会让下游的 (n-1) 下溢，在解析层就挡住
                if n == 0 {
                    bad_arg("--grow", &v, " (partition number is 1-based)");
                }
                a.grow = Some(n);
            }
            "--size" => {
                a.seen.push("--size");
                let v = it.next().unwrap_or_else(|| miss_arg("--size"));
                // 与 resize 的 SIZE 同一单位语法（b/k/m/g/t，1024 进制），但只接受绝对值
                a.size = Some(match parse_size_delta(&v) {
                    Some((bytes, 0, false)) => bytes,
                    Some(_) => bad_arg("--size", &v, " (absolute size only: use 32M, not +32M/-32M/32%)"),
                    None => bad_arg("--size", &v, " (use 10G | 500M | plain bytes, e.g. 33554432)"),
                });
            }
            "--fs" => { a.seen.push("--fs"); a.fs = Some(it.next().unwrap_or_else(|| miss_arg("--fs"))); }
            "--start" => {
                a.seen.push("--start");
                let v = it.next().unwrap_or_else(|| miss_arg("--start"));
                if v.eq_ignore_ascii_case("end") {
                    a.start_end = true; // 尾部打包：挪到 last_usable 内最后位置
                } else {
                    a.start = Some(v.parse().unwrap_or_else(|_| bad_arg("--start", &v, " (LBA, e.g. 2048)")));
                }
            }
            "--end" => {
                a.seen.push("--end");
                let v = it.next().unwrap_or_else(|| miss_arg("--end"));
                a.end = Some(v.parse().unwrap_or_else(|_| bad_arg("--end", &v, " (LBA, e.g. 67583)")));
            }
            "--align" => { a.seen.push("--align"); a.align = it.next().unwrap_or_else(|| miss_arg("--align")); }
            "--chunk-size" => {
                a.seen.push("--chunk-size");
                let v = it.next().unwrap_or_else(|| miss_arg("--chunk-size"));
                a.chunk_mib = v.parse().unwrap_or_else(|_| bad_arg("--chunk-size", &v, " (MiB, e.g. 4)"));
            }
            "--grow-to-end" => { a.seen.push("--grow-to-end"); a.grow_to_end = true; }
            "--allow-move" => { a.seen.push("--allow-move"); a.allow_move = true; }
            "--no-fs" => { a.seen.push("--no-fs"); a.no_fs = true; }
            "--grow-lv" => { a.seen.push("--grow-lv"); a.grow_lv = true; }
            "--lv" => { a.seen.push("--lv"); a.lv = Some(it.next().unwrap_or_else(|| miss_arg("--lv"))); }
            "--name" => { a.seen.push("--name"); a.name = Some(it.next().unwrap_or_else(|| miss_arg("--name"))); }
            "--type" => { a.seen.push("--type"); a.type_guid = Some(it.next().unwrap_or_else(|| miss_arg("--type"))); }
            "--table" => {
                a.seen.push("--table");
                let v = it.next().unwrap_or_else(|| miss_arg("--table"));
                a.table = Some(crate::table::TableKind::parse(&v).unwrap_or_else(|| bad_arg("--table", &v, " (gpt|msdos)")));
            }
            // <CMD> --help：主题是命令名本身——`resize img:1 --help` 的位置参数是目标，
            // 不是主题（取它会让详助退化成顶层 usage）。只有 `--help <CMD>` 这种
            // 命令位本身就是 help 的写法，位置参数才当主题
            "--help" | "-h" => {
                let topic = if matches!(cmd.as_str(), "help" | "--help" | "-h") {
                    match positional.first() {
                        Some(t) => t.clone(),
                        // 命令位与旗标都是 help（`diskedit --help`）：主动求助，退 0
                        None => usage_help(),
                    }
                } else {
                    cmd.clone()
                };
                help_cmd(&topic);
            }
            _ => positional.push(arg),
        }
    }
    // 无 target（含裸调用/未知命令缺参）时打印帮助而非静默退出；但命令位本身就是
    // help 请求（`help` / `--help` / `-h` 且无主题）时是主动求助，退 0
    let target = match positional.first() {
        Some(t) => t.clone(),
        None if matches!(cmd.as_str(), "help" | "--help" | "-h") => usage_help(),
        None => usage(),
    };
    let (target, part) =
        crate::dev::parse_target(&target).unwrap_or_else(|e| bail_fail(Fail::refused(e)));
    a.target = target;
    a.part = part;
    a.pos = positional.clone();
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