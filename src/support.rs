//! 命令层共用的支撑件：进程退出出口、目标打开/journal、几何与对齐助手、GUID/JSON 编解码。
//! 退出码常量的唯一定义在 outcome 模块，此处重导出给命令层，避免两套常量各自漂移

pub(crate) use crate::outcome::{Fail, EXIT_OK, EXIT_REFUSED};

use crate::args::Args;
use crate::dev::{FileSource, PartSelector};
use crate::geometry::ValidatedGeometry;
use crate::transaction::TransactionManager;
use crate::{movepart, table};

#[cfg(test)]
use crate::dev; // 只有测试夹具 src_from 用得到

/// 规划/执行失败的出口：报告文字与退出码都取自 outcome（唯一措辞与唯一映射），
/// 调用点不得自行拼装。**本模块不提供"带裸退出码的退出"**——那会绕开 Outcome::report
/// 的措辞与 exit_code 的映射，正是 10/20/30 语义分裂的入口
pub(crate) fn bail_fail(f: crate::outcome::Fail) -> ! {
    // 把失败折叠成 CLI 结果走 `Fail::into_outcome`（映射的唯一处），不借 `finish`——
    // 那是"结束一次操作"的入口，而这里没有任何要收尾的 pending
    let o = f.into_outcome();
    o.report();
    std::process::exit(o.exit_code() as i32);
}

/// 日志：落点由目标身份派生（见 `dev::TargetIdentity::log_path`）；日志只增、不参与恢复
pub(crate) struct Logger {
    file: Option<std::fs::File>,
}

impl Logger {
    /// `guid`：调用方从**已解析的几何**带来的 Disk GUID（日志命名不该反向依赖表内容——
    /// 命令层手上总有那份几何，本函数不自己读一次表）。仅块设备使用：镜像按路径命名
    pub(crate) fn open(src: &FileSource, guid: Option<[u8; 16]>) -> Self {
        let guid = if src.is_block { guid } else { None };
        let path = src.identity.log_path(guid);
        let file = std::fs::OpenOptions::new().create(true).append(true).open(path).ok();
        if file.is_none() {
            // 落盘失败不静默：用户需知日志只进 stdout
            eprintln!("warning: persistent log unavailable — output only goes to stdout");
        }
        Logger { file }
    }

    pub(crate) fn log(&mut self, msg: &str) {
        let ts = best_effort_unix_secs();
        println!("{msg}");
        if let Some(f) = &mut self.file {
            best_effort_log_write(f, &format!("[{ts}] {msg}"));
        }
    }
}

