//! 区间算术 checked 收口守卫：生产行上的整数二元运算 `+`、`-`、`*`（及 `+=`、`-=`、
//! `*=`）必须走显式路径，或带固定类别的 `// audit-allow: <class>` 注释逐条登记；
//! 既不走显式路径、也未登记的裸算即变红。
//!
//! 扫描范围：`src/` 下全部 `.rs`，用 `guards::production_lines` 去掉 `#[cfg(test)] mod`
//! 测试模块体；`src/**/tests/` 目录与名为 `tests.rs` 的夹具文件整份排除。
//!
//! 显式路径（同一语句的代码区出现其一即视为已收口）：
//! `checked_add` / `checked_sub` / `checked_mul` / `checked_div` / `checked_rem` /
//! `checked_neg` / `saturating_*` / `wrapping_*` / `host_usize(` / `u64::try_from` /
//! `i64::try_from`。
//!
//! 登记类别只允许五个固定枚举：
//! - `index-in-bounds`：下标/长度已由同一函数的 len 上界或切片约束证明
//! - `display-only`：值只进输出串、日志、错误文案，不参与任何判定分支
//! - `const-expr`：编译期常量或与运行数据无关的纯常量折叠
//! - `len-bounded`：由上游 `checked_*` 已证余量，或切片/已知长度保证不越界
//! - `counter`：循环/迭代计数或批量游标，不参与盘面几何
//!
//! 整模块豁免（`EXEMPT_MODULES`）只留给机械上不含整数算术的模块：宏展开重复符、trait
//!  bound 求和、纯帮助文本排版。豁免类同样是固定枚举，理由不能为空；新增一个不在清单里
//! 且含算术站点的模块即变红，豁免条目悬空（覆盖不到任何现存站点）也变红。
//!
//! 登记身份与位置无关：键是（相对路径、去注释去空白后的整行代码、该内容在本文件算术站点
//! 内的第几次出现、该内容现存总条数）。上方插行只移动行号，不改变命中；内容被删/改写即
//! 报悬空登记；同内容条数与登记总数不符即报 count_mismatch。行号只进失败信息。

mod guards;

use guards::{
    Site, Violation, anchor_of, code_only, is_fixture_file, number_occurrences, production_lines,
    rust_sources, tokenize,
};
use std::collections::BTreeMap;

/// 站点登记条目：文件 + 锚定代码行 + 出现次序 + 总条数 + 类别 + 理由。
struct Allow {
    file: &'static str,
    line: &'static str,
    occurrence: usize,
    count: usize,
    class: &'static str,
    reason: &'static str,
}

/// 整模块豁免条目：路径前缀 + 豁免类 + 机械理由。
struct Exempt {
    prefix: &'static str,
    class: &'static str,
    reason: &'static str,
}

const SITE_CLASSES: &[&str] = &[
    "index-in-bounds",
    "display-only",
    "const-expr",
    "len-bounded",
    "counter",
];

const EXEMPT_CLASSES: &[&str] = &["macro-machinery", "trait-bound-only", "help-render"];

/// 显式收口路径关键字（出现在同一行代码区即视为已 checked）。
const CHECKED_PATHS: &[&str] = &[
    "checked_add",
    "checked_sub",
    "checked_mul",
    "checked_div",
    "checked_rem",
    "checked_neg",
    "saturating_add",
    "saturating_sub",
    "saturating_mul",
    "saturating_div",
    "saturating_neg",
    "wrapping_add",
    "wrapping_sub",
    "wrapping_mul",
    "wrapping_neg",
    "host_usize(",
    "u64::try_from",
    "i64::try_from",
];

