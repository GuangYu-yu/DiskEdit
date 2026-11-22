//! 宽度收口守卫：外部宽度（u64/u32 等）转宿主 `usize` 只允许经过 `support::host_usize`
//! 这一个返回 `Result` 的转换点；分配长度、切片下标、条目比较不再各自裸 `as usize`。
//!
//! 扫描范围：`src/` 下全部 `.rs`，用 `guards::production_lines` 去掉 `#[cfg(test)] mod`
//! 测试模块体；`src/**/tests/` 目录与名为 `tests.rs` 的夹具文件整份排除。测试夹具里的
//! 直写不参与本守卫。
//!
//! 白名单只登记复审确认无害的裸转，每条注明类别与理由；新增分配长度/下标/比较位置的
//! 裸转必须走 `host_usize`，未登记即变红。登记身份与位置无关：键是（相对路径、去首尾
//! 空白的整行内容、该内容在本文件裸转行内的第几次出现），上方插入或删除行只移动行号、
//! 不改变命中；此外还钉住该内容在本文件裸转行里的总条数——多出一条逐字相同的行会把
//! 旧登记的次序顶走，总数不符即变红；行被删掉或改写后内容对不上，反向校验报悬空登记。
//!
//! 字面量普查覆盖不到三种形态：宏展开后才生成的 cast、`std::mem::transmute`、`as _`；
//! `transmute` 由独立零容忍断言看守，非 `use` 导入的 `as _` 现存用法单独登记。
//!
//! 同族另有失败策略普查：每个 `host_usize(` 调用点所在语句必须带 `?`、`map_err(...)`、
//! `.ok()` 或等价的 `is_ok_and(...)` / `match` 分支；`.ok()` 站点逐条登记，归类只能是
//! `保守`（失败落拒绝/不适用侧）或 `放行`（失败等价于没有值且下游另有闸），理由不得为空。

mod guards;

use guards::{
    Site, Violation, code_only, is_fixture_file, is_word_byte, number_occurrences, production_lines,
    rust_sources,
};
use std::collections::BTreeMap;

/// 白名单条目：文件 + 锚定行内容 + 出现次序 + 该内容登记总条数 + 类别 + 理由。
/// 行号不参与匹配，只在失败报告里由普查现场带出。
struct Allow {
    file: &'static str,
    line: &'static str,
    occurrence: usize,
    count: usize,
    category: &'static str,
    reason: &'static str,
}

