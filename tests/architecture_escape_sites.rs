//! 逃逸站点普查的可执行台账（`.trae/documents/逃逸站点表.md` 第 17 节）。
//!
//! 普查命令原本是十一条一次性 ripgrep。这里把其中能离散枚举的两类钉成封闭清单：
//!
//! - 折叠：`unwrap_or(false|true)` 把缺失折成布尔量；
//! - 跳过：`let Ok(..) else` 与 `if let Ok(..)` 在读失败时跳过当前判定。
//!
//! 每条按「文件 + 该行原文」登记，原文改动即失配；新增未登记站点直接失败。
//! 同类扫描无法离散枚举的形态（布尔传参、`_ =>` catch-all、文档承诺）留在文档，
//! 由第 17 节逐条给出上游保证与复现命令，不在本文件假装有判据。
//!
//! **项目约定，不属于方法论要求**：「普查必须固化成可执行清单」是本仓库对既有一次性
//! ripgrep 普查的收口方式；方法论只要求每条待复核站点给出上游保证与复现命令。
//!
//! `.trae/` 不在 crate 内，本文件不依赖台账存在。

mod guards;

use std::collections::BTreeMap;

use guards::{production_lines, rust_sources};

/// S1：缺失折成布尔量的站点
struct FoldSite {
    id: &'static str,
    file: &'static str,
    line: &'static str,
}

/// S2：读失败后跳过判定的站点
struct SkipSite {
    id: &'static str,
    file: &'static str,
    line: &'static str,
    /// 上游保证：为什么这一跳不会把缺失折成放行
    why: &'static str,
}

const FOLD_SITES: &[FoldSite] = &[
    FoldSite {
        id: "S1-6",
        file: "cmd/records.rs",
        line: "if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {",
    },
    FoldSite {
        id: "S1-7",
        file: "cmd/records.rs",
        line: "if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {",
    },
    FoldSite {
        id: "S1-10",
        file: "online/part.rs",
        line: "let partx_ok = partx.as_ref().map(|o| o.status.success()).unwrap_or(false);",
    },
];