// 站点逐条登记见下方 LEDGER。
const LEDGER: &[Allow] = &[
    Allow {
        file: "assembly.rs",
        line: "last_lba: p.start_lba as u64 + p.size_lba as u64 - 1,",
        occurrence: 1,
        count: 1,
        class: "len-bounded",
        reason: "上游 checked_* 或前置比较已证余量/上界，该运算不回绕",
    },
    Allow {
        file: "assembly.rs",
        line: "let mut steps: Vec<PlanStep> = Vec::with_capacity(moves.len() * 3 + 1);",
        occurrence: 1,
        count: 1,
        class: "len-bounded",
        reason: "上游 checked_* 或前置比较已证余量/上界，该运算不回绕",
    },
    Allow {
        file: "assembly.rs",
        line: "number: (i + 1) as u32,",
        occurrence: 1,
        count: 1,
        class: "index-in-bounds",
        reason: "下标来自 enumerate 索引、len 上界或已校验切片范围，不越界",
    },
    Allow {
        file: "cli.rs",
        line: "b'g' | b'G' => (crate::support::GIB_BYTES, &rest[..rest.len() - 1]),",
        occurrence: 1,
        count: 1,
        class: "index-in-bounds",
        reason: "下标来自 enumerate 索引、len 上界或已校验切片范围，不越界",
    },
    Allow {
        file: "cli.rs",
        line: "b'k' | b'K' => (crate::support::KIB_BYTES, &rest[..rest.len() - 1]),",
        occurrence: 1,
        count: 1,
        class: "index-in-bounds",
        reason: "下标来自 enumerate 索引、len 上界或已校验切片范围，不越界",
    },
    Allow {
        file: "cli.rs",
        line: "b'm' | b'M' => (crate::support::MIB_BYTES, &rest[..rest.len() - 1]),",
        occurrence: 1,
        count: 1,
        class: "index-in-bounds",
        reason: "下标来自 enumerate 索引、len 上界或已校验切片范围，不越界",
    },
    Allow {
        file: "cli.rs",
        line: "b't' | b'T' => (crate::support::TIB_BYTES, &rest[..rest.len() - 1]),",
        occurrence: 1,
        count: 1,
        class: "index-in-bounds",
        reason: "下标来自 enumerate 索引、len 上界或已校验切片范围，不越界",
    },
    Allow {
        file: "cli.rs",
        line: "frac.push(char::from(b'0' + (r / denom) as u8));",
        occurrence: 1,
        count: 1,
        class: "display-only",
        reason: "结果仅进入输出串/日志/错误文案，不参与任何判定分支或盘面几何",
    },
    Allow {
        file: "cli.rs",
        line: "r *= 10;",
        occurrence: 1,
        count: 1,
        class: "counter",
        reason: "循环/批量游标计数，不参与盘面几何",
    },
    Allow {
        file: "cmd/info.rs",
        line: ".map_err(|e| format!(\"cannot read the VHD footer at offset {}: {e}\", src.size - 512))?;",
        occurrence: 1,
        count: 1,
        class: "display-only",
        reason: "结果仅进入输出串/日志/错误文案，不参与任何判定分支或盘面几何",
    },
    Allow {
        file: "cmd/info.rs",
        line: "Some(t) => println!(\"{}{t}}}\", &out[..out.len() - 1]),",
        occurrence: 1,
        count: 1,
        class: "display-only",
        reason: "结果仅进入输出串/日志/错误文案，不参与任何判定分支或盘面几何",
    },
    Allow {
        file: "cmd/info.rs",
        line: "i + 1,",
        occurrence: 1,
        count: 2,
        class: "display-only",
        reason: "结果仅进入输出串/日志/错误文案，不参与任何判定分支或盘面几何",
    },
    Allow {
        file: "cmd/info.rs",
        line: "let last_lba = p.start_lba as u64 + p.size_lba as u64 - 1;",
        occurrence: 1,
        count: 1,
        class: "display-only",
        reason: "结果仅进入输出串/日志/错误文案，不参与任何判定分支或盘面几何",
    },
    Allow {
        file: "cmd/info.rs",
        line: "println!(\"{},\\\"error\\\":\\\"{}\\\"}}\", &out[..out.len() - 1], json_escape(msg));",
        occurrence: 1,
        count: 1,
        class: "display-only",
        reason: "结果仅进入输出串/日志/错误文案，不参与任何判定分支或盘面几何",
    },
    Allow {
        file: "cmd/info.rs",
        line: "src.read_at(src.size - 512, &mut f)",
        occurrence: 1,
        count: 1,
        class: "len-bounded",
        reason: "上游 checked_* 或前置比较已证余量/上界，该运算不回绕",
    },
    Allow {
        file: "cmd/info.rs",
        line: "i + 1,",
        occurrence: 2,
        count: 2,
        class: "display-only",
        reason: "结果仅进入输出串/日志/错误文案，不参与任何判定分支或盘面几何",
    },
    Allow {
        file: "cmd/inspect.rs",
        line: "let committed = reloc.entry_index as usize + usize::from(*entry == reloc.entry_index);",
        occurrence: 1,
        count: 1,
        class: "len-bounded",
        reason: "上游 checked_* 或前置比较已证余量/上界，该运算不回绕",
    },
    Allow {
        file: "cmd/inspect.rs",
        line: "shown += 1;",
        occurrence: 1,
        count: 1,
        class: "counter",
        reason: "循环/批量游标计数，不参与盘面几何",
    },
    Allow {
        file: "cmd/layout.rs",
        line: "(old_end - old_start + 1)",
        occurrence: 1,
        count: 1,
        class: "len-bounded",
        reason: "上游 checked_* 或前置比较已证余量/上界，该运算不回绕",
    },
    Allow {
        file: "cmd/layout.rs",
        line: "(start, start + len - 1)",
        occurrence: 1,
        count: 1,
        class: "len-bounded",
        reason: "上游 checked_* 或前置比较已证余量/上界，该运算不回绕",
    },
    Allow {
        file: "cmd/layout.rs",
        line: ".find_map(|(s, e)| (e - s + 1 >= len).then_some(s))",
        occurrence: 1,
        count: 1,
        class: "len-bounded",
        reason: "上游 checked_* 或前置比较已证余量/上界，该运算不回绕",
    },
    Allow {
        file: "cmd/layout.rs",
        line: "after[at..at + w.bytes.len()].copy_from_slice(&w.bytes);",
        occurrence: 1,
        count: 1,
        class: "index-in-bounds",
        reason: "下标来自 enumerate 索引、len 上界或已校验切片范围，不越界",
    },
    Allow {
        file: "cmd/layout.rs",
        line: "g2.update_entry((part - 1) as usize, |te| {",
        occurrence: 1,
        count: 1,
        class: "index-in-bounds",
        reason: "下标来自 enumerate 索引、len 上界或已校验切片范围，不越界",
    },
    Allow {
        file: "cmd/layout.rs",
        line: "i + 1",
        occurrence: 1,
        count: 1,
        class: "index-in-bounds",
        reason: "下标来自 enumerate 索引、len 上界或已校验切片范围，不越界",
    },
    Allow {
        file: "cmd/layout.rs",
        line: "if i + 1 == part as usize || crate::table::is_unused(other) {",
        occurrence: 1,
        count: 1,
        class: "index-in-bounds",
        reason: "下标来自 enumerate 索引、len 上界或已校验切片范围，不越界",
    },
    Allow {
        file: "cmd/layout.rs",
        line: "let copy_len = old_len.min(new_end - new_start + 1);",
        occurrence: 1,
        count: 1,
        class: "len-bounded",
        reason: "上游 checked_* 或前置比较已证余量/上界，该运算不回绕",
    },
    Allow {
        file: "cmd/layout.rs",
        line: "let first = w.region.offset - w.region.offset % sector_size;",
        occurrence: 1,
        count: 1,
        class: "len-bounded",
        reason: "上游 checked_* 或前置比较已证余量/上界，该运算不回绕",
    },
    Allow {
        file: "cmd/layout.rs",
        line: "let len = e.ending_lba - e.starting_lba + 1;",
        occurrence: 1,
        count: 2,
        class: "len-bounded",
        reason: "上游 checked_* 或前置比较已证余量/上界，该运算不回绕",
    },
    Allow {
        file: "cmd/layout.rs",
        line: "let old_len = old_end - old_start + 1;",
        occurrence: 1,
        count: 1,
        class: "len-bounded",
        reason: "上游 checked_* 或前置比较已证余量/上界，该运算不回绕",
    },
    Allow {
        file: "cmd/layout.rs",
        line: "old_end - old_start + 1,",
        occurrence: 1,
        count: 1,
        class: "len-bounded",
        reason: "上游 checked_* 或前置比较已证余量/上界，该运算不回绕",
    },
    Allow {
        file: "cmd/layout.rs",
        line: "sectors: entry.ending_lba - entry.starting_lba + 1,",
        occurrence: 1,
        count: 1,
        class: "len-bounded",
        reason: "上游 checked_* 或前置比较已证余量/上界，该运算不回绕",
    },
    Allow {
        file: "cmd/layout.rs",
        line: "let len = e.ending_lba - e.starting_lba + 1;",
        occurrence: 2,
        count: 2,
        class: "len-bounded",
        reason: "上游 checked_* 或前置比较已证余量/上界，该运算不回绕",
    },
    Allow {
        file: "cmd/records.rs",
        line: "collect_dir(&path, backend, current, budget, depth + 1, out)?;",
        occurrence: 1,
        count: 1,
        class: "counter",
        reason: "循环/批量游标计数，不参与盘面几何",
    },
    Allow {
        file: "cmd/records.rs",
        line: "const MAX_DEPTH: usize = LAYOUT_DEPTH + 1;",
        occurrence: 1,
        count: 1,
        class: "const-expr",
        reason: "编译期常量或与运行数据无关的纯常量折叠",
    },
    Allow {
        file: "cmd/records.rs",
        line: "const MAX_FILE_BYTES: u64 = 64 * crate::support::MIB_BYTES;",
        occurrence: 1,
        count: 1,
        class: "const-expr",
        reason: "编译期常量或与运行数据无关的纯常量折叠",
    },
    Allow {
        file: "cmd/records.rs",
        line: "const MAX_TOTAL_BYTES: u64 = 512 * crate::support::MIB_BYTES;",
        occurrence: 1,
        count: 1,
        class: "const-expr",
        reason: "编译期常量或与运行数据无关的纯常量折叠",
    },
    Allow {
        file: "cmd/records.rs",
        line: "depth + 1",
        occurrence: 1,
        count: 1,
        class: "counter",
        reason: "循环/批量游标计数，不参与盘面几何",
    },
    Allow {
        file: "cmd/records.rs",
        line: "if depth + 1 > MAX_DEPTH {",
        occurrence: 1,
        count: 1,
        class: "counter",
        reason: "循环/批量游标计数，不参与盘面几何",
    },
    Allow {
        file: "cmd/release.rs",
        line: "Ok(crate::dev::Quarantine::Moved) => released += 1,",
        occurrence: 1,
        count: 1,
        class: "counter",
        reason: "循环/批量游标计数，不参与盘面几何",
    },
    Allow {
        file: "cmd/release.rs",
        line: "bail_fail(crate::outcome::PersistedChange::of(false, released + copied_only > 0).failure(cause))",
        occurrence: 1,
        count: 1,
        class: "counter",
        reason: "循环/批量游标计数，不参与盘面几何",
    },
    Allow {
        file: "cmd/release.rs",
        line: "copied_only += 1;",
        occurrence: 1,
        count: 1,
        class: "counter",
        reason: "循环/批量游标计数，不参与盘面几何",
    },
    Allow {
        file: "cmd/release.rs",
        line: "total += record.restore.len();",
        occurrence: 1,
        count: 1,
        class: "counter",
        reason: "循环/批量游标计数，不参与盘面几何",
    },
    Allow {
        file: "cmd/resize.rs",
        line: "movepart::ShiftIntent::Grow(new_end - end)",
        occurrence: 1,
        count: 1,
        class: "len-bounded",
        reason: "上游 checked_* 或前置比较已证余量/上界，该运算不回绕",
    },
    Allow {
        file: "cmd/resize/dry_run.rs",
        line: "if new_lba > p.size_lba as u64 + free {",
        occurrence: 1,
        count: 1,
        class: "len-bounded",
        reason: "上游 checked_* 或前置比较已证余量/上界，该运算不回绕",
    },
    Allow {
        file: "cmd/resize/dry_run.rs",
        line: "lba_bytes(new_end - start + 1, ss),",
        occurrence: 1,
        count: 1,
        class: "len-bounded",
        reason: "上游 checked_* 或前置比较已证余量/上界，该运算不回绕",
    },
    Allow {
        file: "cmd/resize/dry_run.rs",
        line: "let need = new_end - end;",
        occurrence: 1,
        count: 1,
        class: "len-bounded",
        reason: "上游 checked_* 或前置比较已证余量/上界，该运算不回绕",
    },
    Allow {
        file: "cmd/resize/dry_run.rs",
        line: "p.start_lba as u64 + new_lba - 1",
        occurrence: 1,
        count: 1,
        class: "len-bounded",
        reason: "上游 checked_* 或前置比较已证余量/上界，该运算不回绕",
    },
    Allow {
        file: "cmd/resize/dry_run.rs",
        line: "p.start_lba as u64 + new_lba - 1,",
        occurrence: 1,
        count: 1,
        class: "len-bounded",
        reason: "上游 checked_* 或前置比较已证余量/上界，该运算不回绕",
    },
    Allow {
        file: "cmd/resize/dry_run.rs",
        line: "p.start_lba as u64 + u64::from(p.size_lba) + free - 1",
        occurrence: 1,
        count: 1,
        class: "len-bounded",
        reason: "上游 checked_* 或前置比较已证余量/上界，该运算不回绕",
    },
    Allow {
        file: "cmd/resize/dry_run.rs",
        line: "p.start_lba as u64 + u64::from(p.size_lba) - 1,",
        occurrence: 1,
        count: 3,
        class: "len-bounded",
        reason: "上游 checked_* 或前置比较已证余量/上界，该运算不回绕",
    },
    Allow {
        file: "cmd/resize/dry_run.rs",
        line: "println!(\"plan: grow partition {part} : LBA {start}..{end} → {start}..{}\", end + free);",
        occurrence: 1,
        count: 1,
        class: "display-only",
        reason: "结果仅进入输出串/日志/错误文案，不参与任何判定分支或盘面几何",
    },
    Allow {
        file: "cmd/resize/dry_run.rs",
        line: "p.start_lba as u64 + u64::from(p.size_lba) - 1,",
        occurrence: 2,
        count: 3,
        class: "len-bounded",
        reason: "上游 checked_* 或前置比较已证余量/上界，该运算不回绕",
    },
    Allow {
        file: "cmd/resize/dry_run.rs",
        line: "p.start_lba as u64 + u64::from(p.size_lba) - 1,",
        occurrence: 3,
        count: 3,
        class: "len-bounded",
        reason: "上游 checked_* 或前置比较已证余量/上界，该运算不回绕",
    },
    Allow {
        file: "cmd/resize/lvm.rs",
        line: "let want = (f.lv_target - f.lv_size_before) / f.extent_bytes;",
        occurrence: 1,
        count: 1,
        class: "len-bounded",
        reason: "上游 checked_* 或前置比较已证余量/上界，该运算不回绕",
    },
    Allow {
        file: "cmd/resize/lvm.rs",
        line: "want - free",
        occurrence: 1,
        count: 1,
        class: "display-only",
        reason: "结果仅进入输出串/日志/错误文案，不参与任何判定分支或盘面几何",
    },
    Allow {
        file: "cmd/resize/msdos.rs",
        line: ".get(off + crate::table::MBR_ENTRY_BYTES - 4..off + crate::table::MBR_ENTRY_BYTES)?",
        occurrence: 1,
        count: 1,
        class: "index-in-bounds",
        reason: "下标来自 enumerate 索引、len 上界或已校验切片范围，不越界",
    },
    Allow {
        file: "cmd/resize/msdos.rs",
        line: "let end = start + u64::from(p.size_lba) - 1;",
        occurrence: 1,
        count: 1,
        class: "len-bounded",
        reason: "上游 checked_* 或前置比较已证余量/上界，该运算不回绕",
    },
    Allow {
        file: "cmd/resize/msdos.rs",
        line: "let new_size_lba = p.size_lba as u64 + free;",
        occurrence: 1,
        count: 1,
        class: "len-bounded",
        reason: "上游 checked_* 或前置比较已证余量/上界，该运算不回绕",
    },
    Allow {
        file: "cmd/resize/msdos.rs",
        line: "let table_range = (u64::from(p.start_lba), u64::from(p.start_lba) + u64::from(p.size_lba) - 1);",
        occurrence: 1,
        count: 1,
        class: "len-bounded",
        reason: "上游 checked_* 或前置比较已证余量/上界，该运算不回绕",
    },
    Allow {
        file: "cmd/resize/msdos.rs",
        line: "let target_end = start + target_size_lba - 1;",
        occurrence: 1,
        count: 1,
        class: "len-bounded",
        reason: "上游 checked_* 或前置比较已证余量/上界，该运算不回绕",
    },
    Allow {
        file: "cmd/resize/msdos.rs",
        line: "let want = new_size_lba - p.size_lba as u64;",
        occurrence: 1,
        count: 1,
        class: "len-bounded",
        reason: "上游 checked_* 或前置比较已证余量/上界，该运算不回绕",
    },
    Allow {
        file: "cmd/resize/msdos.rs",
        line: "let want_end = target_range.0 + bytes / ss - 1;",
        occurrence: 1,
        count: 1,
        class: "len-bounded",
        reason: "上游 checked_* 或前置比较已证余量/上界，该运算不回绕",
    },
    Allow {
        file: "cmd/resize/msdos.rs",
        line: "mbr_shrink_finish(a, &mut src, part, new_size_lba * ss, is_block)",
        occurrence: 1,
        count: 1,
        class: "len-bounded",
        reason: "上游 checked_* 或前置比较已证余量/上界，该运算不回绕",
    },
    Allow {
        file: "cmd/resize/msdos.rs",
        line: "record_lvm_baseline(&mut src, lv_sel.as_ref(), cur_bytes, new_size_lba * ss)",
        occurrence: 1,
        count: 1,
        class: "len-bounded",
        reason: "上游 checked_* 或前置比较已证余量/上界，该运算不回绕",
    },
    Allow {
        file: "cmd/resize/online.rs",
        line: "t / ss * ss",
        occurrence: 1,
        count: 1,
        class: "len-bounded",
        reason: "上游 checked_* 或前置比较已证余量/上界，该运算不回绕",
    },
    Allow {
        file: "cmd/resize/size.rs",
        line: "let delta = raw / crate::support::MIB_BYTES * crate::support::MIB_BYTES;",
        occurrence: 1,
        count: 1,
        class: "len-bounded",
        reason: "上游 checked_* 或前置比较已证余量/上界，该运算不回绕",
    },
    Allow {
        file: "dev/disk_guid.rs",
        line: "pub(crate) const CREDENTIAL_LEN: usize = 1 + 16;",
        occurrence: 1,
        count: 1,
        class: "const-expr",
        reason: "编译期常量或与运行数据无关的纯常量折叠",
    },
    Allow {
        file: "dev/host_repo.rs",
        line: "let mut input = Vec::with_capacity(9 + key.target.len());",
        occurrence: 1,
        count: 1,
        class: "len-bounded",
        reason: "上游 checked_* 或前置比较已证余量/上界，该运算不回绕",
    },
    Allow {
        file: "dev/occupancy.rs",
        line: "const MAGIC_END: u64 = SUPER_OFFSET + MAGIC_OFFSET as u64 + 8;",
        occurrence: 1,
        count: 1,
        class: "const-expr",
        reason: "编译期常量或与运行数据无关的纯常量折叠",
    },
    Allow {
        file: "dev/occupancy.rs",
        line: "const NUM_DEVICES_END: u64 = SUPER_OFFSET + NUM_DEVICES_OFFSET as u64 + 8;",
        occurrence: 1,
        count: 1,
        class: "const-expr",
        reason: "编译期常量或与运行数据无关的纯常量折叠",
    },
    Allow {
        file: "dev/occupancy.rs",
        line: "let covered = (src.size - SUPER_OFFSET).min(sb.len() as u64) as usize;",
        occurrence: 1,
        count: 1,
        class: "len-bounded",
        reason: "上游 checked_* 或前置比较已证余量/上界，该运算不回绕",
    },
    Allow {
        file: "dev/occupancy.rs",
        line: "let magic = &sb[MAGIC_OFFSET..MAGIC_OFFSET + 8];",
        occurrence: 1,
        count: 1,
        class: "index-in-bounds",
        reason: "下标来自 enumerate 索引、len 上界或已校验切片范围，不越界",
    },
    Allow {
        file: "dev/occupancy.rs",
        line: "let mut sb = [0u8; NUM_DEVICES_OFFSET + 8];",
        occurrence: 1,
        count: 1,
        class: "index-in-bounds",
        reason: "下标来自 enumerate 索引、len 上界或已校验切片范围，不越界",
    },
    Allow {
        file: "dev/occupancy.rs",
        line: "raw.copy_from_slice(&sb[NUM_DEVICES_OFFSET..NUM_DEVICES_OFFSET + 8]);",
        occurrence: 1,
        count: 1,
        class: "index-in-bounds",
        reason: "下标来自 enumerate 索引、len 上界或已校验切片范围，不越界",
    },
    Allow {
        file: "dev/source.rs",
        line: "after[at..at + buf.len()].copy_from_slice(buf);",
        occurrence: 1,
        count: 1,
        class: "index-in-bounds",
        reason: "下标来自 enumerate 索引、len 上界或已校验切片范围，不越界",
    },
    Allow {
        file: "dev/source.rs",
        line: "done += n;",
        occurrence: 1,
        count: 1,
        class: "counter",
        reason: "循环/批量游标计数，不参与盘面几何",
    },
    Allow {
        file: "dev/source.rs",
        line: "let end = ((off + buf.len() as u64).div_ceil(sector_size))",
        occurrence: 1,
        count: 1,
        class: "len-bounded",
        reason: "上游 checked_* 或前置比较已证余量/上界，该运算不回绕",
    },
    Allow {
        file: "dev/source.rs",
        line: "let first = off - off % sector_size;",
        occurrence: 1,
        count: 1,
        class: "len-bounded",
        reason: "上游 checked_* 或前置比较已证余量/上界，该运算不回绕",
    },
    Allow {
        file: "dev/source.rs",
        line: "let nth = RECORD_WRITES.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;",
        occurrence: 1,
        count: 1,
        class: "counter",
        reason: "循环/批量游标计数，不参与盘面几何",
    },
    Allow {
        file: "dev/target.rs",
        line: "Some(pos) if pos + 1 == s.len() => Err(\"missing partition number after ':'\"),",
        occurrence: 1,
        count: 1,
        class: "index-in-bounds",
        reason: "下标来自 enumerate 索引、len 上界或已校验切片范围，不越界",
    },
    Allow {
        file: "dev/target.rs",
        line: "Some(pos) if s[pos + 1..].chars().all(|c| c.is_ascii_digit()) => {",
        occurrence: 1,
        count: 1,
        class: "index-in-bounds",
        reason: "下标来自 enumerate 索引、len 上界或已校验切片范围，不越界",
    },
    Allow {
        file: "dev/target.rs",
        line: "Some(pos) if s[pos + 1..].eq_ignore_ascii_case(\"last\") => {",
        occurrence: 1,
        count: 1,
        class: "index-in-bounds",
        reason: "下标来自 enumerate 索引、len 上界或已校验切片范围，不越界",
    },
    Allow {
        file: "dev/target.rs",
        line: "let n: u32 = s[pos + 1..].parse().map_err(|_| \"partition number out of range\")?;",
        occurrence: 1,
        count: 1,
        class: "index-in-bounds",
        reason: "下标来自 enumerate 索引、len 上界或已校验切片范围，不越界",
    },
    Allow {
        file: "domain.rs",
        line: "let unit = (u64::from(mib) * crate::support::MIB_BYTES)",
        occurrence: 1,
        count: 1,
        class: "len-bounded",
        reason: "上游 checked_* 或前置比较已证余量/上界，该运算不回绕",
    },
    Allow {
        file: "exec/dispatch.rs",
        line: ".map_err(|e| e.context(&format!(\"plan step {}\", index + 1)))?;",
        occurrence: 1,
        count: 1,
        class: "display-only",
        reason: "结果仅进入输出串/日志/错误文案，不参与任何判定分支或盘面几何",
    },
    Allow {
        file: "exec/mod.rs",
        line: "committed_entries -= 1;",
        occurrence: 1,
        count: 1,
        class: "counter",
        reason: "循环/批量游标计数，不参与盘面几何",
    },
    Allow {
        file: "exec/mod.rs",
        line: "entry += 1;",
        occurrence: 1,
        count: 2,
        class: "counter",
        reason: "循环/批量游标计数，不参与盘面几何",
    },
    Allow {
        file: "exec/mod.rs",
        line: "entry += 1;",
        occurrence: 2,
        count: 2,
        class: "counter",
        reason: "循环/批量游标计数，不参与盘面几何",
    },
    Allow {
        file: "exec/protocol.rs",
        line: "if bytes.len() + len > MAX_FRAME {",
        occurrence: 1,
        count: 1,
        class: "len-bounded",
        reason: "上游 checked_* 或前置比较已证余量/上界，该运算不回绕",
    },
    Allow {
        file: "exec/protocol.rs",
        line: "let len = newline.map_or(buf.len(), |i| i + 1);",
        occurrence: 1,
        count: 1,
        class: "len-bounded",
        reason: "上游 checked_* 或前置比较已证余量/上界，该运算不回绕",
    },
    Allow {
        file: "exec/protocol.rs",
        line: "let mut out = String::with_capacity(bytes.len() * 2);",
        occurrence: 1,
        count: 1,
        class: "len-bounded",
        reason: "上游 checked_* 或前置比较已证余量/上界，该运算不回绕",
    },
    Allow {
        file: "exec/protocol.rs",
        line: "out.push((hi * 16 + lo) as u8);",
        occurrence: 1,
        count: 1,
        class: "len-bounded",
        reason: "上游 checked_* 或前置比较已证余量/上界，该运算不回绕",
    },
    Allow {
        file: "exec/qemu.rs",
        line: "out[index] = (hi * 16 + lo) as u8;",
        occurrence: 1,
        count: 1,
        class: "index-in-bounds",
        reason: "下标来自 enumerate 索引、len 上界或已校验切片范围，不越界",
    },
    Allow {
        file: "exec/remote.rs",
        line: "let deadline = Instant::now() + timeout;",
        occurrence: 1,
        count: 1,
        class: "len-bounded",
        reason: "上游 checked_* 或前置比较已证余量/上界，该运算不回绕",
    },
    Allow {
        file: "exec/remote.rs",
        line: "self.next_id += 1;",
        occurrence: 1,
        count: 2,
        class: "counter",
        reason: "循环/批量游标计数，不参与盘面几何",
    },
    Allow {
        file: "exec/remote.rs",
        line: "self.next_id += 1;",
        occurrence: 2,
        count: 2,
        class: "counter",
        reason: "循环/批量游标计数，不参与盘面几何",
    },
    Allow {
        file: "execution/copy.rs",
        line: "(pos / chunk_bytes + 1) as usize,",
        occurrence: 1,
        count: 3,
        class: "len-bounded",
        reason: "上游 checked_* 或前置比较已证余量/上界，该运算不回绕",
    },
    Allow {
        file: "execution/copy.rs",
        line: "(src_off + pos) / sector_size,",
        occurrence: 1,
        count: 1,
        class: "len-bounded",
        reason: "上游 checked_* 或前置比较已证余量/上界，该运算不回绕",
    },
    Allow {
        file: "execution/copy.rs",
        line: "dst_lba_end - 1",
        occurrence: 1,
        count: 1,
        class: "len-bounded",
        reason: "上游 checked_* 或前置比较已证余量/上界，该运算不回绕",
    },
    Allow {
        file: "execution/copy.rs",
        line: "let dst = dst_off + pos;",
        occurrence: 1,
        count: 1,
        class: "len-bounded",
        reason: "上游 checked_* 或前置比较已证余量/上界，该运算不回绕",
    },
    Allow {
        file: "execution/copy.rs",
        line: "let len = chunk_bytes.min(total - pos);",
        occurrence: 1,
        count: 1,
        class: "len-bounded",
        reason: "上游 checked_* 或前置比较已证余量/上界，该运算不回绕",
    },
    Allow {
        file: "execution/copy.rs",
        line: "pos += len;",
        occurrence: 1,
        count: 1,
        class: "counter",
        reason: "循环/批量游标计数，不参与盘面几何",
    },
    Allow {
        file: "execution/copy.rs",
        line: "src.read_at(src_off + pos, &mut buf).map_err(|e| {",
        occurrence: 1,
        count: 1,
        class: "len-bounded",
        reason: "上游 checked_* 或前置比较已证余量/上界，该运算不回绕",
    },
    Allow {
        file: "execution/copy.rs",
        line: "src_lba_end - 1,",
        occurrence: 1,
        count: 1,
        class: "len-bounded",
        reason: "上游 checked_* 或前置比较已证余量/上界，该运算不回绕",
    },
    Allow {
        file: "execution/copy.rs",
        line: "(pos / chunk_bytes + 1) as usize,",
        occurrence: 2,
        count: 3,
        class: "len-bounded",
        reason: "上游 checked_* 或前置比较已证余量/上界，该运算不回绕",
    },
    Allow {
        file: "execution/copy.rs",
        line: "(pos / chunk_bytes + 1) as usize,",
        occurrence: 3,
        count: 3,
        class: "len-bounded",
        reason: "上游 checked_* 或前置比较已证余量/上界，该运算不回绕",
    },
    Allow {
        file: "execution/mkfs.rs",
        line: "if base > src.size || len > src.size - base {",
        occurrence: 1,
        count: 1,
        class: "len-bounded",
        reason: "上游 checked_* 或前置比较已证余量/上界，该运算不回绕",
    },
    Allow {
        file: "execution/mod.rs",
        line: ".map_err(|e| e.context(&format!(\"plan step {}\", index + 1)))?;",
        occurrence: 1,
        count: 4,
        class: "display-only",
        reason: "结果仅进入输出串/日志/错误文案，不参与任何判定分支或盘面几何",
    },
    Allow {
        file: "execution/mod.rs",
        line: "const MAX_CHUNK: u64 = 64 * 1024 * 1024;",
        occurrence: 1,
        count: 1,
        class: "const-expr",
        reason: "编译期常量或与运行数据无关的纯常量折叠",
    },
    Allow {
        file: "execution/mod.rs",
        line: "if !matches!(raw.get(index + 1), Some(PlanStep::WriteTable(_))) {",
        occurrence: 1,
        count: 2,
        class: "index-in-bounds",
        reason: "下标来自 enumerate 索引、len 上界或已校验切片范围，不越界",
    },
    Allow {
        file: "execution/mod.rs",
        line: "index + 1",
        occurrence: 1,
        count: 11,
        class: "index-in-bounds",
        reason: "下标来自 enumerate 索引、len 上界或已校验切片范围，不越界",
    },
    Allow {
        file: "execution/mod.rs",
        line: "index + 1,",
        occurrence: 1,
        count: 1,
        class: "index-in-bounds",
        reason: "下标来自 enumerate 索引、len 上界或已校验切片范围，不越界",
    },
    Allow {
        file: "execution/mod.rs",
        line: "index += 1;",
        occurrence: 1,
        count: 4,
        class: "counter",
        reason: "循环/批量游标计数，不参与盘面几何",
    },
    Allow {
        file: "execution/mod.rs",
        line: "index += 2 + usize::from(swap.is_some());",
        occurrence: 1,
        count: 1,
        class: "counter",
        reason: "循环/批量游标计数，不参与盘面几何",
    },
    Allow {
        file: "execution/mod.rs",
        line: "index += 2;",
        occurrence: 1,
        count: 1,
        class: "counter",
        reason: "循环/批量游标计数，不参与盘面几何",
    },
    Allow {
        file: "execution/mod.rs",
        line: "let Some(PlanStep::WriteTable(commit)) = raw.get(index + 1) else {",
        occurrence: 1,
        count: 2,
        class: "index-in-bounds",
        reason: "下标来自 enumerate 索引、len 上界或已校验切片范围，不越界",
    },
    Allow {
        file: "execution/mod.rs",
        line: "let swap = match raw.get(index + 2) {",
        occurrence: 1,
        count: 1,
        class: "index-in-bounds",
        reason: "下标来自 enumerate 索引、len 上界或已校验切片范围，不越界",
    },
    Allow {
        file: "execution/mod.rs",
        line: "pub(crate) const RESIZE_CHUNK_BYTES: u64 = 8 * crate::support::MIB_BYTES;",
        occurrence: 1,
        count: 1,
        class: "const-expr",
        reason: "编译期常量或与运行数据无关的纯常量折叠",
    },
    Allow {
        file: "execution/mod.rs",
        line: "reloc_index += 1;",
        occurrence: 1,
        count: 1,
        class: "counter",
        reason: "循环/批量游标计数，不参与盘面几何",
    },
    Allow {
        file: "execution/mod.rs",
        line: "self.chunks_in_entry += 1;",
        occurrence: 1,
        count: 1,
        class: "counter",
        reason: "循环/批量游标计数，不参与盘面几何",
    },
    Allow {
        file: "execution/mod.rs",
        line: "self.completed_steps += 1;",
        occurrence: 1,
        count: 2,
        class: "counter",
        reason: "循环/批量游标计数，不参与盘面几何",
    },
    Allow {
        file: "execution/mod.rs",
        line: "self.entry_index += 1;",
        occurrence: 1,
        count: 1,
        class: "counter",
        reason: "循环/批量游标计数，不参与盘面几何",
    },
    Allow {
        file: "execution/mod.rs",
        line: "self.transfer_cursor += 1;",
        occurrence: 1,
        count: 1,
        class: "counter",
        reason: "循环/批量游标计数，不参与盘面几何",
    },
    Allow {
        file: "execution/mod.rs",
        line: ".map_err(|e| e.context(&format!(\"plan step {}\", index + 1)))?;",
        occurrence: 2,
        count: 4,
        class: "display-only",
        reason: "结果仅进入输出串/日志/错误文案，不参与任何判定分支或盘面几何",
    },
    Allow {
        file: "execution/mod.rs",
        line: "if !matches!(raw.get(index + 1), Some(PlanStep::WriteTable(_))) {",
        occurrence: 2,
        count: 2,
        class: "index-in-bounds",
        reason: "下标来自 enumerate 索引、len 上界或已校验切片范围，不越界",
    },
    Allow {
        file: "execution/mod.rs",
        line: "index + 1",
        occurrence: 2,
        count: 11,
        class: "index-in-bounds",
        reason: "下标来自 enumerate 索引、len 上界或已校验切片范围，不越界",
    },
    Allow {
        file: "execution/mod.rs",
        line: "index += 1;",
        occurrence: 2,
        count: 4,
        class: "counter",
        reason: "循环/批量游标计数，不参与盘面几何",
    },
    Allow {
        file: "execution/mod.rs",
        line: "let Some(PlanStep::WriteTable(commit)) = raw.get(index + 1) else {",
        occurrence: 2,
        count: 2,
        class: "index-in-bounds",
        reason: "下标来自 enumerate 索引、len 上界或已校验切片范围，不越界",
    },
    Allow {
        file: "execution/mod.rs",
        line: "self.completed_steps += 1;",
        occurrence: 2,
        count: 2,
        class: "counter",
        reason: "循环/批量游标计数，不参与盘面几何",
    },
    Allow {
        file: "execution/mod.rs",
        line: ".map_err(|e| e.context(&format!(\"plan step {}\", index + 1)))?;",
        occurrence: 3,
        count: 4,
        class: "display-only",
        reason: "结果仅进入输出串/日志/错误文案，不参与任何判定分支或盘面几何",
    },
    Allow {
        file: "execution/mod.rs",
        line: "index + 1",
        occurrence: 3,
        count: 11,
        class: "index-in-bounds",
        reason: "下标来自 enumerate 索引、len 上界或已校验切片范围，不越界",
    },
    Allow {
        file: "execution/mod.rs",
        line: "index += 1;",
        occurrence: 3,
        count: 4,
        class: "counter",
        reason: "循环/批量游标计数，不参与盘面几何",
    },
    Allow {
        file: "execution/mod.rs",
        line: ".map_err(|e| e.context(&format!(\"plan step {}\", index + 1)))?;",
        occurrence: 4,
        count: 4,
        class: "display-only",
        reason: "结果仅进入输出串/日志/错误文案，不参与任何判定分支或盘面几何",
    },
    Allow {
        file: "execution/mod.rs",
        line: "index + 1",
        occurrence: 4,
        count: 11,
        class: "index-in-bounds",
        reason: "下标来自 enumerate 索引、len 上界或已校验切片范围，不越界",
    },
    Allow {
        file: "execution/mod.rs",
        line: "index += 1;",
        occurrence: 4,
        count: 4,
        class: "counter",
        reason: "循环/批量游标计数，不参与盘面几何",
    },
    Allow {
        file: "execution/mod.rs",
        line: "index + 1",
        occurrence: 5,
        count: 11,
        class: "index-in-bounds",
        reason: "下标来自 enumerate 索引、len 上界或已校验切片范围，不越界",
    },
    Allow {
        file: "execution/mod.rs",
        line: "index + 1",
        occurrence: 6,
        count: 11,
        class: "index-in-bounds",
        reason: "下标来自 enumerate 索引、len 上界或已校验切片范围，不越界",
    },
    Allow {
        file: "execution/mod.rs",
        line: "index + 1",
        occurrence: 7,
        count: 11,
        class: "index-in-bounds",
        reason: "下标来自 enumerate 索引、len 上界或已校验切片范围，不越界",
    },
    Allow {
        file: "execution/mod.rs",
        line: "index + 1",
        occurrence: 8,
        count: 11,
        class: "index-in-bounds",
        reason: "下标来自 enumerate 索引、len 上界或已校验切片范围，不越界",
    },
    Allow {
        file: "execution/mod.rs",
        line: "index + 1",
        occurrence: 9,
        count: 11,
        class: "index-in-bounds",
        reason: "下标来自 enumerate 索引、len 上界或已校验切片范围，不越界",
    },
    Allow {
        file: "execution/mod.rs",
        line: "index + 1",
        occurrence: 10,
        count: 11,
        class: "index-in-bounds",
        reason: "下标来自 enumerate 索引、len 上界或已校验切片范围，不越界",
    },
    Allow {
        file: "execution/mod.rs",
        line: "index + 1",
        occurrence: 11,
        count: 11,
        class: "index-in-bounds",
        reason: "下标来自 enumerate 索引、len 上界或已校验切片范围，不越界",
    },
    Allow {
        file: "execution/relocate.rs",
        line: "const CHECKPOINT_FLOOR: u64 = 8 * crate::support::MIB_BYTES;",
        occurrence: 1,
        count: 1,
        class: "const-expr",
        reason: "编译期常量或与运行数据无关的纯常量折叠",
    },
    Allow {
        file: "execution/relocate.rs",
        line: "dst_base + total,",
        occurrence: 1,
        count: 1,
        class: "len-bounded",
        reason: "上游 checked_* 或前置比较已证余量/上界，该运算不回绕",
    },
    Allow {
        file: "execution/relocate.rs",
        line: "if let Err(e) = src.read_at(src_base + within, &mut buf) {",
        occurrence: 1,
        count: 1,
        class: "len-bounded",
        reason: "上游 checked_* 或前置比较已证余量/上界，该运算不回绕",
    },
    Allow {
        file: "execution/relocate.rs",
        line: "if src_base + total > src.size || dst_base + total > src.size {",
        occurrence: 1,
        count: 1,
        class: "len-bounded",
        reason: "上游 checked_* 或前置比较已证余量/上界，该运算不回绕",
    },
    Allow {
        file: "execution/relocate.rs",
        line: "let dst = dst_base + within;",
        occurrence: 1,
        count: 2,
        class: "len-bounded",
        reason: "上游 checked_* 或前置比较已证余量/上界，该运算不回绕",
    },
    Allow {
        file: "execution/relocate.rs",
        line: "let mut grown = Vec::with_capacity(batch.len() + buf.len());",
        occurrence: 1,
        count: 1,
        class: "len-bounded",
        reason: "上游 checked_* 或前置比较已证余量/上界，该运算不回绕",
    },
    Allow {
        file: "execution/relocate.rs",
        line: "let off = src_base + within;",
        occurrence: 1,
        count: 1,
        class: "len-bounded",
        reason: "上游 checked_* 或前置比较已证余量/上界，该运算不回绕",
    },
    Allow {
        file: "execution/relocate.rs",
        line: "pending += len;",
        occurrence: 1,
        count: 1,
        class: "counter",
        reason: "循环/批量游标计数，不参与盘面几何",
    },
    Allow {
        file: "execution/relocate.rs",
        line: "src_base + total,",
        occurrence: 1,
        count: 1,
        class: "len-bounded",
        reason: "上游 checked_* 或前置比较已证余量/上界，该运算不回绕",
    },
    Allow {
        file: "execution/relocate.rs",
        line: "let dst = dst_base + within;",
        occurrence: 2,
        count: 2,
        class: "len-bounded",
        reason: "上游 checked_* 或前置比较已证余量/上界，该运算不回绕",
    },
    Allow {
        file: "fsid.rs",
        line: "const ALIGN: u64 = 64 * 1024;",
        occurrence: 1,
        count: 1,
        class: "const-expr",
        reason: "编译期常量或与运行数据无关的纯常量折叠",
    },
    Allow {
        file: "fsid.rs",
        line: "const BCACHEFS_MAGIC_OFFSET: u64 = 4096 + 24;",
        occurrence: 1,
        count: 1,
        class: "const-expr",
        reason: "编译期常量或与运行数据无关的纯常量折叠",
    },
    Allow {
        file: "fsid.rs",
        line: "const REISERFS_MAGIC_OFFSET: u64 = 65536 + 52;",
        occurrence: 1,
        count: 1,
        class: "const-expr",
        reason: "编译期常量或与运行数据无关的纯常量折叠",
    },
    Allow {
        file: "fsid.rs",
        line: "let blkszbits = sb[1024 + 0x0C] as u32;",
        occurrence: 1,
        count: 1,
        class: "index-in-bounds",
        reason: "下标来自 enumerate 索引、len 上界或已校验切片范围，不越界",
    },
    Allow {
        file: "fsid.rs",
        line: "let blocks = u32::from_le_bytes(sb[1024 + 0x24..1024 + 0x28].try_into().unwrap()) as u64;",
        occurrence: 1,
        count: 1,
        class: "index-in-bounds",
        reason: "下标来自 enumerate 索引、len 上界或已校验切片范围，不越界",
    },
    Allow {
        file: "fsops/attrs.rs",
        line: "text[at + \"uuid:\".len()..].split_whitespace().next()?.to_string()",
        occurrence: 1,
        count: 1,
        class: "index-in-bounds",
        reason: "下标来自 enumerate 索引、len 上界或已校验切片范围，不越界",
    },
    Allow {
        file: "fsops/grow.rs",
        line: "let inner = crate::fsid::identify(src, inner_base, len - rel).map_err(FsError::from)?;",
        occurrence: 1,
        count: 1,
        class: "len-bounded",
        reason: "上游 checked_* 或前置比较已证余量/上界，该运算不回绕",
    },
    Allow {
        file: "fsops/grow.rs",
        line: "scope: DeviceScope::Range(base + rel, len - rel),",
        occurrence: 1,
        count: 1,
        class: "len-bounded",
        reason: "上游 checked_* 或前置比较已证余量/上界，该运算不回绕",
    },
    Allow {
        file: "fsops/mount.rs",
        line: "&& b[i + 1..i + 4].iter().all(|c| (b'0'..=b'7').contains(c))",
        occurrence: 1,
        count: 1,
        class: "index-in-bounds",
        reason: "下标来自 enumerate 索引、len 上界或已校验切片范围，不越界",
    },
    Allow {
        file: "fsops/mount.rs",
        line: "&& i + 3 < b.len()",
        occurrence: 1,
        count: 1,
        class: "len-bounded",
        reason: "上游 checked_* 或前置比较已证余量/上界，该运算不回绕",
    },
    Allow {
        file: "fsops/mount.rs",
        line: "i += 1;",
        occurrence: 1,
        count: 1,
        class: "counter",
        reason: "循环/批量游标计数，不参与盘面几何",
    },
    Allow {
        file: "fsops/mount.rs",
        line: "i += 4;",
        occurrence: 1,
        count: 1,
        class: "counter",
        reason: "循环/批量游标计数，不参与盘面几何",
    },
    Allow {
        file: "fsops/mount.rs",
        line: "let fstype = rest.get(sep + 1)?;",
        occurrence: 1,
        count: 1,
        class: "index-in-bounds",
        reason: "下标来自 enumerate 索引、len 上界或已校验切片范围，不越界",
    },
    Allow {
        file: "fsops/mount.rs",
        line: "let source = rest.get(sep + 2)?;",
        occurrence: 1,
        count: 1,
        class: "index-in-bounds",
        reason: "下标来自 enumerate 索引、len 上界或已校验切片范围，不越界",
    },
    Allow {
        file: "fsops/mount.rs",
        line: "let v = (b[i + 1] - b'0') as u32 * 64 + (b[i + 2] - b'0') as u32 * 8 + (b[i + 3] - b'0') as u32;",
        occurrence: 1,
        count: 1,
        class: "index-in-bounds",
        reason: "下标来自 enumerate 索引、len 上界或已校验切片范围，不越界",
    },
    Allow {
        file: "fsops/scope.rs",
        line: "if len > src.size - off {",
        occurrence: 1,
        count: 1,
        class: "len-bounded",
        reason: "上游 checked_* 或前置比较已证余量/上界，该运算不回绕",
    },
    Allow {
        file: "fsops/swap.rs",
        line: "(-(3087 * 512), 1, 512),",
        occurrence: 1,
        count: 1,
        class: "const-expr",
        reason: "编译期常量或与运行数据无关的纯常量折叠",
    },
    Allow {
        file: "fsops/swap.rs",
        line: "(-(512 * K as i64), 256 * K, 768 * K),",
        occurrence: 1,
        count: 1,
        class: "const-expr",
        reason: "编译期常量或与运行数据无关的纯常量折叠",
    },
    Allow {
        file: "fsops/swap.rs",
        line: "(-(M as i64), 1, 4 * K),",
        occurrence: 1,
        count: 1,
        class: "const-expr",
        reason: "编译期常量或与运行数据无关的纯常量折叠",
    },
    Allow {
        file: "fsops/swap.rs",
        line: "(0, 1, 512 * K),",
        occurrence: 1,
        count: 1,
        class: "const-expr",
        reason: "编译期常量或与运行数据无关的纯常量折叠",
    },
    Allow {
        file: "fsops/swap.rs",
        line: "(2 * M as i64, 1, 4 * K),",
        occurrence: 1,
        count: 1,
        class: "const-expr",
        reason: "编译期常量或与运行数据无关的纯常量折叠",
    },
    Allow {
        file: "fsops/swap.rs",
        line: "(256 * G as i64, 1, 4 * K),",
        occurrence: 1,
        count: 1,
        class: "const-expr",
        reason: "编译期常量或与运行数据无关的纯常量折叠",
    },
    Allow {
        file: "fsops/swap.rs",
        line: "(64 * M as i64, 1, 4 * K),",
        occurrence: 1,
        count: 1,
        class: "const-expr",
        reason: "编译期常量或与运行数据无关的纯常量折叠",
    },
    Allow {
        file: "fsops/swap.rs",
        line: "const G: u64 = 1024 * M;",
        occurrence: 1,
        count: 1,
        class: "const-expr",
        reason: "编译期常量或与运行数据无关的纯常量折叠",
    },
    Allow {
        file: "fsops/swap.rs",
        line: "const M: u64 = 1024 * K;",
        occurrence: 1,
        count: 1,
        class: "const-expr",
        reason: "编译期常量或与运行数据无关的纯常量折叠",
    },
    Allow {
        file: "fsops/swap.rs",
        line: "f + unit",
        occurrence: 1,
        count: 1,
        class: "len-bounded",
        reason: "上游 checked_* 或前置比较已证余量/上界，该运算不回绕",
    },
    Allow {
        file: "fsops/swap.rs",
        line: "floor_to_unit(floor_to_unit(part_end + off, rounding as i64), ss_i)",
        occurrence: 1,
        count: 1,
        class: "len-bounded",
        reason: "上游 checked_* 或前置比较已证余量/上界，该运算不回绕",
    },
    Allow {
        file: "fsops/swap.rs",
        line: "if s < e && out.last() != Some(&(s, e - s)) {",
        occurrence: 1,
        count: 1,
        class: "len-bounded",
        reason: "上游 checked_* 或前置比较已证余量/上界，该运算不回绕",
    },
    Allow {
        file: "fsops/swap.rs",
        line: "left -= n as u64;",
        occurrence: 1,
        count: 1,
        class: "counter",
        reason: "循环/批量游标计数，不参与盘面几何",
    },
    Allow {
        file: "fsops/swap.rs",
        line: "let end = ceil_to_unit(start + len as i64, ss_i);",
        occurrence: 1,
        count: 1,
        class: "len-bounded",
        reason: "上游 checked_* 或前置比较已证余量/上界，该运算不回绕",
    },
    Allow {
        file: "fsops/swap.rs",
        line: "out.push((s, e - s));",
        occurrence: 1,
        count: 1,
        class: "len-bounded",
        reason: "上游 checked_* 或前置比较已证余量/上界，该运算不回绕",
    },
    Allow {
        file: "fsops/swap.rs",
        line: "v.div_euclid(unit) * unit",
        occurrence: 1,
        count: 1,
        class: "len-bounded",
        reason: "上游 checked_* 或前置比较已证余量/上界，该运算不回绕",
    },
    Allow {
        file: "geometry.rs",
        line: ".map(|(i, e)| (e.starting_lba, e.ending_lba, i + 1))",
        occurrence: 1,
        count: 1,
        class: "index-in-bounds",
        reason: "下标来自 enumerate 索引、len 上界或已校验切片范围，不越界",
    },
    Allow {
        file: "geometry.rs",
        line: "i + 1",
        occurrence: 1,
        count: 2,
        class: "index-in-bounds",
        reason: "下标来自 enumerate 索引、len 上界或已校验切片范围，不越界",
    },
    Allow {
        file: "geometry.rs",
        line: "i + 1,",
        occurrence: 1,
        count: 1,
        class: "index-in-bounds",
        reason: "下标来自 enumerate 索引、len 上界或已校验切片范围，不越界",
    },
    Allow {
        file: "geometry.rs",
        line: "index + 1,",
        occurrence: 1,
        count: 3,
        class: "index-in-bounds",
        reason: "下标来自 enumerate 索引、len 上界或已校验切片范围，不越界",
    },
    Allow {
        file: "geometry.rs",
        line: "i + 1",
        occurrence: 2,
        count: 2,
        class: "index-in-bounds",
        reason: "下标来自 enumerate 索引、len 上界或已校验切片范围，不越界",
    },
    Allow {
        file: "geometry.rs",
        line: "index + 1,",
        occurrence: 2,
        count: 3,
        class: "index-in-bounds",
        reason: "下标来自 enumerate 索引、len 上界或已校验切片范围，不越界",
    },
    Allow {
        file: "geometry.rs",
        line: "index + 1,",
        occurrence: 3,
        count: 3,
        class: "index-in-bounds",
        reason: "下标来自 enumerate 索引、len 上界或已校验切片范围，不越界",
    },
    Allow {
        file: "gpt_policy.rs",
        line: ".map(|(i, e)| ((i + 1) as u32, e.ending_lba)),",
        occurrence: 1,
        count: 1,
        class: "index-in-bounds",
        reason: "下标来自 enumerate 索引、len 上界或已校验切片范围，不越界",
    },
    Allow {
        file: "gpt_policy.rs",
        line: ".map(|p| (p.num, p.start_lba as u64 + p.size_lba as u64 - 1)),",
        occurrence: 1,
        count: 1,
        class: "len-bounded",
        reason: "上游 checked_* 或前置比较已证余量/上界，该运算不回绕",
    },
    Allow {
        file: "gpt_policy.rs",
        line: "Ok((fresh, (index + 1) as u32))",
        occurrence: 1,
        count: 1,
        class: "index-in-bounds",
        reason: "下标来自 enumerate 索引、len 上界或已校验切片范围，不越界",
    },
    Allow {
        file: "gpt_policy.rs",
        line: "Ok((index + 1) as u32)",
        occurrence: 1,
        count: 1,
        class: "index-in-bounds",
        reason: "下标来自 enumerate 索引、len 上界或已校验切片范围，不越界",
    },
    Allow {
        file: "lvm.rs",
        line: "&& t[i + 1..t.len() - 1].bytes().all(|b| b.is_ascii_digit())",
        occurrence: 1,
        count: 1,
        class: "index-in-bounds",
        reason: "下标来自 enumerate 索引、len 上界或已校验切片范围，不越界",
    },
    Allow {
        file: "lvm.rs",
        line: "rows += 1;",
        occurrence: 1,
        count: 1,
        class: "counter",
        reason: "循环/批量游标计数，不参与盘面几何",
    },
    Allow {
        file: "movepart/copy.rs",
        line: "let len = e.ending_lba - e.starting_lba + 1;",
        occurrence: 1,
        count: 1,
        class: "len-bounded",
        reason: "上游 checked_* 或前置比较已证余量/上界，该运算不回绕",
    },
    Allow {
        file: "movepart/copy.rs",
        line: "return Err(RunOutcome::refused(format!(\"copy target overlaps partition #{}\", i + 1)));",
        occurrence: 1,
        count: 1,
        class: "display-only",
        reason: "结果仅进入输出串/日志/错误文案，不参与任何判定分支或盘面几何",
    },
    Allow {
        file: "movepart/plan.rs",
        line: ".map(|(i, e)| ((i + 1) as u32, e))",
        occurrence: 1,
        count: 1,
        class: "index-in-bounds",
        reason: "下标来自 enumerate 索引、len 上界或已校验切片范围，不越界",
    },
    Allow {
        file: "movepart/plan.rs",
        line: "Ok(mib * crate::support::MIB_BYTES)",
        occurrence: 1,
        count: 1,
        class: "len-bounded",
        reason: "上游 checked_* 或前置比较已证余量/上界，该运算不回绕",
    },
    Allow {
        file: "movepart/plan.rs",
        line: "entry.ending_lba - entry.starting_lba + 1,",
        occurrence: 1,
        count: 1,
        class: "len-bounded",
        reason: "上游 checked_* 或前置比较已证余量/上界，该运算不回绕",
    },
    Allow {
        file: "movepart/plan.rs",
        line: "for &(bpn, bd, be) in dst_ranges.iter().skip(i + 1) {",
        occurrence: 1,
        count: 1,
        class: "index-in-bounds",
        reason: "下标来自 enumerate 索引、len 上界或已校验切片范围，不越界",
    },
    Allow {
        file: "movepart/plan.rs",
        line: "for m in r.entries.iter().skip(i + 1) {",
        occurrence: 1,
        count: 1,
        class: "index-in-bounds",
        reason: "下标来自 enumerate 索引、len 上界或已校验切片范围，不越界",
    },
    Allow {
        file: "movepart/plan.rs",
        line: "if shift < movable[0].1.starting_lba - target_end {",
        occurrence: 1,
        count: 1,
        class: "len-bounded",
        reason: "上游 checked_* 或前置比较已证余量/上界，该运算不回绕",
    },
    Allow {
        file: "movepart/plan.rs",
        line: "let delta = new_bytes - old_bytes;",
        occurrence: 1,
        count: 1,
        class: "len-bounded",
        reason: "上游 checked_* 或前置比较已证余量/上界，该运算不回绕",
    },
    Allow {
        file: "movepart/plan.rs",
        line: "let len = e.ending_lba - e.starting_lba + 1;",
        occurrence: 1,
        count: 3,
        class: "len-bounded",
        reason: "上游 checked_* 或前置比较已证余量/上界，该运算不回绕",
    },
    Allow {
        file: "movepart/plan.rs",
        line: "let n = ei as u32 + 1;",
        occurrence: 1,
        count: 2,
        class: "index-in-bounds",
        reason: "下标来自 enumerate 索引、len 上界或已校验切片范围，不越界",
    },
    Allow {
        file: "movepart/plan.rs",
        line: "let len = e.ending_lba - e.starting_lba + 1;",
        occurrence: 2,
        count: 3,
        class: "len-bounded",
        reason: "上游 checked_* 或前置比较已证余量/上界，该运算不回绕",
    },
    Allow {
        file: "movepart/plan.rs",
        line: "let n = ei as u32 + 1;",
        occurrence: 2,
        count: 2,
        class: "index-in-bounds",
        reason: "下标来自 enumerate 索引、len 上界或已校验切片范围，不越界",
    },
    Allow {
        file: "movepart/plan.rs",
        line: "let len = e.ending_lba - e.starting_lba + 1;",
        occurrence: 3,
        count: 3,
        class: "len-bounded",
        reason: "上游 checked_* 或前置比较已证余量/上界，该运算不回绕",
    },
    Allow {
        file: "movepart/shift.rs",
        line: "&crate::dev::part_dev_hint(src, part, r.old.0 * ss),",
        occurrence: 1,
        count: 1,
        class: "display-only",
        reason: "结果仅进入输出串/日志/错误文案，不参与任何判定分支或盘面几何",
    },
    Allow {
        file: "movepart/shift.rs",
        line: "ge.ending_lba - ge.starting_lba + 1,",
        occurrence: 1,
        count: 1,
        class: "len-bounded",
        reason: "上游 checked_* 或前置比较已证余量/上界，该运算不回绕",
    },
    Allow {
        file: "movepart/shift.rs",
        line: "let grow_idx = (plan.grow_part - 1) as usize;",
        occurrence: 1,
        count: 1,
        class: "index-in-bounds",
        reason: "下标来自 enumerate 索引、len 上界或已校验切片范围，不越界",
    },
    Allow {
        file: "online/guard.rs",
        line: "let digits_end = part_name.bytes().rposition(|b| !b.is_ascii_digit()).map(|i| i + 1);",
        occurrence: 1,
        count: 1,
        class: "index-in-bounds",
        reason: "下标来自 enumerate 索引、len 上界或已校验切片范围，不越界",
    },
    Allow {
        file: "planner/chunk.rs",
        line: "let offset = i * chunk_lba;",
        occurrence: 1,
        count: 1,
        class: "len-bounded",
        reason: "上游 checked_* 或前置比较已证余量/上界，该运算不回绕",
    },
    Allow {
        file: "planner/mod.rs",
        line: "&& matches!(steps.get(i - 1), Some(PlanStep::WriteTable(_)))",
        occurrence: 1,
        count: 1,
        class: "index-in-bounds",
        reason: "下标来自 enumerate 索引、len 上界或已校验切片范围，不越界",
    },
    Allow {
        file: "planner/mod.rs",
        line: "PlanStep::Relocate { .. } => matches!(steps.get(i + 1), Some(PlanStep::WriteTable(_))),",
        occurrence: 1,
        count: 1,
        class: "index-in-bounds",
        reason: "下标来自 enumerate 索引、len 上界或已校验切片范围，不越界",
    },
    Allow {
        file: "planner/mod.rs",
        line: "self.last_lba - self.first_lba + 1",
        occurrence: 1,
        count: 1,
        class: "len-bounded",
        reason: "上游 checked_* 或前置比较已证余量/上界，该运算不回绕",
    },
    Allow {
        file: "planner/mod.rs",
        line: "steps.get(i - 2),",
        occurrence: 1,
        count: 1,
        class: "index-in-bounds",
        reason: "下标来自 enumerate 索引、len 上界或已校验切片范围，不越界",
    },
    Allow {
        file: "platform/evidence_store.rs",
        line: "let n = unsafe { libc::write(fd, bytes[written..].as_ptr().cast(), bytes.len() - written) };",
        occurrence: 1,
        count: 1,
        class: "len-bounded",
        reason: "上游 checked_* 或前置比较已证余量/上界，该运算不回绕",
    },
    Allow {
        file: "platform/evidence_store.rs",
        line: "off += n as libc::off_t;",
        occurrence: 1,
        count: 1,
        class: "counter",
        reason: "循环/批量游标计数，不参与盘面几何",
    },
    Allow {
        file: "platform/evidence_store.rs",
        line: "written += n as usize;",
        occurrence: 1,
        count: 1,
        class: "counter",
        reason: "循环/批量游标计数，不参与盘面几何",
    },
    Allow {
        file: "recovery/journal.rs",
        line: "(reloc.entry_index + 1, 0),",
        occurrence: 1,
        count: 1,
        class: "counter",
        reason: "循环/批量游标计数，不参与盘面几何",
    },
    Allow {
        file: "recovery/journal.rs",
        line: "progress.completed_steps + 1,",
        occurrence: 1,
        count: 1,
        class: "counter",
        reason: "循环/批量游标计数，不参与盘面几何",
    },
    Allow {
        file: "recovery/record.rs",
        line: "let covered = u64::from(self.sectors) * ss;",
        occurrence: 1,
        count: 1,
        class: "len-bounded",
        reason: "上游 checked_* 或前置比较已证余量/上界，该运算不回绕",
    },
    Allow {
        file: "recovery/record.rs",
        line: "let mut input = Vec::with_capacity(1 + 4 + 8 * 4);",
        occurrence: 1,
        count: 1,
        class: "const-expr",
        reason: "编译期常量或与运行数据无关的纯常量折叠",
    },
    Allow {
        file: "recovery/record.rs",
        line: "let previous = covered - ss;",
        occurrence: 1,
        count: 1,
        class: "len-bounded",
        reason: "上游 checked_* 或前置比较已证余量/上界，该运算不回绕",
    },
    Allow {
        file: "recovery/record.rs",
        line: "let total_len = HEADER_LEN as u64 + payload.len() as u64;",
        occurrence: 1,
        count: 1,
        class: "len-bounded",
        reason: "上游 checked_* 或前置比较已证余量/上界，该运算不回绕",
    },
    Allow {
        file: "recovery/record.rs",
        line: "let total_len = HEADER_LEN as u64 + u64::from(payload_len);",
        occurrence: 1,
        count: 1,
        class: "len-bounded",
        reason: "上游 checked_* 或前置比较已证余量/上界，该运算不回绕",
    },
    Allow {
        file: "recovery/record.rs",
        line: "pub(crate) const MAX_RECORD_BYTES: u64 = 16 * 1024 * 1024;",
        occurrence: 1,
        count: 1,
        class: "const-expr",
        reason: "编译期常量或与运行数据无关的纯常量折叠",
    },
    Allow {
        file: "recovery/rollback.rs",
        line: "&& let Err(first) = tears.record(offset + tear.cut as u64)",
        occurrence: 1,
        count: 1,
        class: "len-bounded",
        reason: "上游 checked_* 或前置比较已证余量/上界，该运算不回绕",
    },
    Allow {
        file: "recovery/rollback.rs",
        line: "buf[from..to].copy_from_slice(&bytes[at..at + (to - from)]);",
        occurrence: 1,
        count: 1,
        class: "index-in-bounds",
        reason: "下标来自 enumerate 索引、len 上界或已校验切片范围，不越界",
    },
    Allow {
        file: "recovery/rollback.rs",
        line: "let fits = |p: usize, s: usize, len: usize| p + s >= len;",
        occurrence: 1,
        count: 1,
        class: "len-bounded",
        reason: "上游 checked_* 或前置比较已证余量/上界，该运算不回绕",
    },
    Allow {
        file: "recovery/rollback.rs",
        line: "let mut kept: Vec<(u64, Vec<u8>)> = Vec::with_capacity(image.len() + 2);",
        occurrence: 1,
        count: 1,
        class: "len-bounded",
        reason: "上游 checked_* 或前置比较已证余量/上界，该运算不回绕",
    },
    Allow {
        file: "recovery/rollback.rs",
        line: "offset + tear.cut as u64",
        occurrence: 1,
        count: 1,
        class: "len-bounded",
        reason: "上游 checked_* 或前置比较已证余量/上界，该运算不回绕",
    },
    Allow {
        file: "recovery/rollback.rs",
        line: "out.push((entry.starting_lba, entry.ending_lba - entry.starting_lba + 1));",
        occurrence: 1,
        count: 1,
        class: "len-bounded",
        reason: "上游 checked_* 或前置比较已证余量/上界，该运算不回绕",
    },
    Allow {
        file: "support.rs",
        line: "const LOG_MAX_BYTES: u64 = 8 * MIB_BYTES;",
        occurrence: 1,
        count: 1,
        class: "const-expr",
        reason: "编译期常量或与运行数据无关的纯常量折叠",
    },
    Allow {
        file: "support.rs",
        line: "done += take as u64;",
        occurrence: 1,
        count: 1,
        class: "counter",
        reason: "循环/批量游标计数，不参与盘面几何",
    },
    Allow {
        file: "support.rs",
        line: "e1 - 1",
        occurrence: 1,
        count: 1,
        class: "len-bounded",
        reason: "上游 checked_* 或前置比较已证余量/上界，该运算不回绕",
    },
    Allow {
        file: "support.rs",
        line: "eprintln!(\"aligned to {align}: end {end} -> {}\", e1 - 1);",
        occurrence: 1,
        count: 1,
        class: "display-only",
        reason: "结果仅进入输出串/日志/错误文案，不参与任何判定分支或盘面几何",
    },
    Allow {
        file: "support.rs",
        line: "eprintln!(\"note: size {bytes} is not a multiple of the {ss}-byte sector — {action} {} bytes\", bytes / ss * ss);",
        occurrence: 1,
        count: 1,
        class: "display-only",
        reason: "结果仅进入输出串/日志/错误文案，不参与任何判定分支或盘面几何",
    },
    Allow {
        file: "support.rs",
        line: "free.push((cur, s - 1));",
        occurrence: 1,
        count: 1,
        class: "len-bounded",
        reason: "上游 checked_* 或前置比较已证余量/上界，该运算不回绕",
    },
    Allow {
        file: "support.rs",
        line: "if (i + 1) as u32 == part || crate::table::is_unused(o) {",
        occurrence: 1,
        count: 1,
        class: "index-in-bounds",
        reason: "下标来自 enumerate 索引、len 上界或已校验切片范围，不越界",
    },
    Allow {
        file: "support.rs",
        line: "if e1 - 1 != end {",
        occurrence: 1,
        count: 1,
        class: "len-bounded",
        reason: "上游 checked_* 或前置比较已证余量/上界，该运算不回绕",
    },
    Allow {
        file: "support.rs",
        line: "if e1 == 0 || start > e1 - 1 {",
        occurrence: 1,
        count: 1,
        class: "len-bounded",
        reason: "上游 checked_* 或前置比较已证余量/上界，该运算不回绕",
    },
    Allow {
        file: "support.rs",
        line: "let end = p.start_lba as u64 + p.size_lba as u64;",
        occurrence: 1,
        count: 1,
        class: "len-bounded",
        reason: "上游 checked_* 或前置比较已证余量/上界，该运算不回绕",
    },
    Allow {
        file: "support.rs",
        line: "let mut out = String::with_capacity(bytes.len() * 2);",
        occurrence: 1,
        count: 1,
        class: "len-bounded",
        reason: "上游 checked_* 或前置比较已证余量/上界，该运算不回绕",
    },
    Allow {
        file: "support.rs",
        line: "let take = (len - done).min(CHUNK) as usize;",
        occurrence: 1,
        count: 1,
        class: "len-bounded",
        reason: "上游 checked_* 或前置比较已证余量/上界，该运算不回绕",
    },
    Allow {
        file: "support.rs",
        line: "pub(crate) const DEFAULT_CHUNK_BYTES: u64 = 4 * MIB_BYTES;",
        occurrence: 1,
        count: 1,
        class: "const-expr",
        reason: "编译期常量或与运行数据无关的纯常量折叠",
    },
    Allow {
        file: "support.rs",
        line: "table::last_sector_of(p.start_lba as u64 + p.size_lba as u64)",
        occurrence: 1,
        count: 1,
        class: "len-bounded",
        reason: "上游 checked_* 或前置比较已证余量/上界，该运算不回绕",
    },
    Allow {
        file: "support.rs",
        line: "text[i] = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).ok()?;",
        occurrence: 1,
        count: 1,
        class: "index-in-bounds",
        reason: "下标来自 enumerate 索引、len 上界或已校验切片范围，不越界",
    },
    Allow {
        file: "table/create.rs",
        line: "(\"primary GPT header\", ss, 2 * ss),",
        occurrence: 1,
        count: 1,
        class: "len-bounded",
        reason: "上游 checked_* 或前置比较已证余量/上界，该运算不回绕",
    },
    Allow {
        file: "table/create.rs",
        line: "(\"primary entry array\", 2 * ss, (2 + span) * ss),",
        occurrence: 1,
        count: 1,
        class: "len-bounded",
        reason: "上游 checked_* 或前置比较已证余量/上界，该运算不回绕",
    },
    Allow {
        file: "table/create.rs",
        line: "first_usable_lba: PRIMARY_ARRAY_LBA + span,",
        occurrence: 1,
        count: 2,
        class: "len-bounded",
        reason: "上游 checked_* 或前置比较已证余量/上界，该运算不回绕",
    },
    Allow {
        file: "table/create.rs",
        line: "format!(\"GPT partition {}\", i + 1),",
        occurrence: 1,
        count: 1,
        class: "display-only",
        reason: "结果仅进入输出串/日志/错误文案，不参与任何判定分支或盘面几何",
    },
    Allow {
        file: "table/create.rs",
        line: "last_usable_lba: last_lba - span - 1,",
        occurrence: 1,
        count: 2,
        class: "len-bounded",
        reason: "上游 checked_* 或前置比较已证余量/上界，该运算不回绕",
    },
    Allow {
        file: "table/create.rs",
        line: "first_usable_lba: PRIMARY_ARRAY_LBA + span,",
        occurrence: 2,
        count: 2,
        class: "len-bounded",
        reason: "上游 checked_* 或前置比较已证余量/上界，该运算不回绕",
    },
    Allow {
        file: "table/create.rs",
        line: "last_usable_lba: last_lba - span - 1,",
        occurrence: 2,
        count: 2,
        class: "len-bounded",
        reason: "上游 checked_* 或前置比较已证余量/上界，该运算不回绕",
    },
    Allow {
        file: "table/entry_array.rs",
        line: "let byte_len = entry_count as u64 * entry_size as u64;",
        occurrence: 1,
        count: 1,
        class: "len-bounded",
        reason: "上游 checked_* 或前置比较已证余量/上界，该运算不回绕",
    },
    Allow {
        file: "table/entry_array.rs",
        line: "pub const MAX_ARRAY_BYTES: u64 = 16 * 1024 * 1024;",
        occurrence: 1,
        count: 1,
        class: "const-expr",
        reason: "编译期常量或与运行数据无关的纯常量折叠",
    },
    Allow {
        file: "table/entry_array.rs",
        line: "self.entry_count as u64 * self.entry_size as u64",
        occurrence: 1,
        count: 1,
        class: "len-bounded",
        reason: "上游 checked_* 或前置比较已证余量/上界，该运算不回绕",
    },
    Allow {
        file: "table/entry_array.rs",
        line: "self.lba_span() * self.sector_size",
        occurrence: 1,
        count: 1,
        class: "len-bounded",
        reason: "上游 checked_* 或前置比较已证余量/上界，该运算不回绕",
    },
    Allow {
        file: "table/gpt_parse.rs",
        line: "(0..n).map(|i| parse_entry(&raw[i * es..i * es + ENTRY_FIXED_BYTES])).collect()",
        occurrence: 1,
        count: 1,
        class: "index-in-bounds",
        reason: "下标来自 enumerate 索引、len 上界或已校验切片范围，不越界",
    },
    Allow {
        file: "table/gpt_parse.rs",
        line: "if header.first_usable_lba < 2 + span {",
        occurrence: 1,
        count: 1,
        class: "len-bounded",
        reason: "上游 checked_* 或前置比较已证余量/上界，该运算不回绕",
    },
    Allow {
        file: "table/gpt_parse.rs",
        line: "if header.last_usable_lba > file_last_lba - span - 1 {",
        occurrence: 1,
        count: 1,
        class: "len-bounded",
        reason: "上游 checked_* 或前置比较已证余量/上界，该运算不回绕",
    },
    Allow {
        file: "table/gpt_parse.rs",
        line: "if let Err(e) = src.read_at(file_last_lba * ss, &mut sec) {",
        occurrence: 1,
        count: 1,
        class: "len-bounded",
        reason: "上游 checked_* 或前置比较已证余量/上界，该运算不回绕",
    },
    Allow {
        file: "table/gpt_parse.rs",
        line: "if src.size < ss * 2 {",
        occurrence: 1,
        count: 5,
        class: "len-bounded",
        reason: "上游 checked_* 或前置比较已证余量/上界，该运算不回绕",
    },
    Allow {
        file: "table/gpt_parse.rs",
        line: "if src.size < ss + GPT_SIGNATURE.len() as u64 {",
        occurrence: 1,
        count: 1,
        class: "len-bounded",
        reason: "上游 checked_* 或前置比较已证余量/上界，该运算不回绕",
    },
    Allow {
        file: "table/gpt_parse.rs",
        line: "let index = i + 1;",
        occurrence: 1,
        count: 1,
        class: "index-in-bounds",
        reason: "下标来自 enumerate 索引、len 上界或已校验切片范围，不越界",
    },
    Allow {
        file: "table/gpt_parse.rs",
        line: "let size = rd_u32(&lba0, mbr_entry_range(0).start + offset_of!(MbrEntryLayout, size_in_lba));",
        occurrence: 1,
        count: 1,
        class: "index-in-bounds",
        reason: "下标来自 enumerate 索引、len 上界或已校验切片范围，不越界",
    },
    Allow {
        file: "table/gpt_parse.rs",
        line: "name_bytes.copy_from_slice(&b[ENTRY_NAME_OFFSET..ENTRY_NAME_OFFSET + ENTRY_NAME_LEN]);",
        occurrence: 1,
        count: 1,
        class: "index-in-bounds",
        reason: "下标来自 enumerate 索引、len 上界或已校验切片范围，不越界",
    },
    Allow {
        file: "table/gpt_parse.rs",
        line: "src.read_at(last * ss, &mut sector)?;",
        occurrence: 1,
        count: 1,
        class: "len-bounded",
        reason: "上游 checked_* 或前置比较已证余量/上界，该运算不回绕",
    },
    Allow {
        file: "table/gpt_parse.rs",
        line: "tmp[crc_at..crc_at + GPT_CRC_BYTES].fill(0);",
        occurrence: 1,
        count: 1,
        class: "index-in-bounds",
        reason: "下标来自 enumerate 索引、len 上界或已校验切片范围，不越界",
    },
    Allow {
        file: "table/gpt_parse.rs",
        line: "u32::from_le_bytes(b[off..off + 4].try_into().unwrap())",
        occurrence: 1,
        count: 1,
        class: "index-in-bounds",
        reason: "下标来自 enumerate 索引、len 上界或已校验切片范围，不越界",
    },
    Allow {
        file: "table/gpt_parse.rs",
        line: "u64::from_le_bytes(b[off..off + 8].try_into().unwrap())",
        occurrence: 1,
        count: 1,
        class: "index-in-bounds",
        reason: "下标来自 enumerate 索引、len 上界或已校验切片范围，不越界",
    },
    Allow {
        file: "table/gpt_parse.rs",
        line: "if src.size < ss * 2 {",
        occurrence: 2,
        count: 5,
        class: "len-bounded",
        reason: "上游 checked_* 或前置比较已证余量/上界，该运算不回绕",
    },
    Allow {
        file: "table/gpt_parse.rs",
        line: "if src.size < ss * 2 {",
        occurrence: 3,
        count: 5,
        class: "len-bounded",
        reason: "上游 checked_* 或前置比较已证余量/上界，该运算不回绕",
    },
    Allow {
        file: "table/gpt_parse.rs",
        line: "if src.size < ss * 2 {",
        occurrence: 4,
        count: 5,
        class: "len-bounded",
        reason: "上游 checked_* 或前置比较已证余量/上界，该运算不回绕",
    },
    Allow {
        file: "table/gpt_parse.rs",
        line: "if src.size < ss * 2 {",
        occurrence: 5,
        count: 5,
        class: "len-bounded",
        reason: "上游 checked_* 或前置比较已证余量/上界，该运算不回绕",
    },
    Allow {
        file: "table/gpt_write.rs",
        line: "b[ENTRY_NAME_OFFSET..ENTRY_NAME_OFFSET + ENTRY_NAME_LEN].copy_from_slice(&encode_name(e));",
        occurrence: 1,
        count: 1,
        class: "index-in-bounds",
        reason: "下标来自 enumerate 索引、len 上界或已校验切片范围，不越界",
    },
    Allow {
        file: "table/gpt_write.rs",
        line: "b[off..off + ENTRY_FIXED_BYTES].copy_from_slice(&serialize_entry(e));",
        occurrence: 1,
        count: 1,
        class: "index-in-bounds",
        reason: "下标来自 enumerate 索引、len 上界或已校验切片范围，不越界",
    },
    Allow {
        file: "table/gpt_write.rs",
        line: "let off = i * entry_size;",
        occurrence: 1,
        count: 1,
        class: "index-in-bounds",
        reason: "下标来自 enumerate 索引、len 上界或已校验切片范围，不越界",
    },
    Allow {
        file: "table/gpt_write.rs",
        line: "out[i * 2..i * 2 + 2].copy_from_slice(&u.to_le_bytes());",
        occurrence: 1,
        count: 1,
        class: "index-in-bounds",
        reason: "下标来自 enumerate 索引、len 上界或已校验切片范围，不越界",
    },
    Allow {
        file: "table/layout.rs",
        line: "assert!(MBR_ENTRY_SLOTS * MBR_ENTRY_BYTES == MBR_SIGNATURE_AT - MBR_ENTRY_BASE);",
        occurrence: 1,
        count: 1,
        class: "const-expr",
        reason: "编译期常量或与运行数据无关的纯常量折叠",
    },
    Allow {
        file: "table/layout.rs",
        line: "assert!(MBR_SIGNATURE_AT + 2 == MBR_SIGNED_BYTES);",
        occurrence: 1,
        count: 1,
        class: "const-expr",
        reason: "编译期常量或与运行数据无关的纯常量折叠",
    },
    Allow {
        file: "table/layout.rs",
        line: "assert!(offset_of!(GptEntryLayout, name) + GPT_NAME_BYTES == GPT_ENTRY_FIXED_BYTES);",
        occurrence: 1,
        count: 1,
        class: "const-expr",
        reason: "编译期常量或与运行数据无关的纯常量折叠",
    },
    Allow {
        file: "table/layout.rs",
        line: "assert!(offset_of!(GptHeaderLayout, partition_entry_array_crc32) + GPT_CRC_BYTES == GPT_HEADER_FIXED_BYTES);",
        occurrence: 1,
        count: 1,
        class: "const-expr",
        reason: "编译期常量或与运行数据无关的纯常量折叠",
    },
    Allow {
        file: "table/layout.rs",
        line: "b[off..off + 4].copy_from_slice(&v.to_le_bytes());",
        occurrence: 1,
        count: 1,
        class: "index-in-bounds",
        reason: "下标来自 enumerate 索引、len 上界或已校验切片范围，不越界",
    },
    Allow {
        file: "table/layout.rs",
        line: "b[off..off + 8].copy_from_slice(&v.to_le_bytes());",
        occurrence: 1,
        count: 1,
        class: "index-in-bounds",
        reason: "下标来自 enumerate 索引、len 上界或已校验切片范围，不越界",
    },
    Allow {
        file: "table/layout.rs",
        line: "let start = MBR_ENTRY_BASE + slot * MBR_ENTRY_BYTES;",
        occurrence: 1,
        count: 1,
        class: "index-in-bounds",
        reason: "下标来自 enumerate 索引、len 上界或已校验切片范围，不越界",
    },
    Allow {
        file: "table/layout.rs",
        line: "pub(crate) const GPT_NAME_BYTES: usize = GPT_NAME_UNITS * 2;",
        occurrence: 1,
        count: 1,
        class: "const-expr",
        reason: "编译期常量或与运行数据无关的纯常量折叠",
    },
    Allow {
        file: "table/layout.rs",
        line: "start..start + MBR_ENTRY_BYTES",
        occurrence: 1,
        count: 1,
        class: "index-in-bounds",
        reason: "下标来自 enumerate 索引、len 上界或已校验切片范围，不越界",
    },
    Allow {
        file: "table/mbr.rs",
        line: "Ok((lba0, (i + 1) as u32))",
        occurrence: 1,
        count: 1,
        class: "index-in-bounds",
        reason: "下标来自 enumerate 索引、len 上界或已校验切片范围，不越界",
    },
    Allow {
        file: "table/mbr.rs",
        line: "damage.push(MbrDamage::PastEnd { num: i + 1, start, size, total_sectors });",
        occurrence: 1,
        count: 1,
        class: "index-in-bounds",
        reason: "下标来自 enumerate 索引、len 上界或已校验切片范围，不越界",
    },
    Allow {
        file: "table/mbr.rs",
        line: "damage.push(MbrDamage::StartZero { num: i + 1 });",
        occurrence: 1,
        count: 1,
        class: "index-in-bounds",
        reason: "下标来自 enumerate 索引、len 上界或已校验切片范围，不越界",
    },
    Allow {
        file: "table/mbr.rs",
        line: "for j in (i + 1)..parts.len() {",
        occurrence: 1,
        count: 1,
        class: "index-in-bounds",
        reason: "下标来自 enumerate 索引、len 上界或已校验切片范围，不越界",
    },
    Allow {
        file: "table/mbr.rs",
        line: "i + 1",
        occurrence: 1,
        count: 2,
        class: "index-in-bounds",
        reason: "下标来自 enumerate 索引、len 上界或已校验切片范围，不越界",
    },
    Allow {
        file: "table/mbr.rs",
        line: "if MDOS_CONTAINER_TYPES.contains(&lba0[off + 4]) {",
        occurrence: 1,
        count: 1,
        class: "index-in-bounds",
        reason: "下标来自 enumerate 索引、len 上界或已校验切片范围，不越界",
    },
    Allow {
        file: "table/mbr.rs",
        line: "if i + 1 == part {",
        occurrence: 1,
        count: 1,
        class: "index-in-bounds",
        reason: "下标来自 enumerate 索引、len 上界或已校验切片范围，不越界",
    },
    Allow {
        file: "table/mbr.rs",
        line: "if lba0[mbr_entry_range(i).start + 4] != 0 {",
        occurrence: 1,
        count: 1,
        class: "index-in-bounds",
        reason: "下标来自 enumerate 索引、len 上界或已校验切片范围，不越界",
    },
    Allow {
        file: "table/mbr.rs",
        line: "if lba0[off + 4] == 0 {",
        occurrence: 1,
        count: 1,
        class: "index-in-bounds",
        reason: "下标来自 enumerate 索引、len 上界或已校验切片范围，不越界",
    },
    Allow {
        file: "table/mbr.rs",
        line: "if lba0[off + 4] == PROT_MBR_TYPE {",
        occurrence: 1,
        count: 1,
        class: "index-in-bounds",
        reason: "下标来自 enumerate 索引、len 上界或已校验切片范围，不越界",
    },
    Allow {
        file: "table/mbr.rs",
        line: "if rd_u32(&lba0[off..off + 16], 12) == 0 {",
        occurrence: 1,
        count: 2,
        class: "index-in-bounds",
        reason: "下标来自 enumerate 索引、len 上界或已校验切片范围，不越界",
    },
    Allow {
        file: "table/mbr.rs",
        line: "if start + new_size_lba as u64 - 1 > MBR_LBA_MAX {",
        occurrence: 1,
        count: 1,
        class: "len-bounded",
        reason: "上游 checked_* 或前置比较已证余量/上界，该运算不回绕",
    },
    Allow {
        file: "table/mbr.rs",
        line: "if start + new_size_lba as u64 > total_sectors {",
        occurrence: 1,
        count: 1,
        class: "len-bounded",
        reason: "上游 checked_* 或前置比较已证余量/上界，该运算不回绕",
    },
    Allow {
        file: "table/mbr.rs",
        line: "if start as u64 + size as u64 > total_sectors {",
        occurrence: 1,
        count: 1,
        class: "len-bounded",
        reason: "上游 checked_* 或前置比较已证余量/上界，该运算不回绕",
    },
    Allow {
        file: "table/mbr.rs",
        line: "lba0.get(MBR_SIGNATURE_OFFSET..MBR_SIGNATURE_OFFSET + 2)",
        occurrence: 1,
        count: 1,
        class: "index-in-bounds",
        reason: "下标来自 enumerate 索引、len 上界或已校验切片范围，不越界",
    },
    Allow {
        file: "table/mbr.rs",
        line: "lba0[MBR_SIGNATURE_OFFSET..MBR_SIGNATURE_OFFSET + 2].copy_from_slice(&MBR_SIGNATURE.to_le_bytes());",
        occurrence: 1,
        count: 1,
        class: "index-in-bounds",
        reason: "下标来自 enumerate 索引、len 上界或已校验切片范围，不越界",
    },
    Allow {
        file: "table/mbr.rs",
        line: "let ae = a.start_lba as u64 + a.size_lba as u64 - 1;",
        occurrence: 1,
        count: 1,
        class: "len-bounded",
        reason: "上游 checked_* 或前置比较已证余量/上界，该运算不回绕",
    },
    Allow {
        file: "table/mbr.rs",
        line: "let be = b.start_lba as u64 + b.size_lba as u64 - 1;",
        occurrence: 1,
        count: 1,
        class: "len-bounded",
        reason: "上游 checked_* 或前置比较已证余量/上界，该运算不回绕",
    },
    Allow {
        file: "table/mbr.rs",
        line: "let cur = lba0[off + 4];",
        occurrence: 1,
        count: 1,
        class: "index-in-bounds",
        reason: "下标来自 enumerate 索引、len 上界或已校验切片范围，不越界",
    },
    Allow {
        file: "table/mbr.rs",
        line: "let e = s + rd_u32(rec, 12) as u64 - 1;",
        occurrence: 1,
        count: 1,
        class: "len-bounded",
        reason: "上游 checked_* 或前置比较已证余量/上界，该运算不回绕",
    },
    Allow {
        file: "table/mbr.rs",
        line: "let new_end = start + new_size_lba as u64 - 1;",
        occurrence: 1,
        count: 1,
        class: "len-bounded",
        reason: "上游 checked_* 或前置比较已证余量/上界，该运算不回绕",
    },
    Allow {
        file: "table/mbr.rs",
        line: "let oe = os + rd_u32(o, 12) as u64 - 1;",
        occurrence: 1,
        count: 1,
        class: "len-bounded",
        reason: "上游 checked_* 或前置比较已证余量/上界，该运算不回绕",
    },
    Allow {
        file: "table/mbr.rs",
        line: "let rec = &mut out[off..off + 16];",
        occurrence: 1,
        count: 1,
        class: "index-in-bounds",
        reason: "下标来自 enumerate 索引、len 上界或已校验切片范围，不越界",
    },
    Allow {
        file: "table/mbr.rs",
        line: "let start = u32::from_le_bytes(lba0[off + 8..off + 12].try_into().unwrap()) as u64;",
        occurrence: 1,
        count: 1,
        class: "index-in-bounds",
        reason: "下标来自 enumerate 索引、len 上界或已校验切片范围，不越界",
    },
    Allow {
        file: "table/mbr.rs",
        line: "mbr_entry_range(part as usize - 1).start",
        occurrence: 1,
        count: 1,
        class: "index-in-bounds",
        reason: "下标来自 enumerate 索引、len 上界或已校验切片范围，不越界",
    },
    Allow {
        file: "table/mbr.rs",
        line: "num: i + 1,",
        occurrence: 1,
        count: 1,
        class: "index-in-bounds",
        reason: "下标来自 enumerate 索引、len 上界或已校验切片范围，不越界",
    },
    Allow {
        file: "table/mbr.rs",
        line: "out[off + 12..off + 16].copy_from_slice(&new_size_lba.to_le_bytes());",
        occurrence: 1,
        count: 1,
        class: "index-in-bounds",
        reason: "下标来自 enumerate 索引、len 上界或已校验切片范围，不越界",
    },
    Allow {
        file: "table/mbr.rs",
        line: "out[off + 4] = next;",
        occurrence: 1,
        count: 1,
        class: "index-in-bounds",
        reason: "下标来自 enumerate 索引、len 上界或已校验切片范围，不越界",
    },
    Allow {
        file: "table/mbr.rs",
        line: "out[off + 4] = t;",
        occurrence: 1,
        count: 1,
        class: "index-in-bounds",
        reason: "下标来自 enumerate 索引、len 上界或已校验切片范围，不越界",
    },
    Allow {
        file: "table/mbr.rs",
        line: "rec[12..16].copy_from_slice(&((end - start + 1) as u32).to_le_bytes());",
        occurrence: 1,
        count: 1,
        class: "index-in-bounds",
        reason: "下标来自 enumerate 索引、len 上界或已校验切片范围，不越界",
    },
    Allow {
        file: "table/mbr.rs",
        line: "return Err(RunOutcome::refused(format!(\"range {start}..{end} outside disk (1..{})\", total - 1)));",
        occurrence: 1,
        count: 1,
        class: "display-only",
        reason: "结果仅进入输出串/日志/错误文案，不参与任何判定分支或盘面几何",
    },
    Allow {
        file: "table/mbr.rs",
        line: "start + new_size_lba as u64 - 1,",
        occurrence: 1,
        count: 1,
        class: "len-bounded",
        reason: "上游 checked_* 或前置比较已证余量/上界，该运算不回绕",
    },
    Allow {
        file: "table/mbr.rs",
        line: "i + 1",
        occurrence: 2,
        count: 2,
        class: "index-in-bounds",
        reason: "下标来自 enumerate 索引、len 上界或已校验切片范围，不越界",
    },
    Allow {
        file: "table/mbr.rs",
        line: "if rd_u32(&lba0[off..off + 16], 12) == 0 {",
        occurrence: 2,
        count: 2,
        class: "index-in-bounds",
        reason: "下标来自 enumerate 索引、len 上界或已校验切片范围，不越界",
    },
    Allow {
        file: "table/step.rs",
        line: "if at + sig_len > lba0.len() {",
        occurrence: 1,
        count: 1,
        class: "len-bounded",
        reason: "上游 checked_* 或前置比较已证余量/上界，该运算不回绕",
    },
    Allow {
        file: "table/step.rs",
        line: "if lba0[at..at + sig_len] == *GPT_SIGNATURE {",
        occurrence: 1,
        count: 1,
        class: "index-in-bounds",
        reason: "下标来自 enumerate 索引、len 上界或已校验切片范围，不越界",
    },
    Allow {
        file: "table/step.rs",
        line: "if src.size < 3 * ss {",
        occurrence: 1,
        count: 1,
        class: "len-bounded",
        reason: "上游 checked_* 或前置比较已证余量/上界，该运算不回绕",
    },
    Allow {
        file: "table/step.rs",
        line: "lba0[at..at + sig_len].fill(0);",
        occurrence: 1,
        count: 1,
        class: "index-in-bounds",
        reason: "下标来自 enumerate 索引、len 上界或已校验切片范围，不越界",
    },
];