/// 类别说明：
/// - `nibble`：u8 半字节/字节查固定表，值域 0..=255，不承载外部宽度
/// - `display`：仅格式化或错误文案取值，不作分配长度、切片下标或条目比较
/// - `fault-hook`：崩溃注入点参数；入参是循环枚举下标，值域 ≤ 已枚举条目数
/// - `clamped`：取值已被固定切片长度或常量上界夹紧，下标不越界
/// - `const`：编译期常量，值域恒 ≤ 任何支持目标的 `usize::MAX`
/// - `ffi`：函数指针/地址宽度转换，不是整数宽度收口
/// - `u32-decode`：盘上 u32 解码出的长度/计数/步数；u32 在 32/64 位目标上宽度都不超过
///   `usize`，且随后按游标余量、切片长度或几何上界再次夹紧
/// - `slot-index`：分区/槽位编号（1 起的 u32），值域已由前置校验或随后的 `get` 夹紧
/// - `geometry`：条目数组几何字段，由 `EntryArrayGeometry::new` 单点判定夹紧在上界内
/// - `bounded-sector`：逻辑扇区大小，经 `SectorSize::try_new` 校验（512..=65536）
/// - `ssize-t`：POSIX 返回的有符号长度，已先判非负且不超过缓冲长度
/// - `test-fixture`：`#[cfg(test)]` 外壳函数，只服务测试装配
/// - `test-helper`：cfg(test) 专用辅助模块，按 `usize` 长度取模后再转，值域 < len
const WHITELIST: &[Allow] = &[
    // u8 半字节查固定十六进制表
    Allow { file: "assembly.rs", line: "text.push(HEX[(byte >> 4) as usize] as char);", occurrence: 1, count: 1, category: "nibble", reason: "u8 高半字节查 HEX 表，值域 0..=15" },
    Allow { file: "assembly.rs", line: "text.push(HEX[(byte & 0x0F) as usize] as char);", occurrence: 1, count: 1, category: "nibble", reason: "u8 低半字节查 HEX 表，值域 0..=15" },

    // 进度展示文案：条目序号只进 format!，不参与分配或下标
    Allow { file: "cmd/inspect.rs", line: "let committed = reloc.entry_index as usize + usize::from(*entry == reloc.entry_index);", occurrence: 1, count: 1, category: "display", reason: "已提交条数的展示值，仅进 format!" },
    Allow { file: "cmd/inspect.rs", line: "format!(\"{}/{} committed\", reloc.entry_index as usize, total)", occurrence: 1, count: 1, category: "display", reason: "已提交条数的展示值，仅进 format!" },
    Allow { file: "cmd/inspect.rs", line: "reloc.entry_index as usize, total", occurrence: 1, count: 1, category: "display", reason: "已提交条数的展示值，仅进 format!" },

    // 分区号/条目游标：前置校验或随后的 get 把值域夹紧在条目数组内
    Allow { file: "cmd/layout.rs", line: "if i + 1 == part as usize || crate::table::is_unused(other) {", occurrence: 1, count: 1, category: "slot-index", reason: "循环枚举下标与分区号比较，part 已由槽位校验夹紧" },
    Allow { file: "cmd/layout.rs", line: "g2.update_entry((part - 1) as usize, |te| {", occurrence: 1, count: 1, category: "slot-index", reason: "分区号转条目下标，part 经前置分区校验，随后 update_entry 落槽" },
    Allow { file: "cmd/layout.rs", line: ".get(r.entry_index as usize)", occurrence: 1, count: 1, category: "slot-index", reason: "只读查询里按记录游标取条目，越界即 None" },
    Allow { file: "cmd/resize/msdos.rs", line: "let off = crate::table::mbr_entry_range(index as usize).start;", occurrence: 1, count: 1, category: "slot-index", reason: "MBR 槽位 1..=4，mbr_entry_range 内部再夹紧" },

    // 读取长度被固定超块数组长度夹紧
    Allow { file: "dev/occupancy.rs", line: "let covered = (src.size - SUPER_OFFSET).min(sb.len() as u64) as usize;", occurrence: 1, count: 1, category: "clamped", reason: "covered 取 min(差值, sb.len())，下标不超固定超块数组" },

    // 崩溃注入点参数：循环枚举下标，仅与环境变量标签比对
    Allow { file: "exec/mod.rs", line: "crate::movepart::fault_after_swap_commit(entry as usize);", occurrence: 1, count: 1, category: "fault-hook", reason: "注入点参数，entry 是循环枚举下标，不分配不下标" },
    Allow { file: "exec/mod.rs", line: "before: dispatch::FaultPoint::AfterSwapEntry(entry as usize),", occurrence: 1, count: 1, category: "fault-hook", reason: "注入点参数，entry 是循环枚举下标，不分配不下标" },
    Allow { file: "exec/mod.rs", line: "crate::movepart::fault_after_entry_commit(entry as usize);", occurrence: 1, count: 1, category: "fault-hook", reason: "注入点参数，entry 是循环枚举下标，不分配不下标" },

    // 帧上限常量与 u8 十六进制数字表
    Allow { file: "exec/protocol.rs", line: "pub(crate) const MAX_FRAME: usize = crate::support::MIB_BYTES as usize;", occurrence: 1, count: 1, category: "const", reason: "1 MiB 编译期常量，恒 ≤ 任何目标 usize::MAX" },
    Allow { file: "exec/protocol.rs", line: "out.push(DIGITS[(byte >> 4) as usize] as char);", occurrence: 1, count: 1, category: "nibble", reason: "u8 高半字节查 DIGITS 表，值域 0..=15" },
    Allow { file: "exec/protocol.rs", line: "out.push(DIGITS[(byte & 0xf) as usize] as char);", occurrence: 1, count: 1, category: "nibble", reason: "u8 低半字节查 DIGITS 表，值域 0..=15" },

    // 信号处理函数指针落地址，不是整数宽度收口
    Allow { file: "exec/remote.rs", line: "action.sa_sigaction = on_sigint as *const () as usize;", occurrence: 1, count: 1, category: "ffi", reason: "函数指针地址宽度转换，与盘上整数宽度无关" },

    // 错误文案里的块序号，不分配不下标
    Allow { file: "execution/copy.rs", line: "let total_n = total.div_ceil(chunk_bytes) as usize;", occurrence: 1, count: 1, category: "display", reason: "总块数只用于错误文案序号" },
    Allow { file: "execution/copy.rs", line: "(pos / chunk_bytes + 1) as usize,", occurrence: 1, count: 3, category: "display", reason: "当前块号只用于错误文案序号" },
    Allow { file: "execution/copy.rs", line: "(pos / chunk_bytes + 1) as usize,", occurrence: 2, count: 3, category: "display", reason: "当前块号只用于错误文案序号" },
    Allow { file: "execution/copy.rs", line: "(pos / chunk_bytes + 1) as usize,", occurrence: 3, count: 3, category: "display", reason: "当前块号只用于错误文案序号" },

    // NTFS 引导扇区探测缓冲；记录游标 take
    Allow { file: "execution/mod.rs", line: "let mut probe = vec![0u8; ss as usize];", occurrence: 1, count: 1, category: "bounded-sector", reason: "ss 是命令层校验过的逻辑扇区大小，取值 512..=65536" },
    Allow { file: "execution/mod.rs", line: "for count in counts.take(reloc.entry_index as usize) {", occurrence: 1, count: 1, category: "slot-index", reason: "已提交条目计数 take 游标，entry_index 经计划校验 ≤ 条目数" },

    // 清零写入长度被固定零缓冲夹紧
    Allow { file: "fsops/swap.rs", line: "let n = (left as usize).min(zero.len());", occurrence: 1, count: 1, category: "clamped", reason: "n 取 min(剩余, zero.len())，写入长度不超零缓冲" },

    // 几何条目数与实际切片长度比较
    Allow { file: "geometry.rs", line: "if g.entries.len() != entry_geometry.entry_count as usize {", occurrence: 1, count: 1, category: "geometry", reason: "entry_count 与 entries.len() 比较，几何构造已夹紧上界" },

    // 分区号 1 起，checked_sub 兜住 0，随后 get 夹紧
    Allow { file: "gpt_policy.rs", line: "let idx = part.checked_sub(1).map(|i| i as usize);", occurrence: 1, count: 1, category: "slot-index", reason: "part-1 经 checked_sub，随后 entries.get 越界即 None" },

    // 扩容分区号转条目下标，随后 get 夹紧并报越界
    Allow { file: "movepart/shift.rs", line: "let grow_idx = (plan.grow_part - 1) as usize;", occurrence: 1, count: 1, category: "slot-index", reason: "grow_part 经计划校验，随后 entries.get 越界即报错" },

    // POSIX 读写返回的有符号长度，已判非负且不超缓冲
    Allow { file: "platform/evidence_store.rs", line: "out.extend_from_slice(&buf[..n as usize]);", occurrence: 1, count: 1, category: "ssize-t", reason: "read 返回值先判 n >= 0，且 n ≤ buf 长度" },
    Allow { file: "platform/evidence_store.rs", line: "written += n as usize;", occurrence: 1, count: 1, category: "ssize-t", reason: "write 返回值先判 n > 0，累计已写字节数" },

    // 解码侧 u32 长度/计数/步数：u32 宽度不超 usize，再由游标余量或切片长度夹紧
    Allow { file: "recovery/record.rs", line: "let done = progress.completed_steps as usize;", occurrence: 1, count: 1, category: "u32-decode", reason: "u32 完成步数与条目长度比较，宽度不超 usize" },
    Allow { file: "recovery/record.rs", line: "let len = c.u32()? as usize;", occurrence: 1, count: 3, category: "u32-decode", reason: "u32 载荷长度，随后 c.take(len) 按游标余量夹紧" },
    Allow { file: "recovery/record.rs", line: "let restore_count = c.u32()? as usize;", occurrence: 1, count: 1, category: "u32-decode", reason: "u32 条目计数，随后按游标余量逐条解码" },
    Allow { file: "recovery/record.rs", line: "let count = c.u32()? as usize;", occurrence: 1, count: 1, category: "u32-decode", reason: "u32 条目计数，随后按游标余量逐条解码" },
    Allow { file: "recovery/record.rs", line: "let len = c.u32()? as usize;", occurrence: 2, count: 3, category: "u32-decode", reason: "u32 载荷长度，随后 c.take(len) 按游标余量夹紧" },
    Allow { file: "recovery/record.rs", line: "let len = c.u32()? as usize;", occurrence: 3, count: 3, category: "u32-decode", reason: "u32 载荷长度，随后 c.take(len) 按游标余量夹紧" },

    // 预映像解析：固定头长常量；头自述条目数/步长经 checked_mul 与数组长度复核
    Allow { file: "recovery/rollback.rs", line: "if primary.len() < crate::table::GPT_MIN_HEADER_SIZE as usize {", occurrence: 1, count: 1, category: "const", reason: "92 字节规范固定头长，编译期常量" },
    Allow { file: "recovery/rollback.rs", line: "let array_len = (header.number_of_partition_entries as usize)", occurrence: 1, count: 1, category: "u32-decode", reason: "头自述条目数，checked_mul 后再与 array.len() 复核" },
    Allow { file: "recovery/rollback.rs", line: ".checked_mul(header.size_of_partition_entry as usize)?;", occurrence: 1, count: 1, category: "u32-decode", reason: "头自述条目步长，随后与 ENTRY_FIXED_BYTES 比较" },
    Allow { file: "recovery/rollback.rs", line: "if (header.size_of_partition_entry as usize) < crate::table::ENTRY_FIXED_BYTES", occurrence: 1, count: 1, category: "u32-decode", reason: "头自述条目步长，随后与 ENTRY_FIXED_BYTES 比较" },
    Allow { file: "recovery/rollback.rs", line: "header.number_of_partition_entries as usize,", occurrence: 1, count: 1, category: "u32-decode", reason: "头自述条目数，传给条目解析器，数组长度已先行复核" },
    Allow { file: "recovery/rollback.rs", line: "header.size_of_partition_entry as usize,", occurrence: 1, count: 1, category: "u32-decode", reason: "头自述条目步长，传给条目解析器，数组长度已先行复核" },
    Allow { file: "recovery/rollback.rs", line: "if entry.original.len() < crate::table::GPT_MIN_HEADER_SIZE as usize {", occurrence: 1, count: 1, category: "const", reason: "92 字节规范固定头长，编译期常量" },

    // 摘要分段：1 MiB 常量与被它夹紧的本段读取长度
    Allow { file: "support.rs", line: "let mut buf = vec![0u8; CHUNK as usize];", occurrence: 1, count: 1, category: "const", reason: "CHUNK = 1 MiB 编译期常量" },
    Allow { file: "support.rs", line: "let take = (len - done).min(CHUNK) as usize;", occurrence: 1, count: 1, category: "clamped", reason: "本段读取长度取 min(剩余, CHUNK)，不超分段缓冲" },

    // 计划期空表装配；条目数由几何构造夹紧
    Allow { file: "table/create.rs", line: "Ok(GptContent { ss, header, entries: vec![empty_entry(); geom.entry_count as usize], last_lba })", occurrence: 1, count: 1, category: "geometry", reason: "entry_count 由 EntryArrayGeometry::new 夹紧在 MAX_ARRAY_BYTES 内" },
    Allow { file: "table/create.rs", line: "let g = RawGpt { ss, header, entries: vec![empty_entry(); geom.entry_count as usize], state: GptState::Valid, pmbr: PmbrSize::Normal, decided: None };", occurrence: 1, count: 1, category: "test-fixture", reason: "#[cfg(test)] 装配外壳，几何同样经构造点校验" },

    // 条目步长/槽位校验，u32 比较与槽位换算
    Allow { file: "table/entry_array.rs", line: "entry_size as usize >= ENTRY_FIXED_BYTES && entry_size.is_power_of_two()", occurrence: 1, count: 1, category: "u32-decode", reason: "entry_size 与固定最小步长比较，u32 宽度不超 usize" },
    Allow { file: "table/entry_array.rs", line: "let idx = part.checked_sub(1)? as usize;", occurrence: 1, count: 1, category: "slot-index", reason: "part-1 经 checked_sub，返回 Option 槽位" },
    Allow { file: "table/entry_array.rs", line: "(idx < self.entry_count as usize).then_some(idx)", occurrence: 1, count: 1, category: "geometry", reason: "槽位与 entry_count 比较，几何构造已夹紧上界" },

    // 解析侧：头字段先与最小头长、扇区长度、数组长度复核；几何由构造点夹紧
    Allow { file: "table/gpt_parse.rs", line: "let hdr_size = rd_u32(sector, offset_of!(GptHeaderLayout, header_size)) as usize;", occurrence: 1, count: 1, category: "u32-decode", reason: "u32 头长，随后与最小头长和 sector.len() 比较夹紧" },
    Allow { file: "table/gpt_parse.rs", line: "if hdr_size < GPT_MIN_HEADER_SIZE as usize || hdr_size as u64 > sector.len() as u64 {", occurrence: 1, count: 1, category: "const", reason: "92 字节规范固定头长，编译期常量" },
    Allow { file: "table/gpt_parse.rs", line: "let mut raw = vec![0u8; array_len as usize];", occurrence: 1, count: 1, category: "geometry", reason: "array_len 取自夹紧后的几何 span_bytes" },
    Allow { file: "table/gpt_parse.rs", line: "if crc32(&raw[..geom.byte_len() as usize]) != rd_u32(sec, 88) {", occurrence: 1, count: 1, category: "geometry", reason: "byte_len 取自夹紧后的几何，CRC 切片不超数组" },
    Allow { file: "table/gpt_parse.rs", line: "let mut sec = vec![0u8; ss as usize];", occurrence: 1, count: 3, category: "bounded-sector", reason: "候选扇区大小，候选集合为容器 ss 与 512/4096" },
    Allow { file: "table/gpt_parse.rs", line: "let n = header.number_of_partition_entries as usize;", occurrence: 1, count: 2, category: "u32-decode", reason: "头自述条目数，几何校验通过后才进入解析" },
    Allow { file: "table/gpt_parse.rs", line: "let es = header.size_of_partition_entry as usize;", occurrence: 1, count: 2, category: "u32-decode", reason: "头自述条目步长，几何校验通过后才进入解析" },
    Allow { file: "table/gpt_parse.rs", line: "let mut sec = vec![0u8; ss as usize];", occurrence: 2, count: 3, category: "bounded-sector", reason: "候选扇区大小，候选集合为容器 ss 与 512/4096" },
    Allow { file: "table/gpt_parse.rs", line: "let n = header.number_of_partition_entries as usize;", occurrence: 2, count: 2, category: "u32-decode", reason: "头自述条目数，几何校验通过后才进入解析" },
    Allow { file: "table/gpt_parse.rs", line: "let es = header.size_of_partition_entry as usize;", occurrence: 2, count: 2, category: "u32-decode", reason: "头自述条目步长，几何校验通过后才进入解析" },
    Allow { file: "table/gpt_parse.rs", line: "let mut sec = vec![0u8; ss as usize];", occurrence: 3, count: 3, category: "bounded-sector", reason: "候选扇区大小，候选集合为容器 ss 与 512/4096" },
    Allow { file: "table/gpt_parse.rs", line: "let mut sector = vec![0u8; ss as usize];", occurrence: 1, count: 2, category: "bounded-sector", reason: "候选扇区大小，候选集合为容器 ss 与 512/4096" },
    Allow { file: "table/gpt_parse.rs", line: "let mut sector = vec![0u8; ss as usize];", occurrence: 2, count: 2, category: "bounded-sector", reason: "候选扇区大小，候选集合为容器 ss 与 512/4096" },

    // 保护 MBR 扇区缓冲；ss 已过 SectorSize::try_new 校验
    Allow { file: "table/gpt_write.rs", line: "let mut lba0 = vec![0u8; ss as usize];", occurrence: 1, count: 1, category: "bounded-sector", reason: "ss 经 SectorSize::try_new 校验，取值 512..=65536" },

    // MBR 槽位编号 1..=4
    Allow { file: "table/mbr.rs", line: "mbr_entry_range(part as usize - 1).start", occurrence: 1, count: 1, category: "slot-index", reason: "part 经 1..=4 校验后转槽位下标" },
    Allow { file: "table/mbr.rs", line: "let rec = &lba0[mbr_entry_range(i as usize)];", occurrence: 1, count: 1, category: "slot-index", reason: "i 是 0..4 循环计数器" },
    Allow { file: "table/mbr.rs", line: "let o = &lba0[mbr_entry_range(i as usize)];", occurrence: 1, count: 1, category: "slot-index", reason: "i 是 0..4 循环计数器" },

    // 摘要读取长度被扇区大小夹紧
    Allow { file: "table/stamp.rs", line: "let len = src.size.saturating_sub(off).min(ss) as usize;", occurrence: 1, count: 1, category: "clamped", reason: "len 取 min(剩余, ss)，不超当前扇区缓冲" },

    // 候选签名偏移，随后与 lba0.len() 比较夹紧
    Allow { file: "table/step.rs", line: "let at = at as usize;", occurrence: 1, count: 1, category: "clamped", reason: "候选偏移随后 at + sig_len > lba0.len() 即 continue" },

    // 测试随机数辅助：按切片长度取模后再转，值域 < len
    Allow { file: "testrng.rs", line: "self.below(len as u64) as usize", occurrence: 1, count: 1, category: "test-helper", reason: "cfg(test) 专用，index 值域 < len" },
];

