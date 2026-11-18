//! 同形问题的机械门禁：只读源码 + 计数断言。
//!
//! 门禁只认形状，不认措辞：计数超出基线即失败，基线里的每一项都要写明为什么它不算
//! 同类问题。这里是「同类问题不再长出」的唯一机械保证——静态审查的条目可以逐条改完，
//! 但新写一遍同样的形状只有计数能拦住。
//!
//! 六条形状与基线：
//!
//! 1. `file_type()` 的缺失折成 `unwrap_or(false)`：0 处。类型读不出不等于「不是普通
//!    文件」，这个折法把「读不出」变成「跳过」；
//! 2. `saturating_sub(1)` 表示盘末：只允许 `table/mbr.rs::container_last_lba`；
//!    另一处是搬移游标回退一格，与盘末无关，按文件登记并写明理由；
//! 3. sysfs `partition` 属性用丢失精度的 `as u32` 解析：0 处。分区号的唯一入口是
//!    `dev/target.rs::sysfs_partition_no`，溢出必须是环境故障；
//! 4. 命令层直接给出 3 / 4 的码值：0 处。成因到码的对应只写在 `outcome`；
//! 5. `ensure_idle_before_write` 的调用点：只有登记的六处，新增调用点即失败；
//! 6. 已退役的命令名以命令身份出现在源码、`docs/`、README 或 `tests/` 里：0 处。改名后
//!    按旧名字写的注释、文档与脚本指向不存在的命令。只有三种写法算「以命令身份出现」：
//!    反引号包裹、`diskedit <name>` 调用串、以及全部由命令名构成的斜杠清单；裸词不判
//!    ——`plan` 与 `undo` 是普通英文（"plan steps"、"undo" 作为动词），
//!    按词匹配只会产出噪声。`[`plan`]` 这类子模块链接与 `.abandoned` 这类落盘名不在其列。

mod guards;

use guards::{production_lines, rust_sources};

/// `saturating_sub(1)` 的基线：(文件, 该文件内的出现次数, 为什么它不是「盘末」形状)
const SATURATING_SUB_ONE: &[(&str, usize, &str)] = &[
    ("table/mbr.rs", 1, "container_last_lba：末 LBA 的唯一产生点"),
    ("movepart/plan.rs", 1, "搬移游标回退一格，接收者不是盘容量"),
];

/// `ensure_idle_before_write` 的调用点基线：(文件, 该文件内的调用次数)
const IDLE_GATE_CALLS: &[(&str, usize)] = &[
    ("cmd/fs.rs", 1),
    ("cmd/layout.rs", 2),
    ("cmd/resize/msdos.rs", 1),
    ("movepart/shift.rs", 1),
    ("online/pv.rs", 1),
];

/// 命令层允许按判词给码的文件：只读报告命令自己就是判词的产出者，码随判词走
const VERDICT_COMMANDS: &[&str] = &["cmd/inspect.rs", "cmd/records.rs"];

#[test]
fn a_failed_file_type_is_never_flattened_to_false() {
    let mut hits = Vec::new();
    for (path, text) in rust_sources() {
        let lines: Vec<&str> = production_lines(&text).into_iter().map(|(_, l)| l).collect();
        for (i, line) in lines.iter().enumerate() {
            if !line.contains("unwrap_or(false)") {
                continue;
            }
            // 折法可以跨行写成 `file_type()\n    .map(..)\n    .unwrap_or(false)`
            let window = &lines[i.saturating_sub(2)..=i];
            if window.iter().any(|l| l.contains("file_type")) {
                hits.push(format!("{path}:{}: {}", i + 1, line.trim()));
            }
        }
    }
    assert!(
        hits.is_empty(),
        "file_type() 的读不出被折成 false —— 读不出不是「不是普通文件」：\n{}",
        hits.join("\n")
    );
}

#[test]
fn saturating_sub_one_is_only_the_last_lba() {
    let mut counts: Vec<(String, usize)> = Vec::new();
    for (path, text) in rust_sources() {
        let n = production_lines(&text)
            .iter()
            .filter(|(_, line)| line.contains("saturating_sub(1)"))
            .count();
        if n > 0 {
            counts.push((path, n));
        }
    }
    counts.sort();
    let want: Vec<(String, usize)> = {
        let mut v: Vec<(String, usize)> =
            SATURATING_SUB_ONE.iter().map(|(f, n, _)| ((*f).to_string(), *n)).collect();
        v.sort();
        v
    };
    assert_eq!(
        counts, want,
        "saturating_sub(1) 的分布变了：盘末只能由 container_last_lba 给出，其他语义需要按文件重新登记理由"
    );
}