/// 整模块豁免：宏定义模块（`$(...)*` 是重复符，不是乘法）、trait bound 求和模块、
/// 纯帮助文本排版模块。
const EXEMPT_MODULES: &[Exempt] = &[
    Exempt {
        prefix: "tagged.rs",
        class: "macro-machinery",
        reason: "整份是 tagged_enum 声明宏，`*`/`+` 均为宏重复符与变体码值，无运行期整数算术",
    },
    Exempt {
        prefix: "movepart/faults.rs",
        class: "macro-machinery",
        reason: "整份是 fault_points 声明宏，`*`/`)` 均为宏重复符；注入点存根无几何算术",
    },
    Exempt {
        prefix: "fsops/env.rs",
        class: "trait-bound-only",
        reason: "唯一 `+` 出现在闭包 trait bound（`+ Send + Sync + 'static`），不是整数求和",
    },
    Exempt {
        prefix: "table/error.rs",
        class: "trait-bound-only",
        reason: "唯一 `+` 出现在 `dyn Error + 'static` trait bound，不是整数求和",
    },
    Exempt {
        prefix: "docs.rs",
        class: "help-render",
        reason: "帮助文本排版：列宽/缩进/换行长度只进 format!/repeat! 输出串，不参与任何判定分支或盘面几何",
    },
];