const SKIP_SITES: &[SkipSite] = &[
    SkipSite {
        id: "S2-19",
        file: "cli.rs",
        line: "&& let Ok(byte) = u8::from_str_radix(hex, 16)",
        why: "解析过滤，失败落到 `Err` 而不是跳过校验",
    },
    SkipSite {
        id: "S2-1",
        file: "ioctl.rs",
        line: "let Ok(start) = i64::try_from(start_bytes) else {",
        why: "溢出按拒绝返回，不折成 0 或省略",
    },
    SkipSite {
        id: "S2-2",
        file: "ioctl.rs",
        line: "let Ok(length) = i64::try_from(new_len_bytes) else {",
        why: "溢出按拒绝返回，不折成 0 或省略",
    },
    SkipSite {
        id: "S2-3",
        file: "ioctl.rs",
        line: "let Ok(pno) = i32::try_from(pno) else {",
        why: "溢出按拒绝返回，不折成 0 或省略",
    },
    SkipSite {
        id: "S2-4",
        file: "dev/identity.rs",
        line: "let Ok(node) = sysfs_node(path) else { return true };",
        why: "答案是「是否整盘设备」，读不出反而按是处理，进入更严的分支",
    },
    SkipSite {
        id: "S2-20",
        file: "dev/target.rs",
        line: "&& let Ok(d) = std::fs::File::open(dir)",
        why: "目录 fsync 只影响元数据新鲜度，失败时数据写入已完成且无前置状态",
    },
    SkipSite {
        id: "S2-14",
        file: "fsops/grow.rs",
        line: "if let Ok(mut f) = std::fs::File::open(\"/dev/urandom\") {",
        why: "随机源缺失退回进程熵源，不放弃擦除",
    },
    SkipSite {
        id: "S2-21",
        file: "fsops/grow.rs",
        line: "let Ok(own) = self.dir.metadata().map(|m| {",
        why: "取不到目录项身份时放弃清理自己的记录，不签发许可",
    },
    SkipSite {
        id: "S2-12",
        file: "fsops/mount.rs",
        line: "let Ok(pid) = name.parse::<u32>() else { continue };",
        why: "只跳过不是进程号的目录名，挂载归属由后续系统表核对",
    },
    SkipSite {
        id: "S2-13",
        file: "fsops/mount.rs",
        line: "if let Ok(mut udevadm) = tool_command(\"udevadm\") {",
        why: "只决定是否等待 udev 落定，不参与任何判定",
    },
    SkipSite {
        id: "S2-8",
        file: "fsops/scope.rs",
        line: "let Ok(txt) = std::fs::read_to_string(&start_file) else { continue };",
        why: "候选读不出时跳过该候选，全部跳过则整体报错",
    },
    SkipSite {
        id: "S2-9",
        file: "fsops/scope.rs",
        line: "let Ok(start_sectors) = txt.trim().parse::<u64>() else { continue };",
        why: "候选内容不可解析时跳过该候选，全部跳过则整体报错",
    },
    SkipSite {
        id: "S2-10",
        file: "fsops/scope.rs",
        line: "let Ok(num_txt) = std::fs::read_to_string(entry.path().join(\"partition\")) else { continue };",
        why: "候选读不出时跳过该候选，全部跳过则整体报错",
    },
    SkipSite {
        id: "S2-11",
        file: "fsops/scope.rs",
        line: "let Ok(num) = num_txt.trim().parse::<u32>() else { continue };",
        why: "候选内容不可解析时跳过该候选，全部跳过则整体报错",
    },
    SkipSite {
        id: "S2-18",
        file: "movepart/faults.rs",
        line: "let Ok(v) = std::env::var(\"DISKEDIT_FAULT\") else { return };",
        why: "仅 test-faults 特性下存在；未设置注入点时不做任何事",
    },
    SkipSite {
        id: "S2-16",
        file: "platform/evidence_store.rs",
        line: "let Ok(cname) = c_name(name) else {",
        why: "名字含 NUL 时直接出错，不折成另一个记录名",
    },
    SkipSite {
        id: "S2-15",
        file: "platform/probe.rs",
        line: "if let Ok(meta) = std::fs::metadata(spec.path())",
        why: "取不到元数据时按镜像打开，随后打开失败即拒绝",
    },
    SkipSite {
        id: "S2-22",
        file: "recovery/engine.rs",
        line: "let Ok(scene) = ownership_scene(src) else {",
        why: "锁前预检读不出所有者时跳过，锁下枚举仍按记录形态判定",
    },
    SkipSite {
        id: "S2-17",
        file: "recovery/engine.rs",
        line: "if let Ok(meta) = std::fs::metadata(a.target())",
        why: "取不到元数据时按镜像打开，随后打开失败即拒绝",
    },
    SkipSite {
        id: "S2-5",
        file: "recovery/journal.rs",
        line: "let Ok((dir, name)) = companion_parts(record_path) else { return };",
        why: "只清理目录里的临时项，读不出就放弃清理，不签发许可",
    },
    SkipSite {
        id: "S2-6",
        file: "recovery/journal.rs",
        line: "let Ok(dir_c) = std::ffi::CString::new(dir.as_os_str().as_bytes()) else { return };",
        why: "只清理目录里的临时项，读不出就放弃清理，不签发许可",
    },
    SkipSite {
        id: "S2-7",
        file: "recovery/journal.rs",
        line: "let Ok(entry) = std::ffi::CString::new(file_name.as_bytes()) else { continue };",
        why: "只跳过无法命名的目录项，不影响记录状态判定",
    },
];

fn is_test_file(path: &str) -> bool {
    path.ends_with("tests.rs") || path.contains("/tests/")
}