#[test]
fn the_sysfs_partition_attribute_has_one_lossless_parser() {
    let mut hits = Vec::new();
    for (path, text) in rust_sources() {
        let lines: Vec<&str> = production_lines(&text).into_iter().map(|(_, l)| l).collect();
        for (i, line) in lines.iter().enumerate() {
            if !line.contains(r#"join("partition")"#) {
                continue;
            }
            let end = (i + 5).min(lines.len());
            for follow in &lines[i + 1..end] {
                if follow.contains("as u32") {
                    hits.push(format!("{path}:{}: {}", i + 1, follow.trim()));
                }
            }
        }
    }
    assert!(
        hits.is_empty(),
        "sysfs partition 属性又出现丢失精度的解析——唯一入口是 dev/target.rs::sysfs_partition_no：\n{}",
        hits.join("\n")
    );
    let target = guards::source("dev/target.rs");
    assert_eq!(
        target.matches("fn sysfs_partition_no").count(),
        1,
        "sysfs 分区号只能有一个解析函数"
    );
}

#[test]
fn the_command_layer_never_hands_out_refusal_or_no_mutation_codes() {
    let mut hits = Vec::new();
    for (path, text) in rust_sources() {
        if !path.starts_with("cmd/") || VERDICT_COMMANDS.contains(&path.as_str()) {
            continue;
        }
        for (n, line) in production_lines(&text) {
            let trimmed = line.trim();
            let literal = ["Code(3)", "Code(4)", "exit(3)", "exit(4)"]
                .iter()
                .any(|needle| trimmed.contains(needle));
            if literal {
                hits.push(format!("{path}:{n}: {trimmed}"));
            }
        }
    }
    assert!(
        hits.is_empty(),
        "命令层直接给出 3 / 4：成因到码的对应只写在 outcome 的构造点里：\n{}",
        hits.join("\n")
    );
}

#[test]
fn the_occupancy_gate_is_called_from_exactly_the_registered_sites() {
    let mut counts: Vec<(String, usize)> = Vec::new();
    for (path, text) in rust_sources() {
        if path == "fsops/mount.rs" {
            // 定义与平台空实现所在处，不是调用点
            continue;
        }
        let n = production_lines(&text)
            .iter()
            .filter(|(_, line)| {
                let trimmed = line.trim_start();
                !trimmed.starts_with("//") && line.contains("ensure_idle_before_write(")
            })
            .count();
        if n > 0 {
            counts.push((path, n));
        }
    }
    counts.sort();
    let want: Vec<(String, usize)> =
        IDLE_GATE_CALLS.iter().map(|(f, n)| ((*f).to_string(), *n)).collect();
    assert_eq!(
        counts, want,
        "占用闸的调用点变了：每一处写路径都要在首写之前过闸，新增处先登记再放行"
    );
}

/// 退役命令名里的一个命中：文件、行号与行文
fn retired_command_hit(path: &str, n: usize, line: &str) -> Option<String> {
    let trimmed = line.trim();
    if trimmed.contains("resize-part") || trimmed.contains("plan --grow") {
        return Some(format!("{path}:{n}: {trimmed}"));
    }
    for name in RETIRED_COMMANDS {
        let quoted = format!("`{name}`");
        let mut from = 0;
        while let Some(i) = trimmed[from..].find(&quoted) {
            let at = from + i;
            // `[`plan`]` 是子模块内链，不是命令引用
            if !trimmed[..at].ends_with('[') {
                return Some(format!("{path}:{n}: {trimmed}"));
            }
            from = at + quoted.len();
        }
        if trimmed.contains(&format!("diskedit {name}")) {
            return Some(format!("{path}:{n}: {trimmed}"));
        }
    }
    if let Some(name) = slash_list_retired(trimmed) {
        return Some(format!("{path}:{n}: {trimmed}  (bare `{name}` in a command list)"));
    }
    None
}

/// 现行顶层命令名：读 `src/cli.rs` 的 `COMMANDS` 表，取每条 `name` 的首词
/// （`records list` 这类是同一顶层命令的子命令）。`help` 由解析器内建，不在表里
fn live_commands() -> &'static [String] {
    static LIVE: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
    LIVE.get_or_init(|| {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/cli.rs");
        let text = std::fs::read_to_string(&path).expect("read src/cli.rs");
        let start = text
            .find("pub(crate) const COMMANDS: &[CommandDef]")
            .expect("src/cli.rs 里没有 COMMANDS 表");
        let table = &text[start..];
        let table = &table[..table.find("\n];").map(|i| i + 1).unwrap_or(table.len())];
        let mut names: Vec<String> = Vec::new();
        for entry in table.split("CommandDef {").skip(1) {
            let Some(declared) = entry.find("name: \"").map(|i| &entry[i + "name: \"".len()..]) else {
                continue;
            };
            let Some(close) = declared.find('"') else { continue };
            let Some(word) = declared[..close].split_whitespace().next() else { continue };
            if !names.iter().any(|n| n == word) {
                names.push(word.to_string());
            }
        }
        names.push("help".to_string());
        names
    })
}

