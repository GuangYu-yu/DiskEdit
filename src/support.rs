//! 命令层共用的支撑件：进程退出出口、目标打开/记录、几何与对齐助手、GUID/JSON 编解码。
//! 退出码常量的唯一定义在 outcome 模块，此处重导出给命令层，避免两套常量各自漂移

pub(crate) use crate::outcome::{RunOutcome, EXIT_OK};

use crate::cli::{ChunkRequest, TargetRequest};
use crate::dev::{FileSource, WriteSource};
use crate::geometry::ValidatedGeometry;
use crate::recovery::TransactionManager;
use crate::{movepart, table};

#[cfg(test)]
use crate::dev; // 只有测试夹具 src_from 用得到

/// 规划/执行失败的出口：先按本次写事实归责，报告文字与退出码都取自 outcome
/// （唯一措辞与唯一映射），调用点不得自行拼装。**本模块不提供"带裸退出码的退出"**——
/// 那会绕开 RunOutcome::report 的措辞与 exit_code 的映射，正是 10/20/30 语义分裂的入口
pub(crate) fn bail_fail(f: crate::outcome::RunOutcome) -> ! {
    // 报告文字与退出码都取自 outcome，不借 `finish`——那是"结束一次操作"的入口，
    // 而这里没有任何要收尾的 pending
    let f = f.reconcile_with_writes();
    f.report();
    std::process::exit(f.exit_code() as i32);
}

/// 持久日志的单文件上限：打开时已有日志达到这个长度就轮转一代
const LOG_MAX_BYTES: u64 = 8 * 1024 * 1024;

/// 轮转一代：已有日志达到上限时整份改名为同名加 `.1`，原有的 `.1` 被覆盖。
/// 打开点都在取得目标锁之后，同一目标不存在第二个进程同时轮转
fn rotate_log_if_oversized(path: &std::path::Path) {
    let oversized = std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.len() >= LOG_MAX_BYTES);
    if !oversized {
        return;
    }
    let mut name = path.as_os_str().to_os_string();
    name.push(".1");
    let rotated = std::path::PathBuf::from(name);
    if std::fs::rename(path, &rotated).is_ok() {
        return;
    }
    // Windows 的 rename 不覆盖已存在的目标：先删旧代再改名。锁内无并发轮转
    if let Err(e) = std::fs::remove_file(&rotated)
        && e.kind() != std::io::ErrorKind::NotFound
    {
        eprintln!("warning: persistent log rotation failed ({e}) — appending to the existing log");
        return;
    }
    if let Err(e) = std::fs::rename(path, &rotated) {
        eprintln!("warning: persistent log rotation failed ({e}) — appending to the existing log");
    }
}

/// 日志：落点由目标身份派生（见 `dev::TargetIdentity::log_path`）；单个文件内只追加、
/// 打开时按 [`LOG_MAX_BYTES`] 轮转一代，不参与恢复
pub(crate) struct Logger {
    file: Option<std::fs::File>,
}

