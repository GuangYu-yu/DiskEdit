//! 守卫 target 共用的只读工具：源码读取、标记顺序、进程级命令入口。
//!
//! 这些 target 把「计划步骤只有一个执行器」「码值只有一份映射」「记录自证且无版本号」
//! 三条结构约束固化成对源码的可机械复核断言；其中 `cmd_info`、`cmd_plan` 另以进程级
//! 只读命令核对输出字段与退出码。目标字节的零写检查由既有 `no_write_probe`（Linux，
//! 递归 sha256 快照）与 `cli/platform_gate`（非 Linux）承担。结构与行为两类断言不互相替代。
//!
//! 每个 integration test 是独立 crate，各自只用到本模块的一部分函数；未使用的 `pub`
//! 项在该 crate 内必然触发 `dead_code`，故模块级放行（全仓唯一的模块级 allow）。

#![allow(dead_code)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

pub fn src_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src")
}

/// 读一份源码；缺文件直接失败，不静默跳过（否则守卫会随文件改名失效）
pub fn source(rel: &str) -> String {
    let path = src_root().join(rel);
    std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()))
}

/// `src/` 下全部 `.rs` 的 (相对路径, 内容)。跳过 `tests/` 子目录——测试夹具里的直写
/// 不属于生产写点，不该被「不得新增写路径」的断言误伤
pub fn rust_sources() -> Vec<(String, String)> {
    fn walk(dir: &Path, out: &mut Vec<(String, String)>) {
        let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)
            .unwrap_or_else(|e| panic!("cannot list {}: {e}", dir.display()))
            .map(|e| e.expect("read a directory entry").path())
            .collect();
        entries.sort();
        for path in entries {
            if path.is_dir() {
                if path.file_name().is_some_and(|n| n == "tests") {
                    continue;
                }
                walk(&path, out);
            } else if path.extension().is_some_and(|e| e == "rs") {
                let rel = path
                    .strip_prefix(src_root())
                    .expect("walk started at src/")
                    .to_string_lossy()
                    .replace('\\', "/");
                let text = std::fs::read_to_string(&path)
                    .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
                out.push((rel, text));
            }
        }
    }
    let mut out = Vec::new();
    walk(&src_root(), &mut out);
    out
}

/// 去掉尾部 `#[cfg(test)] mod ...` 块之后的行。`#[cfg(test)]` 访问器不算测试模块，
/// 仍留在生产行里；返回 (1-based 行号, 行文本)
pub fn production_lines(text: &str) -> Vec<(usize, &str)> {
    let lines: Vec<&str> = text.lines().collect();
    let mut out = Vec::new();
    let mut skipping: Option<i32> = None;
    for (i, line) in lines.iter().enumerate() {
        if let Some(depth) = skipping.as_mut() {
            *depth += line.matches('{').count() as i32 - line.matches('}').count() as i32;
            if *depth <= 0 {
                skipping = None;
            }
            continue;
        }
        if line.trim() == "#[cfg(test)]" {
            let opens_tests = lines[i + 1..]
                .iter()
                .take(2)
                .any(|l| l.trim_start().starts_with("mod "));
            if opens_tests {
                skipping = Some(0);
                continue;
            }
        }
        out.push((i + 1, *line));
    }
    out
}

/// 豁免标记的行首。标记写在被豁免语句的上一行，形如 `// audit-allow: W-2 S1-6`：
/// 一个标记可列多个 id，各守卫只取自己命名空间下的那些
pub const MARKER: &str = "// audit-allow:";

/// 一条标记行里的 id 列表；不是标记行时为空
pub fn marker_ids(line: &str) -> Vec<&str> {
    line.trim()
        .strip_prefix(MARKER)
        .map(|rest| rest.split_whitespace().collect())
        .unwrap_or_default()
}