/// 持久日志的秒级时间戳只用于人读排序；时钟取不到时落 0，best-effort 语义按
/// `best_effort_*` 命名收敛（见 Cargo.toml 的 lint 门禁）
fn best_effort_unix_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// chunk 大小 + 持久日志的成对构造（搬移/拷贝类命令共用）。
/// `guid` 来自调用方已解析的几何（见 `Logger::open`）
pub(crate) fn chunk_logger(a: &Args, src: &FileSource, guid: [u8; 16]) -> (u64, Logger) {
    let chunk = movepart::chunk_bytes(a.chunk_mib).unwrap_or_else(|e| bail_fail(Fail::refused(e.to_string())));
    (chunk, Logger::open(src, Some(guid)))
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

/// MBR 分区类型字节（OSIndicator）的文本口径：0x 前缀大小写不限，其余按十六进制读。
/// `add` 与 `set type` 共用它，两处的取值与拒绝文案因此不会分叉
pub(crate) fn parse_os_type(s: &str) -> Option<u8> {
    let hex = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")).unwrap_or(s);
    u8::from_str_radix(hex, 16).ok()
}

// 目标怎么打开、所有权怎么取、事务怎么开与提交，全部归 `transaction::TransactionManager`。
// 这里只给命令层惯用的入口名，并把"哪一类命令可以接着做未完成的作业"这一条规则写成闭包

/// 会留下 durable history 的写事务（add/del/set/resize/create/mkfs/apply…）
pub(crate) fn open_target_for_write(a: &Args) -> Result<FileSource, crate::outcome::Fail> {
    TransactionManager::begin(a)
}

/// 数据搬移类命令的打开（resize-part / move）：目标上已有 checkpoint 时以显式续跑
/// 进入同一事务，否则开新事务。返回是否按续跑进入。
///
/// 接受态是"存在任意 checkpoint"（[`ResumeClaim::AnyCheckpoint`]），不细分是哪个分区
/// ——这些命令本就把 ckpt 交给 `movepart::resize_part` / `copy_part` 比对，是否属于
/// 同一件事由领域层的恢复校验裁（不匹配即 `Divergent`）
pub(crate) fn open_target_for_data_move(
    a: &Args,
) -> Result<(FileSource, bool), crate::outcome::Fail> {
    TransactionManager::begin_or_resume(a, ResumeClaim::AnyCheckpoint)
}

/// 搬移收尾类命令（resize / apply）的打开：判据是**锁下**的那份目标上还有没有自己
/// 分区的未收尾搬移（[`ResumeClaim::OwnRelocation`] → `movepart::relocation_ownership`），
/// 不靠打开前的只读预判。返回（目标，是否续跑）——resize 要拿后者决定 grow 分支。
///
/// 槽位被别的分区的作业占着时，领域层直接给出拒绝理由：那种情形下本命令既不能当空槽
/// （会覆盖别人的现场），也不能按别人的 ckpt 续跑，通用的 busy 文案说不出这一点
pub(crate) fn open_target_resumable(
    a: &Args,
    grow_part: PartSelector,
) -> Result<(FileSource, bool), crate::outcome::Fail> {
    TransactionManager::begin_or_resume(a, ResumeClaim::OwnRelocation(grow_part))
}

/// 只持有所有权、不建 journal 的写事务（undo / check / resizefs）
pub(crate) fn open_target_owned(a: &Args) -> Result<FileSource, crate::outcome::Fail> {
    TransactionManager::begin_without_history(a)
}

/// 尽力写日志行：诊断设施失败不改变业务结论（数据与布局不受影响），故忽略。
/// 命名表达"可失败且无副作用"的意图，便于静态审计区分"有意忽略"与"忘了处理"
#[allow(clippy::let_underscore_must_use)] // 有意忽略：诊断设施失败不改变业务结论
fn best_effort_log_write(f: &mut std::fs::File, line: &str) {
    use std::io::Write;
    let _ = writeln!(f, "{line}");
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
    // 本层只取"过期与否"：stale 的用户文案是固定句（见 Outcome::report），不携带 errno
    crate::ioctl::blkrrpart(&src.file).is_ok()
}

/// 非 Linux 存根：返回值**不是**"重读成功"的事实——非 Linux 目标没有内核分区视图
/// 可同步，`true` 只表示"没有可过期之物"，settle_layout 因此跳过 stale 标记。
/// 把它读成"内核已重读"是错的；支持新平台时这里须按该平台的重读机制重新实现
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

/// 表类写命令的成功收尾：内核重读 + 报告，仅当后置条件全部满足时才打印成功字样
/// （内核视图过期时退 20，此时打"成功"会与退出码矛盾）
pub(crate) fn table_write_done(src: &FileSource, ok_msg: &str) -> u8 {
    let o = settle_layout(crate::outcome::Outcome::applied_with(Vec::new()), src);
    // 用语义判断而不是比退出码：比数字会把"部分完成"误判成成功，而且数字的含义
    // 只在 outcome 一处解释得清
    if o.is_complete() {
        println!("{ok_msg}");
    }
    o.exit_code()
}

/// 1 MiB 折算成表内扇区的唯一落点：不足一扇区时取 1，调用点共用，
/// 不各自写 `(1024 * 1024 / ss).max(1)` 第二遍
pub(crate) fn mib_in_sectors(ss: u64) -> u64 {
    (1024 * 1024 / ss).max(1)
}

/// 对齐单位（LBA 所属表的扇区数）。mib = 1MiB 折算成表内扇区；cyl = 255 头 × 63 扇区 =
/// 16065 扇区/柱面（BIOS INT 13h 虚拟几何，即 fdisk 的 "cylinders of 16065 * 512"）；
/// none = 不对齐。`table_ss` 必须是 **LBA 所属表的**扇区大小：GPT 条目按表头记录的 ss
/// 解释（可与容器 ss 不同，如 4Kn 表放在 512e 容器），按容器 ss 折算会把边界整体错位
pub(crate) fn align_unit(a: &Args, table_ss: u64) -> Option<u64> {
    match a.align.as_str() {
        "none" => None,
        "mib" => Some(mib_in_sectors(table_ss)),
        "cyl" => Some(16065),
        other => bail_fail(Fail::refused(format!("invalid --align {other:?} (mib|cyl|none)"))),
    }
}

/// 对齐（选项名同 parted --align）：start 上取整、end 下取整；对齐后区间为空即拒绝。
/// 乘法全程 checked：--start 由 CLI 原始输入落 u64，s 逼近 u64::MAX 时
/// `div_ceil * unit` 会静默回绕，把无法对齐的区间伪造成从 0 起的伪区间
/// （与 aligned_gaps 的 s 侧同一防线）
pub(crate) fn align_range(a: &Args, start: u64, end: u64, table_ss: u64) -> (u64, u64) {
    let Some(unit) = align_unit(a, table_ss) else { return (start, end) };
    let s = start.div_ceil(unit).checked_mul(unit)
        .unwrap_or_else(|| bail_fail(Fail::refused(format!("start {start} overflows the LBA range when aligned to {}", a.align))));
    let e1 = end.saturating_add(1) / unit * unit;
    if e1 == 0 || s > e1 - 1 {
        bail_fail(Fail::refused(format!("range {start}..{end} is empty after {} alignment", a.align)));
    }
    if s != start || e1 - 1 != end {
        eprintln!("aligned to {}: {start}..{end} -> {s}..{}", a.align, e1 - 1);
    }
    (s, e1 - 1)
}

/// 起点上取整（copy 的 --start 只有起点语义，长度继承源分区）。checked 同 align_range
pub(crate) fn align_start(a: &Args, start: u64, table_ss: u64) -> u64 {
    let Some(unit) = align_unit(a, table_ss) else { return start };
    let s = start.div_ceil(unit).checked_mul(unit)
        .unwrap_or_else(|| bail_fail(Fail::refused(format!("start {start} overflows the LBA range when aligned to {}", a.align))));
    if s != start {
        eprintln!("aligned to {}: {start} -> {s}", a.align);
    }
    s
}

/// 只读命令的打开（info / plan / resize 的只读阶段）：不取所有权
pub(crate) fn open_target_ro(a: &Args) -> Result<FileSource, crate::outcome::Fail> {
    TransactionManager::read_only(a)
}

/// 恢复现场的枚举口径与"未收尾就拒绝"的闸口都在 `transaction`（见 `TransactionManager`）。
/// 这里只转发名字，命令层不必知道它是怎么判的
pub(crate) use crate::transaction::{
    active_recovery_records, legacy_disk_guid, RecoveryRecord, ResumeClaim,
};

/// 闸口：目标上还留着未收尾的现场 ⇒ 拒绝这次不写 journal 的写盘
pub(crate) fn refuse_if_pending_recovery(src: &FileSource, what: &str) -> Result<(), Fail> {
    TransactionManager::refuse_if_active(src, what)
}

/// 目标分区右侧连续空闲扇区数（到下一分区起点或可用区上界为止）。
///
/// 前提由参数类型给出：入参是 **已验证几何**（[`ValidatedGeometry`]）——条目互不重叠且都落在
/// 修复后的可用区内，因此"起点大于本分区末端"确实等价于"在本分区右侧"。可用区上界取自几何
/// 本身（修复后将生效的值），不再作为参数传入：同一个事实有两个来源时，两者会分叉
/// （历史实现即如此：调用点各自算一次有效上界，本函数只对"起点在右"的表项取最小）
pub(crate) fn free_right_gpt(g: &ValidatedGeometry, part: u32) -> u64 {
    let Some(e) = g.entry_index(part).and_then(|i| g.entries.get(i)) else {
        return 0;
    };
    let mut bound = g.last_usable_lba().saturating_add(1); // 排他上界（饱和防损坏几何下 +1 回绕成 0）
    for (i, o) in g.entries.iter().enumerate() {
        if (i + 1) as u32 == part || (o.starting_lba == 0 && o.ending_lba == 0) {
            continue;
        }
        if o.starting_lba > e.ending_lba {
            bound = bound.min(o.starting_lba);
        }
    }
    // +1 走 checked：ending_lba 虽被几何构造校验拦在 usable 内，这里的回绕仍不可信——
    // 回绕成 0 会把右侧空闲虚报成整段 bound
    e.ending_lba.checked_add(1).map_or(0, |next| bound.saturating_sub(next))
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
            // 对齐乘法躲着回绕：s 逼近 u64::MAX 时 div_ceil*unit 会静默回绕，
            // 把一个"无法对齐"的区间伪造成从 0 起的伪空闲。e 侧下面已有 checked，
            // s 侧同一防线
            let s2 = s.div_ceil(unit).checked_mul(unit)?;
            let e2 = (e.saturating_add(1) / unit * unit).checked_sub(1)?;
            (s2 <= e2).then_some((s2, e2))
        })
        .collect()
}