/// 这些 token 之后出现的 `+`/`-`/`*` 是前缀正负号、解引用或 glob，不是二元算术。
const LEADING: &[&str] = &[
    "=", "(", "[", "{", ",", ":", ";", "?", "|", "&", "<", ">", "!", "+", "-", "*", "/", "%", "^",
    "~", "=>", "->", "&&", "||", "!=", "==", "<=", ">=", "match", "return", "if", "else",
    "while", "for", "loop", "Some", "None", "Ok", "Err", "true", "false", "as", "in", "ref",
    "mut", "use", "::",
];

/// 代码行里是否存在二元 `+`/`-`/`*`（含复合赋值）。
fn has_binary_arithmetic(code: &str) -> bool {
    let toks = tokenize(code);
    for (idx, t) in toks.iter().enumerate() {
        match t.as_str() {
            "+=" | "-=" | "*=" => return true,
            "+" | "-" | "*" => {
                let prev = idx.checked_sub(1).map(|k| toks[k].as_str());
                let leading = prev.is_none() || prev.is_some_and(|p| LEADING.contains(&p));
                if !leading {
                    return true;
                }
            }
            _ => {}
        }
    }
    false
}

fn routed_through_checked(code: &str) -> bool {
    CHECKED_PATHS.iter().any(|p| code.contains(p))
}