const RETIRED_COMMANDS: &[&str] = &["plan", "undo", "abandon", "resize-part"];

/// 斜杠清单的每一段都得落在现行名或退役名里，否则 `plan/apply`、`ext2/3/4`
/// 这类普通词组也会被当成命令清单
fn is_command_name(token: &str) -> bool {
    live_commands().iter().any(|n| n == token) || RETIRED_COMMANDS.contains(&token)
}

/// 斜杠清单里出现的退役名，形如 "info / plan / resize"。判据是 `/` 两侧的记号全是
/// 命令名：这是唯一一种不带反引号也明确指向命令的写法
fn slash_list_retired(line: &str) -> Option<&'static str> {
    let cleaned = line.replace('`', "");
    let tokens: Vec<&str> = cleaned
        .split_whitespace()
        .map(|t| t.trim_matches(|c: char| matches!(c, ',' | ';' | '(' | ')' | ':' | '.' | '"' | '\'')))
        .filter(|t| !t.is_empty())
        .collect();
    for (i, token) in tokens.iter().enumerate() {
        if *token != "/" || i == 0 || i + 1 >= tokens.len() {
            continue;
        }
        let (left, right) = (tokens[i - 1], tokens[i + 1]);
        if !is_command_name(left) || !is_command_name(right) {
            continue;
        }
        if let Some(name) = RETIRED_COMMANDS.iter().copied().find(|n| *n == left || *n == right) {
            return Some(name);
        }
    }
    // 不带空格的粘连写法（`info/plan`）：`/` 切开的每一段都是命令名才算清单，
    // `plan/apply`、`ext2/3/4`、`src/dev/source.rs` 因此都不进来
    for token in &tokens {
        if !token.contains('/') {
            continue;
        }
        let parts: Vec<&str> = token.split('/').filter(|p| !p.is_empty()).collect();
        if parts.len() < 2 || !parts.iter().all(|p| is_command_name(p)) {
            continue;
        }
        if let Some(name) = RETIRED_COMMANDS.iter().copied().find(|n| parts.contains(n)) {
            return Some(name);
        }
    }
    None
}

/// 读者会照着行事的其余文本面：`docs/*.md`、仓库根 README、`tests/` 下的 Rust、shell
/// 与快照。门禁文件自身除外——它的匹配表就是退役名本身
fn reader_facing_files() -> Vec<(String, String)> {
    fn walk(root: &std::path::Path, dir: &std::path::Path, out: &mut Vec<(String, String)>) {
        let mut entries: Vec<std::path::PathBuf> = std::fs::read_dir(dir)
            .unwrap_or_else(|e| panic!("cannot list {}: {e}", dir.display()))
            .map(|e| e.expect("read a directory entry").path())
            .collect();
        entries.sort();
        for path in entries {
            if path.is_dir() {
                walk(root, &path, out);
                continue;
            }
            let ext = path.extension().and_then(|e| e.to_str());
            if !matches!(ext, Some("md" | "rs" | "sh" | "txt")) {
                continue;
            }
            let rel = path
                .strip_prefix(root)
                .expect("walk started at the manifest dir")
                .to_string_lossy()
                .replace('\\', "/");
            if rel == "tests/shape_gate.rs" {
                continue;
            }
            let text = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
            out.push((rel, text));
        }
    }
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut out = Vec::new();
    walk(root, &root.join("docs"), &mut out);
    walk(root, &root.join("tests"), &mut out);
    out.push((
        "README.md".to_string(),
        std::fs::read_to_string(root.join("README.md")).expect("README.md is part of the repository"),
    ));
    out.sort();
    out
}

#[test]
fn retired_names_are_absent_from_the_live_command_surface() {
    let live = live_commands();
    assert!(!live.is_empty(), "src/cli.rs 的 COMMANDS 表没解析出命令名");
    for name in RETIRED_COMMANDS {
        assert!(
            !live.iter().any(|n| n.as_str() == *name),
            "退役的命令名 {name:?} 仍登记为现行命令"
        );
    }
}

#[test]
fn retired_command_names_stay_out_of_sources_and_docs() {
    let mut hits: Vec<String> = Vec::new();
    for (path, text) in rust_sources() {
        for (n, line) in production_lines(&text) {
            if let Some(hit) = retired_command_hit(&path, n, line) {
                hits.push(hit);
            }
        }
    }
    for (path, text) in reader_facing_files() {
        for (i, line) in text.lines().enumerate() {
            if let Some(hit) = retired_command_hit(&path, i + 1, line) {
                hits.push(hit);
            }
        }
    }
    assert!(
        hits.is_empty(),
        "退役的命令名仍以命令身份出现——改名后要回读语义，不能只换命令串：\n{}",
        hits.join("\n")
    );
}