/// 成功路径提交事务：关闭它的 active 状态（见 `TransactionManager::commit`）
pub(crate) fn drop_journal(a: &Args) {
    TransactionManager::commit(a)
}

#[cfg(test)]
pub(crate) fn src_from(tag: &str, data: &[u8]) -> FileSource {
    let mut tmp = std::env::temp_dir();
    tmp.push(format!("diskedit_main_{tag}_{}.img", std::process::id()));
    std::fs::write(&tmp, data).unwrap();
    let f = std::fs::OpenOptions::new().read(true).write(true).open(&tmp).unwrap();
    FileSource {
        identity: dev::TargetIdentity::resolve_image(&tmp),
        file: f,
        path: tmp,
        sector_size: 512,
        size: data.len() as u64,
        is_block: false,
        journal: None,
        ownership: None,
        fingerprint: Default::default(),
        loop_mapping: None,
    }
}

#[cfg(test)]
pub(crate) fn base_args() -> Args {
    Args {
        target: String::new(), part: None, grow: None,
        start: None, end: None, size: None, fs: None, name: None, type_guid: None, table: None,
        yes: false, online: false, random: false, no_fs: false, sector_size: None, align: "mib".to_string(), chunk_mib: 4,
        grow_to_end: false, allow_move: false, grow_lv: false, lv: None, start_end: false, pos: Vec::new(),
        seen: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::table;

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
                header_size: 92,
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

    /// 已验证几何：由解析事实构造（与生产同一构造点）。free_right 只接受它，因此测试
    /// 无法再构造"条目重叠 / 越界"的几何——那正是该类型要排除的状态
    fn validated(on_disk_last_usable: u64, effective_last_usable: u64, ents: &[(u64, u64)]) -> ValidatedGeometry {
        let g = raw_gpt(on_disk_last_usable, ents);
        ValidatedGeometry::new(&g, effective_last_usable + 1, Some(effective_last_usable)).unwrap()
    }

    /// 右侧连续空闲：取"下一个分区起点"与"可用区上界+1"的较小者
    #[test]
    fn free_right_bounds() {
        // 分区 1 右侧紧邻分区 2 → 无空闲
        let g = validated(1000, 1000, &[(100, 199), (200, 299), (0, 0)]);
        assert_eq!(free_right_gpt(&g, 1), 0);
        assert_eq!(free_right_gpt(&g, 2), 1000 + 1 - 300);
        // 右侧隔着空隙 → 以邻分区起点为界
        let g = validated(1000, 1000, &[(100, 199), (300, 399), (0, 0)]);
        assert_eq!(free_right_gpt(&g, 1), 300 - 200);
        assert_eq!(free_right_gpt(&g, 2), 1000 + 1 - 400);
        // 左侧分区不计入（起点小于本分区末端的都被忽略）
        let g = validated(1000, 1000, &[(50, 99), (100, 199), (0, 0)]);
        assert_eq!(free_right_gpt(&g, 2), 1000 + 1 - 200);
        // 上界取自几何本身（修复后将生效的值）：盘上表头还是旧值（500）时，几何携带的是
        // 修复后的 1000，于是扩容新增的空间可见——这正是"上界只有一个来源"的意思
        let stale = validated(500, 1000, &[(100, 199), (0, 0), (0, 0)]);
        assert_eq!(free_right_gpt(&stale, 1), 1000 + 1 - 200);
        // 分区号越界：几何给出上界，越界即 0，不再直接索引（历史实现会 panic）
        assert_eq!(free_right_gpt(&stale, 128), 0);
        assert_eq!(free_right_gpt(&stale, 129), 0);
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