/// 从原始行尾注释取审计类别；不是审计注释返回 None，类名不在枚举返回 Some(Err)。
fn audit_class(raw: &str) -> Option<Result<&str, ()>> {
    let idx = raw.find("//")?;
    let rest = raw[idx + 2..].trim();
    let rest = rest.strip_prefix("audit-allow:")?;
    let class = rest.split_whitespace().next()?;
    Some(if SITE_CLASSES.contains(&class) { Ok(class) } else { Err(()) })
}

fn exempt_for(path: &str) -> Option<&'static Exempt> {
    EXEMPT_MODULES.iter().find(|e| path.starts_with(e.prefix))
}

/// 提取文件内全部算术站点；锚点内容取剥注释、去首尾空白后的整行代码。
fn arithmetic_sites(text: &str) -> Vec<Site> {
    let code = code_only(text);
    let code_lines: Vec<&str> = code.lines().collect();
    number_occurrences(
        production_lines(text)
            .iter()
            .filter_map(|(n, line)| {
                let stripped = code_lines.get(n - 1).copied().unwrap_or("").trim().to_string();
                if has_binary_arithmetic(&stripped) {
                    Some((*n, anchor_of(line)))
                } else {
                    None
                }
            }),
    )
}

struct Census {
    unregistered: Vec<Violation>,
    invalid_class: Vec<Violation>,
    dangling: Vec<Violation>,
    count_mismatch: Vec<Violation>,
    stale_exempt: Vec<Violation>,
}

