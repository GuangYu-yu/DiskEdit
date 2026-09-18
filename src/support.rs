//! 命令层共用的支撑件：进程退出出口、目标打开/journal、几何与对齐助手、GUID/JSON 编解码。
//! 退出码常量的唯一定义在 outcome 模块，此处重导出给命令层，避免两套常量各自漂移

pub(crate) use crate::outcome::{EXIT_INFRA, EXIT_OK, EXIT_PARTIAL, EXIT_REFUSED};

#[cfg(unix)]
use std::os::unix::fs::FileTypeExt;

use crate::args::Args;
use crate::dev::{FileSource, Journal};
use crate::{dev, gpt_policy, movepart, table};

pub(crate) fn bail(code: u8, msg: String) -> ! {
    eprintln!("{msg}");
    std::process::exit(code as i32);
}

/// 规划/执行失败的出口：报告文字与退出码都取自 outcome（唯一措辞与唯一映射），
/// 调用点不得自行拼装
pub(crate) fn bail_fail(f: crate::outcome::Fail) -> ! {
    let o = crate::outcome::finish(Err(f), Vec::new());
    o.report();
    std::process::exit(o.exit_code() as i32);
}

/// 日志：镜像 = `<名>.diskedit.log`；块设备 = <state_dir()>/<GUID>.diskedit.log，
/// 无 GPT（MBR/裸盘）时用 <devname>.diskedit.log。
///
/// 这是**未迁移的历史命名**：日志只增、不参与恢复，也不决定 journal / checkpoint 的
/// 落点，故不并入 TargetIdentity 的推导。改名会打断既有日志的连续性，收益不足；
/// 真要统一命名时另做一次迁移
pub(crate) struct Logger {
    file: Option<std::fs::File>,
}

impl Logger {
    pub(crate) fn open(src: &FileSource) -> Self {
        let path = if src.is_block {
            let dir = dev::state_dir();
            dev::best_effort_mkdir(&dir);
            let name = table::load_gpt(src)
                .ok()
                .flatten()
                .map(|g| {
                    let hex: String = g.header.disk_guid.iter().map(|b| format!("{b:02X}")).collect();
                    format!("{hex}.diskedit.log")
                })
                .unwrap_or_else(|| {
                    // 无 GPT（MBR/裸盘）：devname 是无表场景唯一稳定标识
                    let name = src.path.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "dev".into());
                    format!("{name}.diskedit.log")
                });
            dir.join(name)
        } else {
            let mut p = src.path.clone().into_os_string();
            p.push(".diskedit.log");
            std::path::PathBuf::from(p)
        };
        let file = std::fs::OpenOptions::new().create(true).append(true).open(path).ok();
        if file.is_none() {
            // 落盘失败不静默：用户需知日志只进 stdout
            eprintln!("warning: persistent log unavailable — output only goes to stdout");
        }
        Logger { file }
    }

    pub(crate) fn log(&mut self, msg: &str) {
        let ts = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        println!("{msg}");
        if let Some(f) = &mut self.file {
            best_effort_log_write(f, &format!("[{ts}] {msg}"));
        }
    }
}

/// chunk 大小 + 持久日志的成对构造（搬移/拷贝类命令共用）
pub(crate) fn chunk_logger(a: &Args, src: &FileSource) -> (u64, Logger) {
    let chunk = movepart::chunk_bytes(a.chunk_mib).unwrap_or_else(|e| bail(EXIT_REFUSED, format!("refused: {e}")));
    (chunk, Logger::open(src))
}

/// 磁盘字节序 16 字节 → 标准文本 GUID（前 3 字段小端重排；内核 efi.h EFI_GUID 宏的逆变换）
pub(crate) fn hex_guid(b: &[u8; 16]) -> String {
    let d1 = u32::from_le_bytes(b[0..4].try_into().unwrap());
    let d2 = u16::from_le_bytes(b[4..6].try_into().unwrap());
    let d3 = u16::from_le_bytes(b[6..8].try_into().unwrap());
    format!(
        "{d1:08X}-{d2:04X}-{d3:04X}-{:02X}{:02X}-{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}",
        b[8], b[9], b[10], b[11], b[12], b[13], b[14], b[15]
    )
}