/// `as _` 通配 cast 登记：非 `use` 导入的现存用法，目标类型由 FFI 签名推断，不是整数宽度收口。
struct WildcardCast {
    file: &'static str,
    line: &'static str,
    reason: &'static str,
}

const WILDCARD_CASTS: &[WildcardCast] = &[
    WildcardCast { file: "fsops/mount.rs", line: "let rc = unsafe { libc::ioctl(f.as_raw_fd(), LOOP_GET_STATUS64 as _, &mut info) };", reason: "ioctl 常量码，目标类型由 libc 签名推断" },
];

/// 按 token 认 `as` + 任意空白 + `usize`：多空格、制表符都算，贴在标识符上的 `as usize`
/// 子串不算（如 `basename` 里的片段不参与匹配，词边界两侧都要求不是标识符字符）。
fn contains_as_usize(code: &str) -> bool {
    let b = code.as_bytes();
    let mut i = 0;
    while i + 1 < b.len() {
        if b[i] == b'a'
            && b[i + 1] == b's'
            && !i.checked_sub(1).and_then(|p| b.get(p)).is_some_and(|&p| is_word_byte(p))
            && !b.get(i + 2).is_some_and(|&n| is_word_byte(n))
        {
            let mut j = i + 2;
            while j < b.len() && b[j].is_ascii_whitespace() {
                j += 1;
            }
            if b[j..].starts_with(b"usize")
                && !b.get(j + 5).is_some_and(|&n| is_word_byte(n))
            {
                return true;
            }
        }
        i += 1;
    }
    false
}