fn scan(match_line: impl Fn(&str) -> bool) -> BTreeMap<(String, String), usize> {
    let mut found = BTreeMap::new();
    for (path, text) in rust_sources() {
        if is_test_file(&path) {
            continue;
        }
        for (_, line) in production_lines(&text) {
            let trimmed = line.trim();
            if !match_line(trimmed) {
                continue;
            }
            *found
                .entry((path.clone(), trimmed.to_string()))
                .or_insert(0) += 1;
        }
    }
    found
}

fn compare(
    label: &str,
    registered: impl Iterator<Item = (String, String, &'static str)>,
    found: BTreeMap<(String, String), usize>,
) {
    let mut want: BTreeMap<(String, String), (usize, &'static str)> = BTreeMap::new();
    for (file, line, id) in registered {
        let entry = want.entry((file, line)).or_insert((0, id));
        entry.0 += 1;
    }
    let mut problems = Vec::new();
    for (site, count) in &found {
        let expected = want.get(site).map(|w| w.0).unwrap_or(0);
        if *count > expected {
            problems.push(format!(
                "{0}:{1} 未登记站点（本轮命中 {count} 次，登记 {expected} 次）：{1}",
                site.0, site.1
            ));
        }
    }
    for (site, (count, id)) in &want {
        let actual = found.get(site).copied().unwrap_or(0);
        if *count > actual {
            problems.push(format!(
                "{0}:{1} 登记站点失配（{id}，登记 {count} 次，命中 {actual} 次）：{1}",
                site.0, site.1
            ));
        }
    }
    assert!(
        problems.is_empty(),
        "{label} 与逃逸站点台账不一致，先复核再改登记：\n{}",
        problems.join("\n")
    );
}

#[test]
fn fold_to_bool_sites_are_registered() {
    let found = scan(|l| l.contains("unwrap_or(false)") || l.contains("unwrap_or(true)"));
    compare(
        "折叠站点",
        FOLD_SITES
            .iter()
            .map(|s| (s.file.to_string(), s.line.to_string(), s.id)),
        found,
    );
}

#[test]
fn skip_on_read_failure_sites_are_registered() {
    let found = scan(|l| l.contains("let Ok(") || l.contains("if let Ok("));
    compare(
        "跳过站点",
        SKIP_SITES
            .iter()
            .map(|s| (s.file.to_string(), s.line.to_string(), s.id)),
        found,
    );
}

#[test]
fn registered_skip_sites_carry_an_upstream_guarantee() {
    for site in SKIP_SITES {
        assert!(
            site.why.len() >= 8,
            "{} 没有写上游保证；登记的理由是给人复核的，不是占位",
            site.id
        );
    }
}

#[test]
fn commit_is_never_fed_a_constant_gate() {
    let found = scan(|l| l.contains("RecoveryEngine::commit(") && l.contains("false"));
    assert!(
        found.is_empty(),
        "提交闸不得由调用方常量关死：{found:?}"
    );
}

#[test]
fn settle_companion_call_sites_are_registered() {
    let registered = [
        (
            "cmd/undo.rs",
            "let outcome = match crate::recovery::settle_companion(&path, &record, false) {",
        ),
        (
            "cmd/undo.rs",
            "if let Err(e) = crate::recovery::settle_companion(&path, &record, true) {",
        ),
        (
            "recovery/rollback.rs",
            "let outcome = match settle_companion(&path, &record, true) {",
        ),
    ]
    .map(|(file, line)| (file.to_string(), line.to_string()))
    .into_iter()
    .collect::<Vec<_>>();
    let mut want: BTreeMap<(String, String), usize> = BTreeMap::new();
    for site in registered {
        *want.entry(site).or_insert(0) += 1;
    }
    let found = scan(|l| l.contains("settle_companion(") && !l.contains("fn settle_companion"));
    assert_eq!(
        found, want,
        "settle_companion 的布尔传参是提交事实的开关；新增或改动调用点必须先复核"
    );
}