impl Logger {
    /// `guid`：调用方从**已解析的几何**带来的 Disk GUID（日志命名不该反向依赖表内容——
    /// 命令层手上总有那份几何，本函数不自己读一次表）。仅块设备使用：镜像按路径命名
    pub(crate) fn open(src: &FileSource, guid: Option<[u8; 16]>) -> Self {
        let guid = if src.is_block { guid } else { None };
        let path = src.identity.log_path(guid);
        rotate_log_if_oversized(&path);
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
pub(crate) fn chunk_logger(a: &impl ChunkRequest, src: &FileSource, guid: [u8; 16]) -> (u64, Logger) {
    let chunk = movepart::chunk_bytes(a.chunk_mib()).unwrap_or_else(|e| bail_fail(RunOutcome::refused(e.to_string())));
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

/// 小写十六进制（无分隔）：记录标识、场景标识与 magic 的文本形态。
/// 用 nibble 直接拼，没有可丢弃的返回值（crate 的 `let_underscore_must_use = "deny"`）
pub(crate) fn hex_lower(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(char::from_digit(u32::from(byte >> 4), 16).expect("a nibble is a hex digit"));
        out.push(char::from_digit(u32::from(byte & 0x0f), 16).expect("a nibble is a hex digit"));
    }
    out
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

// 目标怎么打开、所有权怎么取、事务怎么开与提交，全部归 `recovery::TransactionManager`。
// 这里只给命令层惯用的入口名，并把"哪一类命令可以接着做未完成的作业"这一条规则写成闭包

/// 表类写命令（`new` / `add` / `delete` / `copy` / `set` / `mkfs`）的打开：写路径挂
/// 单文件恢复记录，不建旧 journal
pub(crate) fn open_table_record(
    a: &impl TargetRequest,
    op: crate::recovery::OpKind,
) -> Result<WriteSource, crate::outcome::RunOutcome> {
    TransactionManager::begin_table_record(a, op)
}

/// `resize` 的写入打开：判据是**锁下**那份目标上还有什么现场，不靠打开前的只读预判。
/// 返回进入现场（[`ResizeEntry`]）。单分区 resize 与搬移作业共用同一份记录
pub(crate) fn open_target_resize(
    a: &impl TargetRequest,
    allow_dm_held: bool,
) -> Result<(WriteSource, crate::recovery::ResizeEntry), crate::outcome::RunOutcome> {
    TransactionManager::begin_resize(a, allow_dm_held)
}

/// 搬移类命令（`move`）的记录型打开：落点上是本命令的完整记录时按续跑进入。
/// 返回是否按续跑进入——领域层仍要自己按盘面与请求核对，这里只负责"用哪份现场"
pub(crate) fn open_record_resumable(
    a: &impl TargetRequest,
    op: crate::recovery::OpKind,
) -> Result<(WriteSource, bool), crate::outcome::RunOutcome> {
    TransactionManager::begin_table_record_resume(a, op)
}

/// 只持有所有权、不建记录的写事务（undo / check）
pub(crate) fn open_target_owned(a: &impl TargetRequest) -> Result<WriteSource, crate::outcome::RunOutcome> {
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
/// 只探测不打印——"内核视图过期"的措辞由 RunOutcome::report 一处输出
#[cfg(target_os = "linux")]
pub(crate) fn kernel_resync(src: &FileSource) -> bool {
    if !src.is_block {
        return true;
    }
    // 本层只取"过期与否"：stale 的用户文案是固定句（见 RunOutcome::report），不携带 errno
    src.reread_partitions().is_ok()
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
/// 未写盘的命令不做重读（没东西要同步）。`record` 是本次仍持有的恢复记录，没有记录时
/// 传 `None`——判据是"本次已应用，或记录里留着写入证据"
pub(crate) fn settle_layout(
    mut o: crate::outcome::RunOutcome,
    src: &FileSource,
    record: Option<&crate::recovery::Record>,
) -> crate::outcome::RunOutcome {
    if o.disk_may_have_changed(record) && !kernel_resync(src) {
        o.mark_kernel_stale();
    }
    // 归责必须在打印之前：改判后打出的变体与调用方拿到的退出码同源
    o.report_final()
}

/// 表类写命令的成功收尾：内核重读 + 报告，仅当后置条件全部满足时才打印成功字样
/// （内核视图过期时退 5，此时打"成功"会与退出码矛盾）。返回结果而不压成码：
/// "已应用但待办非空"与"仅内核视图过期"对撤销窗口的去留结论不同
pub(crate) fn table_write_done(
    src: &FileSource,
    record: Option<&crate::recovery::Record>,
    ok_msg: &str,
) -> crate::outcome::RunOutcome {
    let o = settle_layout(crate::outcome::RunOutcome::applied_with(Vec::new()), src, record);
    // 用语义判断而不是比退出码：比数字会把"部分完成"误判成成功，而且数字的含义
    // 只在 outcome 一处解释得清
    if o.is_complete() {
        println!("{ok_msg}");
    }
    o
}

/// 搬移类命令的收尾：结果完整才提交并删除记录，否则把记录留在落点上当下一次续跑点。
///
/// 与 [`table_record_done`] 的区别只在这一处判定：`add` / `delete` 的待办只可能是
/// 内核重读，记录删了也不影响续跑；搬移的待办（FS 收尾、工具步骤）还没做完，
/// 删掉记录就再也不知道搬到哪、搬到第几块了
pub(crate) fn table_record_done_if_complete(
    src: &mut WriteSource,
    o: crate::outcome::RunOutcome,
) -> crate::outcome::RunOutcome {
    let o = if o.is_complete() {
        match src.take_record_journal() {
            Some(journal) => match journal.commit_and_discard() {
                Ok(()) => o,
                Err(e) => o.degrade_partial(format!(
                    "the recovery record was not cleaned up ({e}) — release it with `diskedit abandon`"
                )),
            },
            None => o,
        }
    } else {
        o
    };
    let record = src.record_journal().map(|j| j.record());
    settle_layout(o, src, record)
}

/// 挂单文件记录的写命令的成功收尾：先提交并删除记录，再报告与内核重读。
///
/// 记录没删除成功时本次运行仍有待办（下一轮会把目标顶成 busy），结果据此降级，
/// 成功字样不打印
pub(crate) fn table_record_done(src: &mut WriteSource, ok_msg: &str) -> crate::outcome::RunOutcome {
    let mut o = crate::outcome::RunOutcome::applied_with(Vec::new());
    if let Some(journal) = src.take_record_journal()
        && let Err(e) = journal.commit_and_discard()
    {
        o = o.degrade_partial(format!(
            "the recovery record was not cleaned up ({e}) — release it with `diskedit abandon`"
        ));
    }
    let record = src.record_journal().map(|j| j.record());
    let o = settle_layout(o, src, record);
    if o.is_complete() {
        println!("{ok_msg}");
    }
    o
}

/// 已应用结果的成功措辞：**整体成功**文案只在 [`crate::outcome::RunOutcome::is_complete`] 为真时
/// 打印，仍有待办时改报"这一步已完成"——两者共用同一句会让退出码 5 与文案矛盾。
/// 只发措辞、不报告：待办仍由调用方的 `report()` 呈现
pub(crate) fn announce_applied<F, G>(o: &crate::outcome::RunOutcome, ok_msg: F, step_msg: G)
where
    F: FnOnce() -> String,
    G: FnOnce() -> String,
{
    if o.is_complete() {
        println!("{}", ok_msg());
    } else {
        println!("{}", step_msg());
    }
}

/// 1 MiB 折算成表内扇区的唯一落点：不足一扇区时取 1，调用点共用，
/// 不各自写 `(1024 * 1024 / ss).max(1)` 第二遍。`ss` 为 0 时没有有意义的折算，
/// 直接取 1——本函数是折算助手而非校验点，不该成为除零的崩溃点
pub(crate) fn mib_in_sectors(ss: u64) -> u64 {
    (1024u64 * 1024).checked_div(ss).unwrap_or(0).max(1)
}

/// 对齐单位（LBA 所属表的扇区数）。mib = 1MiB 折算成表内扇区；none = 不对齐。
/// `table_ss` 必须是 **LBA 所属表的**扇区大小：GPT 条目按表头记录的 ss
/// 解释（可与容器 ss 不同，如 4Kn 表放在 512e 容器），按容器 ss 折算会把边界整体错位
pub(crate) fn align_unit(align: crate::domain::Align, table_ss: u64) -> Option<u64> {
    align.unit_sectors(table_ss)
}

/// 对齐上取整（乘法全程 checked，s 逼近 u64::MAX 时回绕会伪造从 0 起的伪区间）。
/// 失败策略（拒绝 / 丢区间）由调用点决定，这里只给 `Option`
fn ceil_align(v: u64, unit: u64) -> Option<u64> {
    v.div_ceil(unit).checked_mul(unit)
}

/// 对齐下取整（end 侧：饱和防 +1 回绕，再截到 unit 倍数）
fn floor_align(v: u64, unit: u64) -> u64 {
    v.saturating_add(1) / unit * unit
}

/// 只对末端下取整（resize-part 不改起点）：起点按下取整后的末端校验区间非空
pub(crate) fn align_end(align: crate::domain::Align, start: u64, end: u64, table_ss: u64) -> u64 {
    let Some(unit) = align_unit(align, table_ss) else { return end };
    let e1 = floor_align(end, unit);
    if e1 == 0 || start > e1 - 1 {
        bail_fail(RunOutcome::refused(format!("range {start}..{end} is empty after {align} alignment")));
    }
    if e1 - 1 != end {
        eprintln!("aligned to {align}: end {end} -> {}", e1 - 1);
    }
    e1 - 1
}

/// 起点上取整（copy 的 --start 只有起点语义，长度继承源分区）。checked 同 align_range
pub(crate) fn align_start(align: crate::domain::Align, start: u64, table_ss: u64) -> u64 {
    let Some(unit) = align_unit(align, table_ss) else { return start };
    let s = ceil_align(start, unit)
        .unwrap_or_else(|| bail_fail(RunOutcome::refused(format!("start {start} overflows the LBA range when aligned to {align}"))));
    if s != start {
        eprintln!("aligned to {align}: {start} -> {s}");
    }
    s
}

/// 只读命令的打开（info / plan / resize 的只读阶段）：不取所有权
pub(crate) fn open_target_ro(a: &impl TargetRequest) -> Result<FileSource, crate::outcome::RunOutcome> {
    TransactionManager::read_only(a)
}

/// 建表 / 回放类命令的载体必须是整盘：目标是分区节点时拒绝（判据见
/// `dev::target_is_partition_node`，镜像恒通过）。`new`/`add`/`create` 写分区表，
/// `undo`/`abandon` 回放 / 释放的是整盘的恢复现场——落到分区节点上要么覆盖该分区
/// 数据、要么把整盘偏移的字节写进分区
pub(crate) fn refuse_partition_node_target(cmd: &str, target: &str) {
    if crate::dev::target_is_partition_node(std::path::Path::new(target)) {
        bail_fail(RunOutcome::refused(format!(
            "target is a partition, not a whole disk — `{cmd}` must be given the whole device instead of {target}"
        )));
    }
}

/// GPT 分区名的落盘字段只有 36 个 UTF-16 码元（gptman PartitionName），超长会被
/// From<&str> 静默截断——名字是用户对齐条目的锚点，悄悄少一截比拒绝更糟。按
/// encode_utf16 预判码元数（不是 char 数：BMP 外字符占 2 码元）
pub(crate) fn check_gpt_name_len(name: &str) {
    let units = name.encode_utf16().count();
    if units > 36 {
        bail_fail(RunOutcome::refused(format!(
            "name is {units} UTF-16 code units; a GPT partition name holds at most 36 — refusing rather than truncating"
        )));
    }
}

/// SIZE 换算的下取整让落盘值与请求值不同，必须可见：回显实际生效的字节数。
/// `action` 是该处命令的动词（creating / resizing）
pub(crate) fn note_sector_truncation(action: &str, bytes: u64, ss: u64) {
    // `ss` 为 0 时"整扇区"无从谈起：取模会 panic，而本函数只负责提示，不该成为崩溃点
    if ss == 0 {
        return;
    }
    if !bytes.is_multiple_of(ss) {
        eprintln!("note: size {bytes} is not a multiple of the {ss}-byte sector — {action} {} bytes", bytes / ss * ss);
    }
}

/// 恢复现场的枚举口径与"未收尾就拒绝"的闸口都在 `recovery`（见 `TransactionManager`）。
/// 这里只转发名字，命令层不必知道它是怎么判的
pub(crate) use crate::recovery::{active_recovery_records, RecoveryRecord};

/// 在线写盘前声明它的代价：写经系统工具（sfdisk / 各 FS 官方工具），不经过本工具的
/// 恢复记录。中断后既不能 `undo` 回滚，也不能重跑续跑。
/// 打印点放在首次写盘之前而非命令帮助末尾：用户此刻能否改道（先卸载走离线路径）
/// 取决于是否知道这条事实
#[cfg(target_os = "linux")]
pub(crate) fn note_online_no_undo() {
    eprintln!(
        "note: online write goes through the system tools, not this tool's recovery record — nothing to resume \
         from, and `undo` cannot roll it back (unmount the partition to take the offline path)"
    );
}

/// 闸口：目标上还留着未收尾的现场 ⇒ 拒绝这次不建记录的写盘
pub(crate) fn refuse_if_pending_recovery(src: &FileSource, what: &str) -> Result<(), RunOutcome> {
    TransactionManager::refuse_if_active(src, what)
}

/// 目标分区右侧连续空闲扇区数（到下一分区起点或可用区上界为止）。
///
/// 前提由参数类型给出：入参是 **已验证几何**（[`ValidatedGeometry`]）——条目互不重叠且都落在
/// 修复后的可用区内，因此"起点大于本分区末端"确实等价于"在本分区右侧"。可用区上界取自几何
/// 本身（修复后将生效的值）：同一个事实有两个来源时，两者会分叉
pub(crate) fn free_right_gpt(g: &ValidatedGeometry, part: u32) -> u64 {
    let Some(e) = g.entry_index(part).and_then(|i| g.entries().get(i)) else {
        return 0;
    };
    let mut bound = g.last_usable_lba().saturating_add(1); // 排他上界（饱和防损坏几何下 +1 回绕成 0）
    for (i, o) in g.entries().iter().enumerate() {
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

/// MBR 表项实际占用的区间 [起点, 终点]（含端点）。容器项（扩展分区）的 EBR 链不被解析，
/// 链的实际跨度可能越过容器自述尺寸（外部工具追加到容器末端之后），故容器起点之后
/// 整体按占用计到盘尾——宁可少给空闲，也不把逻辑分区所在的扇区判成可分配
pub(crate) fn mbr_occupied(mbr: &[table::MbrPartition], total_sectors: u64) -> Vec<(u64, u64)> {
    mbr.iter()
        .map(|p| {
            let last = if p.is_container {
                total_sectors.saturating_sub(1)
            } else {
                (p.start_lba as u64 + p.size_lba as u64).saturating_sub(1)
            };
            (p.start_lba as u64, last)
        })
        .collect()
}

/// MBR 分区右侧连续空闲扇区数（到下一表项起点或盘尾为止）。扩展容器项构成边界：
/// 起点在本分区右侧时以容器起点为界；起点落本分区左侧时，链可能越出容器自述末端
/// 进入右侧，右侧空闲整体按 0 处理。MBR 无 usable 区概念，上界=盘尾
pub(crate) fn free_right_msdos(mbr: &[table::MbrPartition], p: &table::MbrPartition, total_sectors: u64) -> u64 {
    let end = p.start_lba as u64 + p.size_lba as u64; // 排他
    let mut bound = total_sectors;
    for (o, (o_start, o_end)) in mbr.iter().zip(mbr_occupied(mbr, total_sectors)) {
        if o.num == p.num {
            continue;
        }
        // 占用区间伸到本分区末端之后：其起点即右侧上界（起点 ≤ 本分区末端时上界落空）
        if o_end >= end {
            bound = bound.min(o_start);
        }
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
            let s2 = ceil_align(s, unit)?;
            let e2 = floor_align(e, unit).checked_sub(1)?;
            (s2 <= e2).then_some((s2, e2))
        })
        .collect()
}

/// checked 换算：表项 LBA 来自盘上内容，回绕的字节值会骗过下游的容量判据——溢出按表损坏报
pub(crate) fn lba_bytes(n_lba: u64, ss: u64) -> u64 {
    n_lba.checked_mul(ss)
        .unwrap_or_else(|| bail_fail(RunOutcome::no_mutation("LBA × sector-size overflows byte range (corrupted table)")))
}

/// LBA 区间 [start, end]（含两端）的字节数，两处换算都走 checked
pub(crate) fn lba_range_bytes(start: u64, end: u64, ss: u64) -> u64 {
    let n = end.checked_sub(start).and_then(|d| d.checked_add(1))
        .unwrap_or_else(|| bail_fail(RunOutcome::no_mutation("partition end below start (corrupted table)")));
    lba_bytes(n, ss)
}

/// 一段盘面字节的 sha256 全值：按 1 MiB 分段读入逐段喂给摘要，输入只有字节本身，
/// 不拼分区号、长度或扇区大小。读取失败原样上抛，由调用点按"有没有写过目标"落码
pub(crate) fn range_sha256(src: &FileSource, offset: u64, len: u64) -> std::io::Result<[u8; 32]> {
    use sha2::{Digest, Sha256};
    const CHUNK: u64 = 1 << 20;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; CHUNK as usize];
    let mut done = 0u64;
    while done < len {
        let take = (len - done).min(CHUNK) as usize;
        let at = offset.checked_add(done).ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, "range digest offset overflows")
        })?;
        src.read_at(at, &mut buf[..take])?;
        hasher.update(&buf[..take]);
        done += take as u64;
    }
    let mut out = [0u8; 32];
    out.copy_from_slice(&hasher.finalize());
    Ok(out)
}

#[cfg(test)]
pub(crate) fn src_from(tag: &str, data: &[u8]) -> crate::testdir::TestSource<WriteSource> {
    let dir = crate::testdir::test_dir(tag);
    let path = dir.join("main.img");
    std::fs::write(&path, data).unwrap();
    let f = std::fs::OpenOptions::new().read(true).write(true).open(&path).unwrap();
    let fingerprint = dev::TargetFingerprint::from_file(&f).unwrap();
    let src = WriteSource::unowned(FileSource {
        identity: dev::TargetIdentity::resolve_image(&path),
        file: f,
        path,
        sector_size: 512,
        size: fingerprint.size,
        is_block: false,
        record_journal: None,
        fingerprint,
        loop_mapping: None,
        owner: None,
    });
    crate::testdir::TestSource::new(dir, src)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::table;

    /// 打开时轮转超限日志：旧内容整份进 `.1`，新文件从 0 开始；再次超限时覆盖已有的
    /// `.1`（不是接在它后面），新日志同样从 0 开始
    #[test]
    fn an_oversized_log_rotates_to_one_generation_on_open() {
        let src = src_from("log_rotate", b"");
        let log = src.identity.log_path(None);
        let mut rotated_name = log.as_os_str().to_os_string();
        rotated_name.push(".1");
        let rotated = std::path::PathBuf::from(rotated_name);

        let first_generation = vec![b'a'; (LOG_MAX_BYTES + 1) as usize];
        std::fs::write(&log, &first_generation).unwrap();
        let mut logger = Logger::open(&src, None);
        assert_eq!(std::fs::metadata(&log).unwrap().len(), 0, "the new log starts at 0");
        assert_eq!(std::fs::read(&rotated).unwrap(), first_generation, "the old log moves to .1 intact");
        logger.log("first run");
        let appended = std::fs::read_to_string(&log).unwrap();
        assert!(appended.contains("first run"), "the new log takes appends after rotation: {appended:?}");
        drop(logger);

        let second_generation = vec![b'b'; (LOG_MAX_BYTES + 2) as usize];
        std::fs::write(&log, &second_generation).unwrap();
        let mut logger = Logger::open(&src, None);
        assert_eq!(std::fs::metadata(&log).unwrap().len(), 0, "the next rotation starts at 0 again");
        assert_eq!(std::fs::read(&rotated).unwrap(), second_generation, ".1 is replaced, not appended to");
        logger.log("second run");
        drop(logger);
    }

    /// 扇区大小为 0 时两个助手都不得 panic：它们不是校验点，把"盘没有扇区"这件事
    /// 说成一次崩溃（101）会让调用方拿不到任何可读的分类
    #[test]
    fn sector_helpers_tolerate_zero_sector_size() {
        assert_eq!(mib_in_sectors(0), 1);
        assert_eq!(mib_in_sectors(512), 2048);
        note_sector_truncation("resizing to", 1000, 0); // 不得 panic
    }

    fn raw_gpt(last_usable: u64, ents: &[(u64, u64)]) -> table::RawGpt {
        // 头自述 128 条目，条目数组必须凑满同样的长度：ValidatedGeometry 拒绝
        // "数组长度 ≠ 头自述条目数"的表
        let mut entries: Vec<_> = ents.iter().map(|&(s, e)| gptman::GPTPartitionEntry {
            partition_type_guid: [1; 16],
            unique_partition_guid: [2; 16],
            starting_lba: s,
            ending_lba: e,
            attribute_bits: 0,
            partition_name: "".into(),
        }).collect();
        entries.resize_with(128, || gptman::GPTPartitionEntry {
            partition_type_guid: [0; 16],
            unique_partition_guid: [0; 16],
            starting_lba: 0,
            ending_lba: 0,
            attribute_bits: 0,
            partition_name: "".into(),
        });
        table::RawGpt {
            ss: 512,
            state: table::GptState::Valid,
            pmbr: table::PmbrSize::Normal,
            header: table::RawHeader {
                revision: 0x0001_0000,
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
            entries,
        }
    }

    /// 已验证几何：由解析事实构造（与生产同一构造点）。free_right 只接受它，因此测试
    /// 无法再构造"条目重叠 / 越界"的几何——那正是该类型要排除的状态
    fn validated(on_disk_last_usable: u64, effective_last_usable: u64, ents: &[(u64, u64)]) -> ValidatedGeometry {
        let g = raw_gpt(on_disk_last_usable, ents);
        // 容器末端给备份数组（128 × 128B = 32 扇区）和备份头各留出位置：
        // effective_last_usable 本身不得侵入备份元数据
        let file_last = effective_last_usable + 33;
        ValidatedGeometry::new(&g, file_last, Some(effective_last_usable)).unwrap()
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
        // 分区号越界（表有 128 个槽位）：几何给出上界，越界即 0，不再直接索引
        assert_eq!(free_right_gpt(&stale, 129), 0);
        assert_eq!(free_right_gpt(&stale, 130), 0);
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