/// 认 `as _` 通配 cast；`use ... as _;` 这类 trait 导入不是宽度转换，另行排除。
fn contains_as_wildcard(code: &str) -> bool {
    let b = code.as_bytes();
    let mut i = 0;
    while i + 1 < b.len() {
        if b[i] == b'a'
            && b[i + 1] == b's'
            && !i.checked_sub(1).and_then(|p| b.get(p)).is_some_and(|&p| is_word_byte(p))
            && !b.get(i + 2).is_some_and(|&n| is_word_byte(n))
        {
            let mut j = i + 2;
            while j < b.len() && b[j].is_ascii_whitespace() {
                j += 1;
            }
            if b.get(j) == Some(&b'_') && !b.get(j + 1).is_some_and(|&n| is_word_byte(n)) {
                return true;
            }
        }
        i += 1;
    }
    false
}

fn is_bare_usize_cast(code: &str) -> bool {
    contains_as_usize(code)
}

fn routed_through_helper(code: &str) -> bool {
    code.contains("host_usize(")
}

/// 提取文件内全部裸转生产站点，并按同内容出现次序编号（1 起）。识别看剥注释、空字面量
/// 后的代码区；站点登记内容保留原始行，锚点仍对准源码文本。
fn bare_cast_sites(text: &str) -> Vec<Site> {
    let code = code_only(text);
    let code_lines: Vec<&str> = code.lines().collect();
    number_occurrences(
        production_lines(text)
            .iter()
            .filter_map(|(n, line)| {
                let stripped = code_lines.get(n - 1).copied().unwrap_or("").trim();
                let trimmed = line.trim().to_string();
                if is_bare_usize_cast(stripped) && !routed_through_helper(stripped) {
                    Some((*n, trimmed))
                } else {
                    None
                }
            }),
    )
}

fn registry_matches(entry: &Allow, path: &str, site: &Site) -> bool {
    entry.file == path && entry.line == site.content && entry.occurrence == site.occurrence
}

/// 生产行对应的剥注释、空字面量后的代码行：(1-based 行号, 去首尾空白代码)。
fn stripped_production(text: &str) -> Vec<(usize, String)> {
    let code = code_only(text);
    let code_lines: Vec<&str> = code.lines().collect();
    production_lines(text)
        .iter()
        .map(|(n, _)| {
            let stripped = code_lines.get(n - 1).copied().unwrap_or("").trim().to_string();
            (*n, stripped)
        })
        .collect()
}

/// 非 `use` 导入的 `as _` 通配 cast 站点：(文件, 剥注释代码行, 行号)。
fn wildcard_sites(sources: &[(String, String)]) -> Vec<(String, String, usize)> {
    let mut out = Vec::new();
    for (path, text) in sources {
        if is_fixture_file(path) {
            continue;
        }
        for (n, stripped) in stripped_production(text) {
            if stripped.starts_with("use ") {
                continue;
            }
            if contains_as_wildcard(&stripped) {
                out.push((path.clone(), stripped, n));
            }
        }
    }
    out
}

/// 普查结果：未登记站点、悬空登记、总数不符三类各自列出。
struct Census {
    unregistered: Vec<Violation>,
    dangling: Vec<Violation>,
    count_mismatch: Vec<Violation>,
}

fn run_census(registry: &[Allow], sources: &[(String, String)]) -> Census {
    let mut sites_by_key: BTreeMap<(String, String), Vec<Site>> = BTreeMap::new();
    for (path, text) in sources {
        if is_fixture_file(path) {
            continue;
        }
        for site in bare_cast_sites(text) {
            sites_by_key
                .entry((path.clone(), site.content.clone()))
                .or_default()
                .push(site);
        }
    }

    let mut unregistered: Vec<Violation> = Vec::new();
    let mut matched: Vec<(String, String, usize)> = Vec::new();
    for ((path, content), sites) in &sites_by_key {
        for site in sites {
            match registry.iter().find(|a| registry_matches(a, path, site)) {
                Some(a) => {
                    assert!(
                        !a.category.is_empty() && !a.reason.is_empty(),
                        "白名单条目 {}:{} 必须带类别与理由",
                        a.file, a.line
                    );
                    matched.push((path.clone(), content.clone(), site.occurrence));
                }
                None => unregistered.push(Violation {
                    file: path.clone(),
                    line_no: site.line_no,
                    category: "unregistered",
                    content: content.clone(),
                }),
            }
        }
    }

    // 反向锁定：每条登记必须仍能在现存代码里找到对应行；内容被删/被改即报悬空登记
    let mut dangling: Vec<Violation> = Vec::new();
    for a in registry {
        if !matched
            .iter()
            .any(|(p, content, k)| p == a.file && content == a.line && *k == a.occurrence)
        {
            dangling.push(Violation {
                file: a.file.to_string(),
                line_no: 0,
                category: a.category,
                content: format!("{} — {}", a.line, a.reason),
            });
        }
    }

    // 总条数锁定：同一内容在本文件裸转行里的现存条数必须与登记总数相等；
    // 多出一条逐字相同的行会顶走旧登记的次序，少一条则登记悬空。
    let mut registered_total: BTreeMap<(&str, &str), usize> = BTreeMap::new();
    for a in registry {
        match registered_total.get(&(a.file, a.line)) {
            Some(&old) => assert_eq!(
                old, a.count,
                "登记内部不一致：{}:{} 同内容条目登记了两个不同总数",
                a.file, a.line
            ),
            None => {
                registered_total.insert((a.file, a.line), a.count);
            }
        }
    }
    let mut count_mismatch: Vec<Violation> = Vec::new();
    for ((file, line), &registered) in &registered_total {
        let actual = sites_by_key
            .get(&((*file).to_string(), (*line).to_string()))
            .map(|v| v.len())
            .unwrap_or(0);
        if registered != actual {
            count_mismatch.push(Violation {
                file: (*file).to_string(),
                line_no: 0,
                category: "count-mismatch",
                content: format!("登记总数 {registered}，现存 {actual} 条：{line}"),
            });
        }
    }

    Census { unregistered, dangling, count_mismatch }
}