fn ledger_matches(entry: &Allow, path: &str, site: &Site) -> bool {
    entry.file == path && entry.line == site.content && entry.occurrence == site.occurrence
}

fn run_census(ledger: &[Allow], sources: &[(String, String)]) -> Census {
    for r in ledger {
        assert!(
            SITE_CLASSES.contains(&r.class),
            "{}:{} 登记类别必须是五个固定枚举之一",
            r.file, r.line
        );
        assert!(!r.reason.is_empty(), "{}:{} 必须带理由", r.file, r.line);
    }
    for e in EXEMPT_MODULES {
        assert!(
            EXEMPT_CLASSES.contains(&e.class),
            "豁免 {} 的类别必须是固定豁免枚举",
            e.prefix
        );
        assert!(!e.reason.is_empty(), "豁免 {} 必须带机械理由", e.prefix);
    }

    let mut sites_by_key: BTreeMap<(String, String), Vec<Site>> = BTreeMap::new();
    let mut exempt_hit: BTreeMap<&str, usize> = BTreeMap::new();
    for (path, text) in sources {
        if is_fixture_file(path) {
            continue;
        }
        let exempt = exempt_for(path);
        for site in arithmetic_sites(text) {
            if let Some(e) = exempt {
                *exempt_hit.entry(e.prefix).or_insert(0) += 1;
                continue;
            }
            sites_by_key
                .entry((path.clone(), site.content.clone()))
                .or_default()
                .push(site);
        }
    }

    let mut unregistered: Vec<Violation> = Vec::new();
    let mut invalid_class: Vec<Violation> = Vec::new();
    let mut matched: Vec<(String, String, usize)> = Vec::new();

    for ((path, content), sites) in &sites_by_key {
        for site in sites {
            let text: &str = sources
                .iter()
                .find(|(p, _)| p == path)
                .map(|(_, t)| t.as_str())
                .unwrap_or("");
            let raw = text.lines().nth(site.line_no - 1).unwrap_or("");
            // 被其它普查按整行锚定锁定的行，标记独占其上方纯注释行；代码行锚点保持原样。
            let prev_raw = text.lines().nth(site.line_no - 2).unwrap_or("");
            let class = match audit_class(raw) {
                None if prev_raw.trim().starts_with("//") => audit_class(prev_raw),
                other => other,
            };
            let routed = {
                let code = code_only(raw);
                routed_through_checked(code.trim())
            };
            if routed {
                continue;
            }
            match class {
                Some(Err(())) => invalid_class.push(Violation {
                    file: path.clone(),
                    line_no: site.line_no,
                    category: "invalid-class",
                    content: content.clone(),
                }),
                Some(Ok(_)) | None => match ledger.iter().find(|a| ledger_matches(a, path, site)) {
                    Some(_a) => {
                        matched.push((path.clone(), content.clone(), site.occurrence));
                    }
                    None => unregistered.push(Violation {
                        file: path.clone(),
                        line_no: site.line_no,
                        category: "unregistered",
                        content: content.clone(),
                    }),
                },
            }
        }
    }

    // 反向锁定：每条登记必须仍能在现存站点里找到对应行；内容被删/被改即悬空。
    let mut dangling: Vec<Violation> = Vec::new();
    for a in ledger {
        if !matched
            .iter()
            .any(|(p, content, k)| p == a.file && content == a.line && *k == a.occurrence)
        {
            dangling.push(Violation {
                file: a.file.to_string(),
                line_no: 0,
                category: a.class,
                content: format!("{} — {}", a.line, a.reason),
            });
        }
    }

    // 总条数锁定：同一代码行在本文件算术站点里的现存条数必须与登记总数相等。
    let mut registered_total: BTreeMap<(&str, &str), usize> = BTreeMap::new();
    for a in ledger {
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

    // 豁免悬空：豁免条目覆盖不到任何现存算术站点即登记失效。
    let mut stale_exempt: Vec<Violation> = Vec::new();
    for e in EXEMPT_MODULES {
        if !exempt_hit.contains_key(e.prefix) {
            stale_exempt.push(Violation {
                file: e.prefix.to_string(),
                line_no: 0,
                category: e.class,
                content: e.reason.to_string(),
            });
        }
    }

    Census { unregistered, invalid_class, dangling, count_mismatch, stale_exempt }
}

#[test]
fn every_interval_arithmetic_is_checked_or_registered() {
    let census = run_census(LEDGER, &rust_sources());

    let mut unregistered = census.unregistered;
    unregistered.sort_by(|a, b| a.file.cmp(&b.file).then(a.line_no.cmp(&b.line_no)));
    assert!(
        unregistered.is_empty(),
        "这些生产行有裸二元算术，未走 checked 路径也未登记 audit-allow：\n{}",
        unregistered.iter().map(Violation::render).collect::<Vec<_>>().join("\n")
    );

    let mut invalid_class = census.invalid_class;
    invalid_class.sort_by(|a, b| a.file.cmp(&b.file).then(a.line_no.cmp(&b.line_no)));
    assert!(
        invalid_class.is_empty(),
        "这些 audit-allow 类名不在固定枚举内（注释不是豁免通道）：\n{}",
        invalid_class.iter().map(Violation::render).collect::<Vec<_>>().join("\n")
    );

    let mut dangling = census.dangling;
    dangling.sort_by(|a, b| a.file.cmp(&b.file).then(a.content.cmp(&b.content)));
    assert!(
        dangling.is_empty(),
        "登记条目已无对应代码行，先复核再撤登记：\n{}",
        dangling.iter().map(Violation::render).collect::<Vec<_>>().join("\n")
    );

    let mut count_mismatch = census.count_mismatch;
    count_mismatch.sort_by(|a, b| a.file.cmp(&b.file).then(a.content.cmp(&b.content)));
    assert!(
        count_mismatch.is_empty(),
        "同内容算术站点条数与登记总数不符：\n{}",
        count_mismatch.iter().map(Violation::render).collect::<Vec<_>>().join("\n")
    );

    let mut stale_exempt = census.stale_exempt;
    stale_exempt.sort_by(|a, b| a.file.cmp(&b.file).then(a.content.cmp(&b.content)));
    assert!(
        stale_exempt.is_empty(),
        "豁免条目已覆盖不到任何现存算术站点：\n{}",
        stale_exempt.iter().map(Violation::render).collect::<Vec<_>>().join("\n")
    );
}

/// 自证一：合成源码含 `let end = start + len;`，无 checked、无注释 → 判红并定位行号。
#[test]
fn a_bare_interval_addition_is_flagged() {
    let sample = "fn f(start: u64, len: u64) -> u64 {\n    let end = start + len;\n    end\n}\n";
    let census = run_census(&[], &[("sample.rs".to_string(), sample.to_string())]);
    assert_eq!(census.unregistered.len(), 1);
    let v = &census.unregistered[0];
    assert_eq!(v.file, "sample.rs");
    assert_eq!(v.line_no, 2);
    assert!(v.content.contains("start + len"));
    assert!(census.invalid_class.is_empty());
    assert!(census.dangling.is_empty());
    assert!(census.count_mismatch.is_empty());
}

/// 自证二：`checked_add` 形式绿；带合法类别注释的裸算登记后绿。
#[test]
fn checked_and_annotated_forms_are_not_flagged() {
    let checked = "fn f(start: u64, len: u64) -> Option<u64> {\n    let end = start.checked_add(len);\n    end\n}\n";
    let clean = run_census(&[], &[("sample.rs".to_string(), checked.to_string())]);
    assert!(clean.unregistered.is_empty());
    assert!(clean.invalid_class.is_empty());

    let line = "let end = start + len;";
    let annotated = format!("fn f(start: u64, len: u64) -> u64 {{\n    {line} // audit-allow: index-in-bounds\n    end\n}}\n");
    let ledger = [Allow {
        file: "sample.rs",
        line,
        occurrence: 1,
        count: 1,
        class: "index-in-bounds",
        reason: "自检样本：下标已由切片长度证明",
    }];
    let census = run_census(&ledger, &[("sample.rs".to_string(), annotated)]);
    assert!(census.unregistered.is_empty());
    assert!(census.invalid_class.is_empty());
    assert!(census.dangling.is_empty());
    assert!(census.count_mismatch.is_empty());
}

/// 自证三：自由文本类名 `// audit-allow: 我觉得没问题` → 判红（类名不在枚举）。
#[test]
fn a_free_text_allowance_is_flagged() {
    let sample = "fn f(start: u64, len: u64) -> u64 {\n    let end = start + len; // audit-allow: 我觉得没问题\n    end\n}\n";
    let census = run_census(&[], &[("sample.rs".to_string(), sample.to_string())]);
    assert_eq!(census.invalid_class.len(), 1);
    let v = &census.invalid_class[0];
    assert_eq!(v.file, "sample.rs");
    assert_eq!(v.line_no, 2);
    assert!(census.unregistered.is_empty());
}

/// 自证四：在已登记行上方插一行注释 → 仍绿（内容锚不受行号影响）。
#[test]
fn shifting_a_registered_line_does_not_break_the_census() {
    let line = "let end = start + len;";
    let before = format!("fn f(start: u64, len: u64) -> u64 {{\n    {line} // audit-allow: len-bounded\n    end\n}}\n");
    let after = format!(
        "fn f(start: u64, len: u64) -> u64 {{\n    // 上面是区间端点余量\n    // 再插一行说明\n    {line} // audit-allow: len-bounded\n    end\n}}\n"
    );
    let ledger = [Allow {
        file: "sample.rs",
        line,
        occurrence: 1,
        count: 1,
        class: "len-bounded",
        reason: "自检样本：上游 checked_add 已证余量",
    }];
    let before_sites = arithmetic_sites(&before);
    let after_sites = arithmetic_sites(&after);
    assert_eq!(before_sites.len(), 1);
    assert_eq!(after_sites.len(), 1);
    assert_ne!(before_sites[0].line_no, after_sites[0].line_no);

    for sample in [&before, &after] {
        let census = run_census(&ledger, &[("sample.rs".to_string(), sample.to_string())]);
        assert!(census.unregistered.is_empty());
        assert!(census.invalid_class.is_empty());
        assert!(census.dangling.is_empty());
        assert!(census.count_mismatch.is_empty());
    }
}