/// JSON 字符串转义：引号、反斜杠、换行/回车/制表符及全部 <0x20 控制字符（RFC 8259）
pub(crate) fn json_escape(s: &str) -> String {
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
pub(crate) fn parse_guid(s: &str) -> Option<[u8; 16]> {
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

pub(crate) fn open_target(a: &Args) -> Result<FileSource, (u8, String)> {
    FileSource::open(std::path::Path::new(&a.target), a.sector_size)
        .map_err(|e| (EXIT_INFRA, format!("open failed: {e}")))
}

/// 尽力写日志行：诊断设施失败不改变业务结论（数据与布局不受影响），故忽略。
/// 命名表达"可失败且无副作用"的意图，便于静态审计区分"有意忽略"与"忘了处理"
#[allow(clippy::let_underscore_must_use)] // 有意忽略：诊断设施失败不改变业务结论
fn best_effort_log_write(f: &mut std::fs::File, line: &str) {
    use std::io::Write;
    let _ = writeln!(f, "{line}");
}

/// undo journal 的落点由目标身份派生（见 dev::TargetIdentity）：镜像 = `<路径>.diskedit.journal`，
/// 块设备 = <state_dir()>/<设备层身份>.diskedit.journal。身份在打开目标时解析一次，
/// 关闭撤销窗口时按同一入口解析，两处不各自推导命名规则。
/// journal 文件本身要到第一条记录才落盘（Journal::open 只做只读校验），故此处的失败
/// 只可能是"落点被陌生文件占着"——真正的写入失败会在首次 write_at 处带上下文报出
pub(crate) fn open_target_for_write(a: &Args) -> Result<FileSource, (u8, String)> {
    let mut src = open_target(a)?;
    let p = src.identity.journal_path().to_path_buf();
    src.journal = Some(Journal::open(&p).map_err(|e| (EXIT_INFRA, format!("journal open failed: {e}")))?);
    Ok(src)
}

/// 表类写命令成功后通知内核重读分区表（BLKRRPART）。
/// 返回 false = 内核视图未跟进：盘上表已写成功 ≠ 内核 partition view 已同步，
/// 后续不得依赖旧的内核几何（实际状态以 /sys/block/<dev>/<part>/size 为准）。
/// 只探测不打印——"内核视图过期"的措辞由 Outcome::report 一处输出
#[cfg(target_os = "linux")]
pub(crate) fn kernel_resync(src: &FileSource) -> bool {
    if !src.is_block {
        return true;
    }
    crate::ioctl::blkrrpart(&src.file)
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn kernel_resync(src: &FileSource) -> bool {
    let _ = src;
    true
}

/// 布局类命令的统一收尾：内核重读分区表 → 把内核视图状态记入结果 → 报告 → 返回结果。
/// 顺序不可换：report 必须在 resync 之后，否则打不出"内核视图过期"这一项；
/// 未写盘的命令不做重读（没东西要同步）
pub(crate) fn settle_layout(mut o: crate::outcome::Outcome, src: &FileSource) -> crate::outcome::Outcome {
    if o.is_applied() && !kernel_resync(src) {
        o.mark_kernel_stale();
    }
    o.report();
    o
}

/// 表类写命令的成功收尾：内核重读 + 报告，仅当退出码为 0 时才打印成功字样
/// （内核视图过期时退 20，此时打"成功"会与退出码矛盾）
pub(crate) fn table_write_done(src: &FileSource, ok_msg: &str) -> u8 {
    let o = settle_layout(crate::outcome::Outcome::applied_with(Vec::new()), src);
    if o.exit_code() == EXIT_OK {
        println!("{ok_msg}");
    }
    o.exit_code()
}

/// 对齐单位（LBA 所属表的扇区数）。mib = 1MiB 折算成表内扇区；cyl = 255 头 × 63 扇区 =
/// 16065 扇区/柱面（BIOS INT 13h 虚拟几何，即 fdisk 的 "cylinders of 16065 * 512"）；
/// none = 不对齐。`table_ss` 必须是 **LBA 所属表的**扇区大小：GPT 条目按表头记录的 ss
/// 解释（可与容器 ss 不同，如 4Kn 表放在 512e 容器），按容器 ss 折算会把边界整体错位
pub(crate) fn align_unit(a: &Args, table_ss: u64) -> Option<u64> {
    match a.align.as_str() {
        "none" => None,
        "mib" => Some((1024 * 1024 / table_ss).max(1)),
        "cyl" => Some(16065),
        other => bail(EXIT_REFUSED, format!("invalid --align {other:?} (mib|cyl|none)")),
    }
}

/// 对齐（选项名同 parted --align）：start 上取整、end 下取整；对齐后区间为空即拒绝
pub(crate) fn align_range(a: &Args, start: u64, end: u64, table_ss: u64) -> (u64, u64) {
    let Some(unit) = align_unit(a, table_ss) else { return (start, end) };
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
pub(crate) fn align_start(a: &Args, start: u64, table_ss: u64) -> u64 {
    let Some(unit) = align_unit(a, table_ss) else { return start };
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
pub(crate) fn entry_byte_range(src: &FileSource, part: u32) -> Result<(u64, u64), crate::outcome::Fail> {
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
pub(crate) fn open_target_ro(a: &Args) -> Result<FileSource, (u8, String)> {
    #[cfg(target_os = "linux")]
    if let Ok(meta) = std::fs::metadata(&a.target)
        && meta.file_type().is_block_device()
    {
        return FileSource::open_read_only(std::path::Path::new(&a.target))
            .map_err(|e| (EXIT_INFRA, format!("open failed: {e}")));
    }
    open_target(a)
}

/// 计算用几何：表属"可修复的 stale"（设备扩容后备份头/PMBR 未更新，见 GptState::NeedsRepair）时
/// 按修复后的 last_usable_lba 计算；plan 不写盘，实际修复由写入路径的 ensure_geometry 完成。
/// 返回 `Fail` 而不是字符串：这里的失败全部是盘/容器自身不自洽（PMBR 越出容器、容器装不下
/// 备份数组、分区越出修复后的可用区），属"未写盘的盘内容故障"，与调用点自己那一堆校验拒绝
/// 是两回事，不能压成同一个码
pub(crate) fn effective_last_usable(src: &FileSource, g: &table::RawGpt) -> Result<u64, crate::outcome::Fail> {
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
pub(crate) fn free_right_gpt(g: &table::RawGpt, part: u32, last_usable: u64) -> u64 {
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

/// MBR 分区右侧连续空闲扇区数（到下一表项起点或盘尾为止；扩展容器起点同样
/// 构成边界——逻辑分区藏在容器内，不可侵入）。MBR 无 usable 区概念，上界=盘尾
pub(crate) fn free_right_msdos(mbr: &[table::MbrPartition], p: &table::MbrPartition, total_sectors: u64) -> u64 {
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

/// 已用区间列表 → 对齐后的空闲区间 [start,end]（含端点）；
/// 溢出用 saturating，对齐后为空的区间丢弃。
/// used/lo/hi/unit 四者必须是同一坐标系（同一种表的扇区），本函数不做单位甄别
pub(crate) fn aligned_gaps(used: &[(u64, u64)], lo: u64, hi: u64, unit: u64) -> Vec<(u64, u64)> {
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

/// 是否属于经 open_target_for_write 写入（因而会创建 undo journal）的命令。
/// mkfs 与 undo 走的是 open_target（不建 journal）：前者只重做 FS 元数据，
/// 后者以 journal 为输入、成功时自行删除——它们若也进这个集合，
/// 成功时会顺手清掉先前某次失败操作留下的 journal。
/// resize 的块设备在线路径（online::resize_online/resize_pv_online）同样不经
/// open_target_for_write，故本判定只是"可能创建"，删除动作须容忍文件不存在
pub(crate) fn is_destructive_cmd(cmd: &str) -> bool {
    matches!(cmd, "new" | "add" | "del" | "delete" | "set" | "resize" | "resize-part" | "move" | "create" | "copy" | "apply")
}

/// 成功路径删除 undo journal。删的是本次身份的全部候选落点——含历史命名那一份：
/// 留着它会被下次查找命中，把历史字节回放到一个已经改过的盘上。
/// 不存在即无残留、无告警；删除真失败则由 dev::warn_if_remove_failed 告警
pub(crate) fn drop_journal(a: &Args) {
    let Some(id) = dev::TargetIdentity::resolve_path(std::path::Path::new(&a.target)) else {
        eprintln!("warning: cannot re-resolve the target identity — the undo journal is left in place");
        return;
    };
    for p in id.journal_candidates() {
        dev::warn_if_remove_failed(p);
    }
}

#[cfg(test)]
pub(crate) fn src_from(tag: &str, data: &[u8]) -> FileSource {
    let mut tmp = std::env::temp_dir();
    tmp.push(format!("diskedit_main_{tag}_{}.img", std::process::id()));
    std::fs::write(&tmp, data).unwrap();
    let f = std::fs::OpenOptions::new().read(true).write(true).open(&tmp).unwrap();
    FileSource {
        identity: dev::TargetIdentity::resolve(&tmp, false, data.len() as u64),
        file: f,
        path: tmp,
        sector_size: 512,
        size: data.len() as u64,
        is_block: false,
        journal: None,
    }
}

#[cfg(test)]
pub(crate) fn base_args() -> Args {
    Args {
        target: String::new(), part: None, fstype: None, grow: None,
        start: None, end: None, size: None, fs: None, name: None, type_guid: None, table: None,
        yes: false, online: false, no_fs: false, sector_size: None, align: "mib".to_string(), chunk_mib: 4,
        grow_to_end: false, allow_move: false, grow_lv: false, lv: None, start_end: false, pos: Vec::new(),
        seen: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::table;

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