#[test]
fn every_width_cast_goes_through_the_single_conversion_point() {
    let census = run_census(WHITELIST, &rust_sources());

    let mut unregistered = census.unregistered;
    unregistered.sort_by(|a, b| a.file.cmp(&b.file).then(a.line_no.cmp(&b.line_no)));
    assert!(
        unregistered.is_empty(),
        "这些生产行把外部宽度直转成 usize，未走 `host_usize` 也未登记白名单：\n{}",
        unregistered.iter().map(Violation::render).collect::<Vec<_>>().join("\n")
    );

    let mut dangling = census.dangling;
    dangling.sort_by(|a, b| a.file.cmp(&b.file).then(a.content.cmp(&b.content)));
    assert!(
        dangling.is_empty(),
        "白名单条目已无对应代码行，先复核再撤登记：\n{}",
        dangling.iter().map(Violation::render).collect::<Vec<_>>().join("\n")
    );

    let mut count_mismatch = census.count_mismatch;
    count_mismatch.sort_by(|a, b| a.file.cmp(&b.file).then(a.content.cmp(&b.content)));
    assert!(
        count_mismatch.is_empty(),
        "同内容裸转条数与登记总数不符（多出的逐字相同行不能冒用旧登记）：\n{}",
        count_mismatch.iter().map(Violation::render).collect::<Vec<_>>().join("\n")
    );
}

/// 匹配器自检：一段未收口的分配长度裸转必须被认作未登记站点，守卫不能只锁住既有清单。
#[test]
fn matcher_flags_a_handwritten_allocation_cast() {
    let sample = "let mut buf = vec![0u8; recorded.len as usize];";
    assert!(is_bare_usize_cast(sample));
    assert!(!routed_through_helper(sample));
}

/// 锚点位置无关自检：同一行内容在文件里上移/下移（上方插入若干行）仍能命中同一登记；
/// 重复行按出现次序锚定，插行后次序不变；内容一旦被改写即失配。
#[test]
fn anchor_follows_the_line_not_its_position() {
    let target = "let mut buf = vec![0u8; recorded.len as usize];";
    let before = "fn f() {\n    let other: u64 = 1;\n    let mut buf = vec![0u8; recorded.len as usize];\n}\n";
    let after = "fn f() {\n    let other: u64 = 1;\n    let inserted: u64 = 2;\n    let another: u64 = 3;\n    let mut buf = vec![0u8; recorded.len as usize];\n}\n";
    let anchor = Allow {
        file: "sample.rs",
        line: target,
        occurrence: 1,
        count: 1,
        category: "display",
        reason: "自检样本",
    };

    let before_sites = bare_cast_sites(before);
    let after_sites = bare_cast_sites(after);
    assert_eq!(before_sites.len(), 1);
    assert_eq!(after_sites.len(), 1);
    assert!(registry_matches(&anchor, "sample.rs", &before_sites[0]));
    assert!(registry_matches(&anchor, "sample.rs", &after_sites[0]));
    // 行号已经漂移，命中仍成立：锚点不看位置
    assert_ne!(before_sites[0].line_no, after_sites[0].line_no);

    // 重复行：第 2 次出现的锚点在上方插行后仍指向第 2 行
    let dup_before = "fn f() {\n    let x = a as usize;\n    let x = a as usize;\n}\n";
    let dup_after = "fn f() {\n    let y = 1u64;\n    let x = a as usize;\n    let x = a as usize;\n}\n";
    let dup_anchor = Allow {
        file: "sample.rs",
        line: "let x = a as usize;",
        occurrence: 2,
        count: 2,
        category: "display",
        reason: "自检样本",
    };
    let dup_before_sites = bare_cast_sites(dup_before);
    let dup_after_sites = bare_cast_sites(dup_after);
    assert_eq!(dup_before_sites.len(), 2);
    assert_eq!(dup_after_sites.len(), 2);
    assert!(registry_matches(&dup_anchor, "sample.rs", &dup_before_sites[1]));
    assert!(registry_matches(&dup_anchor, "sample.rs", &dup_after_sites[1]));

    // 内容被改写即失配：锚点不接受近似行
    let changed = "fn f() {\n    let mut buf = vec![0u8; recorded.other as usize];\n}\n";
    let changed_sites = bare_cast_sites(changed);
    assert_eq!(changed_sites.len(), 1);
    assert!(!registry_matches(&anchor, "sample.rs", &changed_sites[0]));
}

/// 总条数自检：在样本里再插一条与已登记行逐字相同的行，多出的一行不能冒用旧登记——
/// 普查同时报未登记站点与总数不符。
#[test]
fn a_verbatim_duplicate_cannot_ride_an_existing_anchor() {
    let line = "let x = a as usize;";
    let once = format!("fn f() {{\n    {line}\n}}\n");
    let twice = format!("fn f() {{\n    {line}\n    {line}\n}}\n");
    let registry = [Allow {
        file: "sample.rs",
        line,
        occurrence: 1,
        count: 1,
        category: "display",
        reason: "自检样本",
    }];

    let clean = run_census(&registry, &[("sample.rs".into(), once)]);
    assert!(clean.unregistered.is_empty());
    assert!(clean.dangling.is_empty());
    assert!(clean.count_mismatch.is_empty());

    let doubled = run_census(&registry, &[("sample.rs".into(), twice)]);
    // 第二条同内容行没有自己的登记：既落不进次序锚点，也让总数 1 ≠ 2
    assert_eq!(doubled.unregistered.len(), 1);
    assert_eq!(doubled.count_mismatch.len(), 1);
    assert!(doubled.dangling.is_empty());
}

/// 反向悬空自检：已登记行被整行删掉后，反向校验报悬空登记，总数 0 ≠ 1 同时报不符。
#[test]
fn a_deleted_registered_line_is_reported_as_dangling() {
    let line = "let x = a as usize;";
    let present = format!("fn f() {{\n    {line}\n}}\n");
    let removed = "fn f() {\n    let y = 1u64;\n}\n";
    let registry = [Allow {
        file: "sample.rs",
        line,
        occurrence: 1,
        count: 1,
        category: "display",
        reason: "自检样本",
    }];

    let clean = run_census(&registry, &[("sample.rs".into(), present)]);
    assert!(clean.unregistered.is_empty());
    assert!(clean.dangling.is_empty());
    assert!(clean.count_mismatch.is_empty());

    let gone = run_census(&registry, &[("sample.rs".into(), removed.to_string())]);
    assert_eq!(gone.dangling.len(), 1);
    assert_eq!(gone.count_mismatch.len(), 1);
    assert!(gone.unregistered.is_empty());
}