/// `prod[i]` 所述语句的标记里属于 `prefix` 命名空间的 id。标记写在语句上一行，只对它
/// 紧邻的下一行生效——中间隔了空行或其他注释就不再算数。属性行（`#[...]`）属于它下面
/// 那条语句，因此标记写在属性之上时仍算作对整条语句的标注
pub fn marked_ids<'a>(prod: &'a [(usize, &'a str)], i: usize, prefix: &str) -> Vec<&'a str> {
    let mut j = i;
    while j > 0 {
        j -= 1;
        if prod[j].1.trim_start().starts_with("#[") {
            continue;
        }
        return marker_ids(prod[j].1)
            .into_iter()
            .filter(|id| id.starts_with(prefix))
            .collect();
    }
    Vec::new()
}

/// 全部行，含 `#[cfg(test)] mod` 块；返回 (1-based 行号, 行文本)
pub fn all_lines(text: &str) -> Vec<(usize, &str)> {
    text.lines().enumerate().map(|(i, line)| (i + 1, line)).collect()
}

/// 按标记普查一类站点：返回已标记站点的 (文件, id) 与没有标记的命中行。
/// `lines_of` 决定扫哪个行集（生产行或全部行），`match_line` 收 (文件相对路径, trimmed 行)
pub fn census(
    sources: &[(String, String)],
    prefix: &str,
    lines_of: for<'a> fn(&'a str) -> Vec<(usize, &'a str)>,
    match_line: impl Fn(&str, &str) -> bool,
) -> (Vec<(String, String)>, Vec<String>) {
    let mut found = Vec::new();
    let mut unmarked = Vec::new();
    for (path, text) in sources {
        let lines = lines_of(text);
        for (i, (n, line)) in lines.iter().enumerate() {
            let trimmed = line.trim();
            if !match_line(path, trimmed) {
                continue;
            }
            let ids = marked_ids(&lines, i, prefix);
            if ids.is_empty() {
                unmarked.push(format!("{path}:{n}: {trimmed}"));
                continue;
            }
            for id in ids {
                found.push((path.clone(), id.to_string()));
            }
        }
    }
    (found, unmarked)
}

/// 普查并与登记表比对：每个标记 id 恰好一条登记，每条登记恰好一个标记。
/// 计数按 (文件, id) 累加，同一个 id 标在两处同样失败
pub fn compare_sites(
    label: &str,
    sources: &[(String, String)],
    prefix: &str,
    lines_of: for<'a> fn(&'a str) -> Vec<(usize, &'a str)>,
    match_line: impl Fn(&str, &str) -> bool,
    registered: impl Iterator<Item = (String, &'static str)>,
) {
    let (found, mut unmarked) = census(sources, prefix, lines_of, match_line);
    unmarked.sort();
    assert!(
        unmarked.is_empty(),
        "{label}：这些行没有 `{MARKER} <{prefix}id>` 标记（写在紧邻的上一行）——先登记再标记：\n{}",
        unmarked.join("\n")
    );
    let mut actual: BTreeMap<(String, String), usize> = BTreeMap::new();
    for site in found {
        *actual.entry(site).or_insert(0) += 1;
    }
    let mut want: BTreeMap<(String, String), usize> = BTreeMap::new();
    for (file, id) in registered {
        *want.entry((file, id.to_string())).or_insert(0) += 1;
    }
    assert_eq!(
        actual, want,
        "{label} 与登记表不一致，先复核再改登记：每个标记 id 需要恰好一条登记，每条登记需要恰好一个标记"
    );
}

pub fn pos(text: &str, needle: &str, what: &str) -> usize {
    text.find(needle)
        .unwrap_or_else(|| panic!("{what}: expected marker {needle:?} is gone"))
}

pub fn assert_order(text: &str, needles: &[&str], what: &str) {
    let mut at = 0usize;
    for needle in needles {
        let p = text[at..]
            .find(needle)
            .unwrap_or_else(|| panic!("{what}: {needle:?} is missing after byte {at}"));
        at += p + needle.len();
    }
}

pub fn run(args: &[&str]) -> (i32, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_DiskEdit"))
        .args(args)
        .output()
        .expect("spawn DiskEdit");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}