/// `host_usize` 在一份源码里的定义条数（只认 `pub(crate) fn host_usize` 起头的生产行）。
fn definition_count(text: &str) -> usize {
    production_lines(text)
        .iter()
        .filter(|(_, l)| {
            l.contains("fn host_usize") && l.trim_start().starts_with("pub(crate) fn host_usize")
        })
        .count()
}

/// 除 `support.rs` 外，别处再写一个 `fn ... host_usize` 同义收口函数即属第二来源。
fn second_definition_sites(sources: &[(String, String)]) -> Vec<Violation> {
    let mut out = Vec::new();
    for (path, text) in sources {
        if path == "support.rs" || is_fixture_file(path) {
            continue;
        }
        for (n, stripped) in stripped_production(text) {
            if stripped.starts_with("fn ") && stripped.contains("host_usize") {
                out.push(Violation {
                    file: path.clone(),
                    line_no: n,
                    category: "second-definition",
                    content: stripped,
                });
            }
        }
    }
    out
}

/// 转换点只允许在 `support.rs` 定义一处；别处再写一个同义收口函数即属第二来源。
#[test]
fn the_conversion_point_has_a_single_definition() {
    let support = guards::source("support.rs");
    assert_eq!(
        definition_count(&support),
        1,
        "`host_usize` 必须只在 support.rs 定义一次"
    );

    let seconds = second_definition_sites(&rust_sources());
    assert!(
        seconds.is_empty(),
        "这些文件又定义了宽度收口函数：\n{}",
        seconds.iter().map(Violation::render).collect::<Vec<_>>().join("\n")
    );
}

/// 注释剥离自检：行注释、文档注释、跨行块注释与字符串字面量里的 `as usize` 都不计入
/// 生产站点；块注释外的真代码仍被识别。
#[test]
fn comments_are_stripped_before_the_cast_census() {
    let comment_only = [
        "// let x = a as usize;",
        "/// let x = a as usize;",
        "//! let x = a as usize;",
        "let p = \"// not a real cast: as usize\";",
    ];
    for sample in comment_only {
        let sites = bare_cast_sites(&format!("fn f() {{\n    {sample}\n}}\n"));
        assert!(sites.is_empty(), "注释或字符串里的 as usize 不应计入：{sample}");
    }

    let block = "fn f() {\n    /* outer\n       let x = a as usize;\n    */\n    let y = b as usize;\n}\n";
    let sites = bare_cast_sites(block);
    assert_eq!(sites.len(), 1);
    assert!(sites[0].content.contains("let y = b as usize;"));
}

/// 空白形态自检：`as` 与 `usize` 之间多空格、制表符仍按 token 识别；标识符片段不算。
#[test]
fn extra_whitespace_between_as_and_usize_still_matches() {
    for sample in [
        "let x = a as   usize;",
        "let x = a as\tusize;",
        "let x = a as \t usize;",
    ] {
        assert!(is_bare_usize_cast(sample), "多空白仍应识别：{sample}");
        assert!(!routed_through_helper(sample));
    }
    assert!(!is_bare_usize_cast("let x = baseline;"));
}

/// 注释路由自检：同一行注释里出现 `host_usize(` 不会把代码区的裸转当成已收口。
#[test]
fn a_host_usize_in_a_comment_does_not_route_the_cast() {
    let sample = "let x = a as usize; // see host_usize(here)";
    let stripped = code_only(sample);
    assert!(is_bare_usize_cast(stripped.trim()));
    assert!(!routed_through_helper(stripped.trim()));
    let sites = bare_cast_sites(&format!("fn f() {{\n    {sample}\n}}\n"));
    assert_eq!(sites.len(), 1);
}

/// 盲区零容忍：`transmute` 绕过类型系统宽度检查，字面量普查覆盖不到，生产行出现即变红。
fn transmute_sites(sources: &[(String, String)]) -> Vec<Violation> {
    let mut out = Vec::new();
    for (path, text) in sources {
        if is_fixture_file(path) {
            continue;
        }
        for (n, stripped) in stripped_production(text) {
            if stripped.contains("transmute") {
                out.push(Violation {
                    file: path.clone(),
                    line_no: n,
                    category: "transmute",
                    content: stripped,
                });
            }
        }
    }
    out
}

#[test]
fn no_transmute_in_production() {
    let found = transmute_sites(&rust_sources());
    assert!(
        found.is_empty(),
        "生产代码不允许 std::mem::transmute：\n{}",
        found.iter().map(Violation::render).collect::<Vec<_>>().join("\n")
    );
}

/// `as _` 普查结果：未登记站点与悬空登记各自列出。
struct WildcardCensus {
    unregistered: Vec<Violation>,
    dangling: Vec<Violation>,
}

fn wildcard_census(registry: &[WildcardCast], sources: &[(String, String)]) -> WildcardCensus {
    let sites = wildcard_sites(sources);

    let mut unregistered = Vec::new();
    for (path, content, n) in &sites {
        if !registry.iter().any(|w| w.file == path && w.line == content) {
            unregistered.push(Violation {
                file: path.clone(),
                line_no: *n,
                category: "unregistered",
                content: content.clone(),
            });
        }
    }
    let mut dangling = Vec::new();
    for w in registry {
        if !sites.iter().any(|(p, c, _)| p == w.file && c == w.line) {
            dangling.push(Violation {
                file: w.file.to_string(),
                line_no: 0,
                category: "wildcard",
                content: format!("{} — {}", w.line, w.reason),
            });
        }
    }
    WildcardCensus { unregistered, dangling }
}

/// `as _` 普查：非 `use` 导入的通配 cast 必须逐条登记；新出现一条即变红，登记悬空同样报出。
#[test]
fn wildcard_casts_are_registered() {
    let census = wildcard_census(WILDCARD_CASTS, &rust_sources());
    assert!(
        census.unregistered.is_empty(),
        "新出现的非 use 导入 `as _` 未登记：\n{}",
        census.unregistered.iter().map(Violation::render).collect::<Vec<_>>().join("\n")
    );
    assert!(
        census.dangling.is_empty(),
        "`as _` 登记已无对应代码行：\n{}",
        census.dangling.iter().map(Violation::render).collect::<Vec<_>>().join("\n")
    );
}

/// 合成样本自检（仓库外拼样）：一处既没走 `host_usize`、也没登记的裸转必须被纯普查报出，
/// 且报告能定位到具体行号；判据"会不会红"由 `cargo test` 自己回答，不靠人工记录。
#[test]
fn synthetic_violation_is_reported() {
    let sample = "fn f(n: u64) -> Vec<u8> {\n    let mut buf = vec![0u8; n as usize];\n    buf\n}\n";
    let census = run_census(&[], &[("sample.rs".to_string(), sample.to_string())]);

    assert_eq!(census.unregistered.len(), 1);
    let v = &census.unregistered[0];
    assert_eq!(v.file, "sample.rs");
    assert_eq!(v.line_no, 2);
    assert!(v.content.contains("n as usize"));
    assert!(census.dangling.is_empty());
    assert!(census.count_mismatch.is_empty());
}

/// 合成样本自检（仓库外拼样）：注释、字符串字面量、`#[cfg(test)] mod` 体与跨行块注释里的
/// `as usize` 都不算生产裸转，纯普查必须零违规；把样本里的等价写法换成真代码行即会变红。
#[test]
fn equivalent_writing_is_not_reported() {
    let variants: &[(&str, &str)] = &[
        ("line-comment", "// let x = a as usize;\n"),
        ("doc-comment", "/// let x = a as usize;\n"),
        ("inner-doc", "//! let x = a as usize;\n"),
        ("string-literal", "let p = \"// not a real cast: as usize\";\n"),
        (
            "block-comment",
            "/* outer\n   let x = a as usize;\n*/\n",
        ),
        (
            "cfg-test-module",
            "#[cfg(test)]\nmod tests {\n    fn f() { let x = a as usize; }\n}\n",
        ),
    ];
    for (name, source) in variants {
        let census = run_census(&[], &[((*name).to_string(), source.to_string())]);
        assert!(
            census.unregistered.is_empty(),
            "{name}: 等价写法不应被报成裸转：\n{}",
            census.unregistered.iter().map(Violation::render).collect::<Vec<_>>().join("\n")
        );
        assert!(census.dangling.is_empty(), "{name}: 不应报悬空登记",);
        assert!(census.count_mismatch.is_empty(), "{name}: 不应报总数不符",);
    }
}

// ---------------------------------------------------------------------------
// host_usize 调用点失败策略普查
//
// 每个 `host_usize(` 调用点所在语句必须带以下失败处理之一：`?`、`map_err(...)`、
// `.ok()`，或等价的 `is_ok_and(...)` / `match` 显式分支；裸 `.unwrap()`、
// `.unwrap_or(0)`、结果无人接都判红。
//
// `.ok()` 站点逐条登记，锚点口径与裸转白名单一致：（相对路径、去首尾空白整行内容、
// 同内容出现次序、该内容现存总条数）。失败归类只允许两类：
// - `保守`：转换失败落到拒绝/不适用侧（报错返回，或 None 被调用方按相交/不可收尾处理）
// - `放行`：失败等价于"没有值"，且下游另有闸独立判定许可
// 登记理由不能为空；新增未登记的 `.ok()` 即红，登记悬空或总数不符同样报出。

/// `.ok()` 站点登记条目：文件 + 锚定行内容 + 出现次序 + 总条数 + 失败归类 + 理由。
struct OkSite {
    file: &'static str,
    line: &'static str,
    occurrence: usize,
    count: usize,
    class: &'static str,
    reason: &'static str,
}

const OK_CLASSES: &[&str] = &["保守", "放行"];

const OK_SITES: &[OkSite] = &[
    OkSite {
        file: "gpt_policy.rs",
        line: "let i = crate::support::host_usize(n)",
        occurrence: 1,
        count: 1,
        class: "保守",
        reason: "转换失败经 .ok() 落 None，checked_sub/filter 落空后 ok_or_else(...)? 以分区号越界拒绝返回",
    },
    OkSite {
        file: "exec/remote.rs",
        line: ".and_then(|index| crate::support::host_usize(index).ok())",
        occurrence: 1,
        count: 1,
        class: "保守",
        reason: "转换失败成 None，ok_or_else(...)? 以协议错误返回；步骤序号不缺省成 0",
    },
    OkSite {
        file: "recovery/rollback.rs",
        line: "let cursor = crate::support::host_usize(reloc.entry_index).ok()?;",
        occurrence: 1,
        count: 1,
        class: "保守",
        reason: "失败落 None，move_writes_overlap_old_ranges 把 None 当作相交（Some(true)），走拒绝侧",
    },
    OkSite {
        file: "recovery/journal.rs",
        line: ".get(crate::support::host_usize(reloc.entry_index).ok()?)?;",
        occurrence: 1,
        count: 1,
        class: "保守",
        reason: "失败落 None，本轮不写回收尾游标；是否完成另由 table_record_done_if_complete 判定，不据此签发收尾许可",
    },
    OkSite {
        file: "table/mbr.rs",
        line: "let i = crate::support::host_usize(n)",
        occurrence: 1,
        count: 1,
        class: "保守",
        reason: "转换失败经 .ok() 落 None，checked_sub/filter 落空后 ok_or_else(...)? 以 1..=4 越界拒绝返回",
    },
];

/// 一个 `host_usize(` 调用点：所在文件、首行行号、锚定行内容、语句窗口与判定结果。
#[derive(Clone)]
struct HostSite {
    file: String,
    line_no: usize,
    content: String,
    handled: bool,
    handler: String,
    uses_ok: bool,
}

/// 从调用点首行起拼语句窗口：到第一条以 `;` 收尾的生产行为止，封顶 12 行；
/// 窗口文本取剥注释、空字面量后的代码区，避免注释里的 `?`/`match` 干扰判定。
fn statement_window(prod: &[(usize, &str)], code_lines: &[&str], start: usize) -> String {
    let mut window = String::new();
    let mut j = start;
    while j < prod.len() && j - start < 12 {
        let (n, _) = prod[j];
        let stripped = code_lines.get(n - 1).copied().unwrap_or("").trim();
        window.push_str(stripped);
        window.push('\n');
        if stripped.ends_with(';') {
            break;
        }
        j += 1;
    }
    window
}

fn host_usize_sites(sources: &[(String, String)]) -> Vec<HostSite> {
    let mut out = Vec::new();
    for (path, text) in sources {
        if is_fixture_file(path) {
            continue;
        }
        let prod = production_lines(text);
        let code = code_only(text);
        let code_lines: Vec<&str> = code.lines().collect();
        let mut i = 0;
        while i < prod.len() {
            let (n, raw) = prod[i];
            let stripped = code_lines.get(n - 1).copied().unwrap_or("").trim();
            if !stripped.contains("host_usize(") {
                i += 1;
                continue;
            }
            let window = statement_window(&prod, &code_lines, i);
            let uses_ok = window.contains(".ok()");
            let handlers: [(&str, bool); 5] = [
                ("map_err(", window.contains("map_err(")),
                ("?", window.contains('?')),
                (".ok()", uses_ok),
                ("is_ok_and(", window.contains("is_ok_and(")),
                ("match ", window.contains("match ")),
            ];
            let matched: Vec<&str> = handlers
                .iter()
                .filter(|(_, ok)| *ok)
                .map(|(k, _)| *k)
                .collect();
            let handled = !matched.is_empty();
            out.push(HostSite {
                file: path.clone(),
                line_no: n,
                content: raw.trim().to_string(),
                handled,
                handler: matched.join(","),
                uses_ok,
            });
            // 窗口末行已在 statement_window 内定位；跳到其后继续扫
            let mut end = i;
            while end < prod.len() && end - i < 12 {
                let (en, _) = prod[end];
                let s = code_lines.get(en - 1).copied().unwrap_or("").trim();
                if s.ends_with(';') {
                    break;
                }
                end += 1;
            }
            i = end + 1;
        }
    }
    out
}

/// 失败策略普查结果：无失败处理的站点、未登记 `.ok()`、悬空登记、总数不符。
struct OkCensus {
    unhandled: Vec<Violation>,
    unregistered: Vec<Violation>,
    dangling: Vec<Violation>,
    count_mismatch: Vec<Violation>,
}

fn ok_site_matches(entry: &OkSite, path: &str, site: &HostSite, occurrence: usize) -> bool {
    entry.file == path && entry.line == site.content && entry.occurrence == occurrence
}

fn host_usize_failure_census(registry: &[OkSite], sources: &[(String, String)]) -> OkCensus {
    for r in registry {
        assert!(
            OK_CLASSES.contains(&r.class),
            "{}:{} 失败归类必须是 保守/放行",
            r.file, r.line
        );
        assert!(!r.reason.is_empty(), "{}:{} 必须带失败理由", r.file, r.line);
    }

    let sites = host_usize_sites(sources);

    let mut unhandled = Vec::new();
    let mut ok_by_key: BTreeMap<(String, String), Vec<HostSite>> = BTreeMap::new();
    for site in &sites {
        if !site.handled {
            unhandled.push(Violation {
                file: site.file.clone(),
                line_no: site.line_no,
                category: "no-failure-handling",
                content: format!(
                    "{} — 无 ?/map_err/.ok()/is_ok_and/match 处理（识别到的形态: {}）",
                    site.content, site.handler
                ),
            });
        }
        if site.uses_ok {
            ok_by_key
                .entry((site.file.clone(), site.content.clone()))
                .or_default()
                .push(site.clone());
        }
    }

    let mut ok_sites: Vec<(HostSite, usize)> = Vec::new();
    for list in ok_by_key.values() {
        for (k, site) in list.iter().enumerate() {
            ok_sites.push((site.clone(), k + 1));
        }
    }

    let mut unregistered = Vec::new();
    let mut matched: Vec<(String, String, usize)> = Vec::new();
    for (site, occurrence) in &ok_sites {
        match registry.iter().find(|a| ok_site_matches(a, &site.file, site, *occurrence)) {
            Some(_a) => matched.push((site.file.clone(), site.content.clone(), *occurrence)),
            None => unregistered.push(Violation {
                file: site.file.clone(),
                line_no: site.line_no,
                category: "unregistered-ok",
                content: site.content.clone(),
            }),
        }
    }

    let mut dangling = Vec::new();
    for a in registry {
        if !matched
            .iter()
            .any(|(p, c, k)| p == a.file && c == a.line && *k == a.occurrence)
        {
            dangling.push(Violation {
                file: a.file.to_string(),
                line_no: 0,
                category: a.class,
                content: format!("{} — {}", a.line, a.reason),
            });
        }
    }

    let mut registered_total: BTreeMap<(&str, &str), usize> = BTreeMap::new();
    for a in registry {
        match registered_total.get(&(a.file, a.line)) {
            Some(&old) => assert_eq!(
                old, a.count,
                "登记内部不一致：{}:{} 同内容条目登记了两个不同总数",
                a.file, a.line
            ),
            None => {
                registered_total.insert((a.file, a.line), a.count);
            }
        }
    }
    let mut count_mismatch = Vec::new();
    for ((file, line), &registered) in &registered_total {
        let actual = ok_by_key
            .get(&((*file).to_string(), (*line).to_string()))
            .map(|v| v.len())
            .unwrap_or(0);
        if registered != actual {
            count_mismatch.push(Violation {
                file: (*file).to_string(),
                line_no: 0,
                category: "count-mismatch",
                content: format!("登记总数 {registered}，现存 {actual} 条：{line}"),
            });
        }
    }

    OkCensus { unhandled, unregistered, dangling, count_mismatch }
}

#[test]
fn every_host_usize_call_site_handles_failure() {
    let census = host_usize_failure_census(OK_SITES, &rust_sources());

    assert!(
        census.unhandled.is_empty(),
        "这些 host_usize 调用点没有任何失败处理（?/map_err/.ok()/is_ok_and/match）：\n{}",
        census.unhandled.iter().map(Violation::render).collect::<Vec<_>>().join("\n")
    );
    assert!(
        census.unregistered.is_empty(),
        "这些 .ok() 站点未逐条登记（文件 + 内容 + 归类 + 理由）：\n{}",
        census.unregistered.iter().map(Violation::render).collect::<Vec<_>>().join("\n")
    );
    assert!(
        census.dangling.is_empty(),
        ".ok() 登记已无对应代码行，先复核再撤登记：\n{}",
        census.dangling.iter().map(Violation::render).collect::<Vec<_>>().join("\n")
    );
    assert!(
        census.count_mismatch.is_empty(),
        ".ok() 同内容条数与登记总数不符：\n{}",
        census.count_mismatch.iter().map(Violation::render).collect::<Vec<_>>().join("\n")
    );
}

/// 合成样本自检：裸 `.unwrap()` / `.unwrap_or(0)` 的调用点没有任何失败处理，必须判红。
#[test]
fn synthetic_bare_host_usize_without_handling_is_reported() {
    let sample = "fn f(x: u64, y: u64) -> usize {\n    let a = crate::support::host_usize(x).unwrap();\n    let b = crate::support::host_usize(y).unwrap_or(0);\n    a + b\n}\n";
    let census = host_usize_failure_census(&[], &[("sample.rs".to_string(), sample.to_string())]);

    assert_eq!(census.unhandled.len(), 2);
    assert!(census.unhandled.iter().any(|v| v.content.contains(".unwrap()")));
    assert!(census.unhandled.iter().any(|v| v.content.contains("unwrap_or(0)")));
    assert!(census.unregistered.is_empty());
}

/// 合成样本自检：`.ok()` 但登记表为空，必须作为未登记 `.ok()` 站点判红，并定位到该行。
#[test]
fn synthetic_unregistered_ok_site_is_reported() {
    let sample = "fn f(x: u64) -> Option<usize> {\n    let n = crate::support::host_usize(x).ok()?;\n    Some(n)\n}\n";
    let census = host_usize_failure_census(&[], &[("sample.rs".to_string(), sample.to_string())]);

    assert_eq!(census.unregistered.len(), 1);
    let v = &census.unregistered[0];
    assert_eq!(v.file, "sample.rs");
    assert_eq!(v.line_no, 2);
    assert!(v.content.contains(".ok()"));
    assert!(census.unhandled.is_empty());
}

/// 合成样本自检：`.ok()?` 站点已按口径登记且归类/理由齐全，不得误红。
#[test]
fn synthetic_registered_ok_site_is_not_flagged() {
    let line = "let n = crate::support::host_usize(x).ok()?;";
    let sample = format!("fn f(x: u64) -> Option<usize> {{\n    {line}\n    None\n}}\n");
    let registry = [OkSite {
        file: "sample.rs",
        line,
        occurrence: 1,
        count: 1,
        class: "保守",
        reason: "自检样本：失败落 None，调用方按无值处理",
    }];

    let census = host_usize_failure_census(&registry, &[("sample.rs".to_string(), sample)]);
    assert!(census.unhandled.is_empty());
    assert!(census.unregistered.is_empty());
    assert!(census.dangling.is_empty());
    assert!(census.count_mismatch.is_empty());
}
