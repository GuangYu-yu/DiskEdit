//! 单一来源守卫：同一事实只允许一个产生点。
//!
//! 这里断言的是"登记处唯一"，不禁止字面量本身——测试夹具与测试预言需要独立取值，
//! 与生产侧共用常量会把断言变成同义反复。新增合法登记处时改这里的断言并写明理由。
//!
//! 生产代码的块级克隆按"入参来源、出参与错误归属完全一致"决定是否抽取
//! （测试文件之间的重合按测试夹具处理，不在此列）：
//! - 抽取：`fsops/grow.rs` 与 `fsops/shrink.rs` 的 `pending()` 只差步骤名，共用
//!   `fsops::run_pending`
//! - 抽取：`execution/fs.rs`、`execution/mkfs.rs` 与 `execution/copy.rs` 的载荷换算共用
//!   `execution::region_bytes` / `execution::region_at`；调用方各自补"目标字节数一致"、
//!   "零扇区"、盘内边界与重叠
//! - 不抽取：`cmd/resize.rs` 与 `cmd/resize/msdos.rs` 的预检块——同一逻辑在两处的
//!   入参来源不同（`src.is_block` / `is_block`），且未收尾 journal 的拒绝绑定各自流程，
//!   合成一条会把两套错误归属混在一起
//! - 不抽取：`cmd/release.rs` 与 `recovery/engine.rs` 的重合行只有闭合括号与空行，
//!   没有可提取的语句块
//! - 不抽取：`cmd/layout.rs` 与 `cmd/resize.rs` 的重合窗口是 `bail_fail`、`let Some(..)
//!   else`、`unwrap_or_else(|f| bail_fail(f))` 与 Partial 待办拼接——这些骨架决定
//!   "这条错误归谁"，抽成宏会把归责关系藏进宏体，调用点反而看不出拒绝落在哪条流程；
//!   两句 `--end` 拒绝措辞已抽到 `cmd::resize` 的
//!   `NEEDS_PARTITION` / `END_AND_GROW_TO_END_EXCLUSIVE`
//! - 不抽取：`cmd/resize/dry_run.rs` 与 `cmd/resize/msdos.rs` 的重合窗口是同一条预检的
//!   dry-run 与执行两处；两句重复文案已抽到 `cmd::resize` 的常量
//!   （`GROW_NO_FREE_SPACE_MSDOS` / `NO_FS_CANNOT_SHRINK_MSDOS`），其余为接线骨架
//! - 不抽取：`cmd/inspect.rs` 与 `cmd/records.rs` 的重合是"路径拆成目录句柄与锚定
//!   相对名"的几行，错误归属不同（命令失败 vs 单条记录继续扫描），抽取要再造一个
//!   错误枚举才能保留两条路径的措辞
//! - 不抽取：`cmd/layout.rs` 与 `cmd/resize/dry_run.rs` 的重合是几何解析的三步链
//!   （`require_gpt_geometry` → `resolve_part_in` → `live_entry`）与 `bail_fail` 归责，
//!   抽成函数会把"拒绝落在哪条流程"藏起来
//! - 不抽取：`online/pv.rs` 与 `online/target.rs` 的重合是 sysfs 遍历的前置过滤
//!   （`p.is_dir()` 排除属性文件）；两处的循环体与读失败归属不同
//!
//! 记录段的单字节标签：
//! - 无载荷枚举由 `src/tagged.rs` 的 `tagged_enum!` 从一张码值表生成枚举与双向转换，
//!   手写 `from_tag` 只允许出现在宏定义里（见下方守卫）
//! - 恢复动作（码值 + u64 载荷）与目标身份材料（两类后跟各自字段）带载荷，用
//!   `recovery/record.rs` 内的具名 `const`；宏只覆盖无载荷枚举
//! - 在飞位、修复段与围栏的 0/1 是"段存在与否"，第三态 `Undecidable` 不写盘：
//!   写侧直接拒绝，读侧逐段解析
//! - `bool` 字段（`is_swap`、`restore_blocked`）写 `u8::from`，读侧只接受 0/1
//! - 可选段统一走 `recovery/record.rs` 的 `write_opt` / `read_opt`

use std::path::{Path, PathBuf};

fn manifest() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

fn files_under(dir: &Path, ext: &str, out: &mut Vec<PathBuf>) {
    let entries =
        std::fs::read_dir(dir).unwrap_or_else(|e| panic!("read {}: {e}", dir.display()));
    for entry in entries {
        let entry =
            entry.unwrap_or_else(|e| panic!("read an entry of {}: {e}", dir.display()));
        let path = entry.path();
        if path.is_dir() {
            files_under(&path, ext, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some(ext) {
            out.push(path);
        }
    }
}

fn relative(path: &Path) -> String {
    path.strip_prefix(manifest())
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

/// 文档路径只登记在 `src/docs.rs` 的 `DOCS` 表里：生成器写盘、选项表与工具表对账、
/// 示例闸都从那张表取。第二个源文件出现同一路径，就是第二份事实
#[test]
fn a_document_path_is_registered_in_one_place() {
    let mut sources = Vec::new();
    files_under(&manifest().join("src"), "rs", &mut sources);
    let sources: Vec<(String, String)> = sources
        .iter()
        .map(|path| (relative(path), std::fs::read_to_string(path).expect("read a source file")))
        .collect();

    let mut documents = Vec::new();
    files_under(&manifest().join("docs"), "md", &mut documents);
    documents.push(manifest().join("README.md"));
    documents.push(manifest().join("README.zh-CN.md"));

    for doc in &documents {
        let rel = relative(doc);
        let owners: Vec<&str> = sources
            .iter()
            .filter(|(_, text)| text.contains(&rel))
            .map(|(path, _)| path.as_str())
            .collect();
        assert_eq!(
            owners,
            ["src/docs.rs"],
            "{rel} is named in {owners:?}; a document path belongs to the DOCS table in src/docs.rs"
        );
    }
}

/// 用例集合由目录派生：`run_all.sh` 与具体脚本没有名字上的耦合，脚本的差异
/// （超时、是否过滤输出）由脚本头部的变量自述。少一个名字就少一处漏登记
#[test]
fn the_wsl_runner_learns_per_script_facts_from_the_scripts() {
    let runner = std::fs::read_to_string(manifest().join("tests/wsl/run_all.sh"))
        .expect("read the runner");
    let code = shell_code(&runner);
    for declaration in ["TIMEOUT", "RUNNER_FULL_OUTPUT"] {
        assert!(
            code.contains(declaration),
            "run_all.sh no longer reads the {declaration} declaration"
        );
    }

    let mut scripts = Vec::new();
    files_under(&manifest().join("tests/wsl"), "sh", &mut scripts);
    for script in &scripts {
        let name = script.file_name().unwrap().to_string_lossy();
        // 这两个文件不是用例，只在 SKIP_FILES 里出现
        if matches!(name.as_ref(), "lib.sh" | "run_all.sh") {
            continue;
        }
        assert!(
            !code.contains(name.as_ref()),
            "run_all.sh names {name}; it is already discovered from the directory — \
             declare TIMEOUT / RUNNER_FULL_OUTPUT in the script instead"
        );
    }
}

/// 去掉整行注释后的 shell 代码：注释里举例脚本名或写出变量名，都不构成执行与读取
fn shell_code(text: &str) -> String {
    text.lines()
        .filter(|line| !line.trim_start().starts_with('#'))
        .collect::<Vec<_>>()
        .join("\n")
}

/// 错误措辞由 `support` 的单点构造器产生：调用点再写一遍同一句 format，就是第二份事实。
/// 复扫：
/// `rg -n 'format!\("(cannot (read|stat) \{\}|identify failed|parse failed)' src --glob '!src/support.rs'`
#[test]
fn error_wording_is_built_in_one_place() {
    let mut sources = Vec::new();
    files_under(&manifest().join("src"), "rs", &mut sources);
    let mut offenders = Vec::new();
    for path in &sources {
        let rel = relative(path);
        if rel == "src/support.rs" {
            continue;
        }
        let text = std::fs::read_to_string(path).expect("read a source file");
        offenders.extend(wording_offenders(&rel, &text));
    }
    assert!(
        offenders.is_empty(),
        "error wording is duplicated at {offenders:?}; call the support::* constructor instead \
         (rescan: rg -n 'format!(\"(cannot (read|stat)|identify failed|parse failed)' src)"
    );
}

/// 无载荷枚举的码值双向映射由 `tagged_enum!` 生成：`from_tag` 的手写实现只允许出现在
/// 宏定义里。再写一张反向表，就是两个可能各自漂移的事实
#[test]
fn enum_code_mapping_is_generated_from_one_table() {
    let mut sources = Vec::new();
    files_under(&manifest().join("src"), "rs", &mut sources);
    let mut offenders = Vec::new();
    for path in &sources {
        let rel = relative(path);
        let text = std::fs::read_to_string(path).expect("read a source file");
        offenders.extend(handwritten_from_tag(&rel, &text));
    }
    assert!(
        offenders.is_empty(),
        "`from_tag` is written by hand at {offenders:?}; declare the enum with tagged_enum! instead"
    );
}

/// 一段源码里手写的 `fn from_tag`（注释与测试条目不算）；宏定义所在文件是唯一例外
fn handwritten_from_tag(rel: &str, text: &str) -> Vec<String> {
    if rel == "src/tagged.rs" {
        return Vec::new();
    }
    code_only(&production_text(text))
        .lines()
        .enumerate()
        .filter(|(_, line)| line.contains("fn from_tag"))
        .map(|(no, _)| format!("{rel}:{}", no + 1))
        .collect()
}

/// 拒绝侧：手写反向表必须被这条守卫命中
#[test]
fn the_tag_mapping_guard_catches_a_handwritten_reverse_table() {
    let dummy = "impl OpKind {\n    fn from_tag(tag: u8) -> Option<Self> { None }\n}\n";
    assert_eq!(handwritten_from_tag("src/other.rs", dummy).len(), 1);
    assert!(handwritten_from_tag("src/tagged.rs", dummy).is_empty());
}

/// 一段源码里重复写出的错误措辞；`#[cfg(test)]` 条目里的字面量是测试预言，不在此列。
/// 比较的是整条字符串字面量：插值占位符按 `{}` 归一，跨行字符串接回成一行
fn wording_offenders(rel: &str, text: &str) -> Vec<String> {
    const FINGERPRINTS: [&str; 5] = [
        "cannot read {}: {e}",
        "cannot stat {}: {e}",
        "identify failed: {e}",
        "parse failed: {e}",
        "parse failed ({kind}): {e}",
    ];
    let mut offenders = Vec::new();
    for (no, line) in logical_lines(&production_text(text)) {
        let code = line.split("//").next().unwrap_or("");
        for literal in string_literals(code) {
            let literal = normalize_placeholders(&literal);
            for needle in FINGERPRINTS {
                if literal == normalize_placeholders(needle) {
                    offenders.push(format!("{rel}:{no}: {needle}"));
                }
            }
        }
    }
    offenders
}

/// 一行代码里的字符串字面量内容；`\"` 与 `\\` 按转义跳过
fn string_literals(code: &str) -> Vec<String> {
    let b = code.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] != b'"' {
            i += 1;
            continue;
        }
        i += 1;
        let start = i;
        let mut end = i;
        while i < b.len() && b[i] != b'"' {
            if b[i] == b'\\' {
                i += 1;
            }
            i += 1;
            end = i.min(b.len());
        }
        out.push(code[start..end.min(code.len())].to_string());
        i += 1;
    }
    out
}

/// 源码的逻辑行与起始行号（1 起）：行尾 `\` 让字符串续到下一行，下一行的行首空白
/// 不进入字面量。注释行不参与拼接
fn logical_lines(text: &str) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    let mut buf = String::new();
    let mut start = 1;
    let mut continuing = false;
    for (no, raw) in text.lines().enumerate() {
        let line = if continuing { raw.trim_start() } else { raw };
        if !continuing {
            start = no + 1;
        }
        if let Some(head) = line.trim_end().strip_suffix('\\').filter(|_| is_string_continuation(line)) {
            buf.push_str(head);
            continuing = true;
        } else {
            buf.push_str(line);
            out.push((start, std::mem::take(&mut buf)));
            continuing = false;
        }
    }
    if continuing || !buf.is_empty() {
        out.push((start, buf));
    }
    out
}

/// 行尾 `\` 是不是字符串内的续行：该行在此之前有一个未闭合的字符串字面量。
/// 注释行的行尾 `\` 与字符/转义无关
fn is_string_continuation(line: &str) -> bool {
    let body = match line.trim_end().strip_suffix('\\') {
        Some(body) => body,
        None => return false,
    };
    if body.trim_start().starts_with("//") {
        return false;
    }
    let mut quotes = 0usize;
    let mut escaped = false;
    for c in body.chars() {
        if escaped {
            escaped = false;
            continue;
        }
        match c {
            '\\' => escaped = true,
            '"' => quotes += 1,
            _ => {}
        }
    }
    quotes % 2 == 1
}

/// 字符串插值占位符归一成 `{}`：`{part}`、`{e:?}` 与位置参数 `{}` 是同一处
fn normalize_placeholders(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '{' {
            out.push(c);
            continue;
        }
        // `{{` 是转义的花括号，不构成占位符
        if chars.peek() == Some(&'{') {
            out.push('{');
            out.push(chars.next().unwrap());
            continue;
        }
        for inner in chars.by_ref() {
            if inner == '}' {
                break;
            }
        }
        out.push_str("{}");
    }
    out
}

/// 把字符串、字符字面量（含 raw string）、注释替换成空格（换行保留）：结构判定与
/// 数值扫描只看代码
fn code_only(text: &str) -> String {
    let b = text.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'r' | b'b' => {
                let Some((quote, hashes)) = raw_string_open(b, i) else {
                    out.push(b[i]);
                    i += 1;
                    continue;
                };
                let mut j = quote + 1;
                while j < b.len() {
                    if b[j] == b'"' {
                        let close = j + 1 + hashes;
                        if close <= b.len()
                            && b[j + 1..close].iter().all(|&c| c == b'#')
                        {
                            j = close;
                            break;
                        }
                    }
                    j += 1;
                }
                for &byte in &b[i..j] {
                    out.push(if byte == b'\n' { b'\n' } else { b' ' });
                }
                i = j;
            }
            b'/' if b.get(i + 1) == Some(&b'/') => {
                while i < b.len() && b[i] != b'\n' {
                    out.push(b' ');
                    i += 1;
                }
            }
            b'/' if b.get(i + 1) == Some(&b'*') => {
                while i < b.len() {
                    if b[i] == b'*' && b.get(i + 1) == Some(&b'/') {
                        out.extend_from_slice(b"  ");
                        i += 2;
                        break;
                    }
                    out.push(if b[i] == b'\n' { b'\n' } else { b' ' });
                    i += 1;
                }
            }
            b'"' => {
                out.push(b' ');
                i += 1;
                while i < b.len() {
                    if b[i] == b'\\' {
                        out.push(b' ');
                        i += 1;
                        if i < b.len() {
                            out.push(if b[i] == b'\n' { b'\n' } else { b' ' });
                            i += 1;
                        }
                    } else if b[i] == b'"' {
                        out.push(b' ');
                        i += 1;
                        break;
                    } else {
                        out.push(if b[i] == b'\n' { b'\n' } else { b' ' });
                        i += 1;
                    }
                }
            }
            b'\'' => {
                // 字符字面量整段抹掉；生命周期标记 `'a` 原样保留
                let after_quote = b.get(i + 1).copied();
                let width = match (after_quote, b.get(i + 2), b.get(i + 3)) {
                    (Some(c), Some(&b'\''), _) if c != b'\\' => Some(3),
                    (Some(b'\\'), _, Some(&b'\'')) => Some(4),
                    _ => None,
                };
                match width {
                    Some(n) => {
                        out.resize(out.len() + n, b' ');
                        i += n;
                    }
                    None if after_quote == Some(b'\\') => {
                        // '\u{...}' 等变长转义：扫到收盘单引号
                        let mut j = i + 1;
                        while j < b.len() && b[j] != b'\'' {
                            j += 1;
                        }
                        let end = (j + 1).min(b.len());
                        out.resize(out.len() + (end - i), b' ');
                        i = end;
                    }
                    None => {
                        out.push(b[i]);
                        i += 1;
                    }
                }
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8(out).expect("blanking keeps UTF-8 byte boundaries")
}

/// raw string 的起始位置：`r"…"`、`r#"…"#`、`br#"…"#`。返回开引号下标与井号个数
fn raw_string_open(b: &[u8], i: usize) -> Option<(usize, usize)> {
    let mut j = i;
    if b[j] == b'b' {
        j += 1;
    }
    if b.get(j) != Some(&b'r') {
        return None;
    }
    j += 1;
    let hashes = b[j..].iter().take_while(|&&c| c == b'#').count();
    (b.get(j + hashes) == Some(&b'"')).then_some((j + hashes, hashes))
}

/// raw string 里的花括号与引号不参与结构判定
#[test]
fn code_only_blanks_raw_strings() {
    let code = "let a = r#\"x { y \" z\"#;\nlet b = br#\"{ }\"#;\nlet c = r\"{\";\n";
    let blanked = code_only(code);
    assert!(!blanked.contains('{'), "{blanked:?}");
    assert!(!blanked.contains('}'), "{blanked:?}");
}

/// `#[cfg(... test ...)]` 修饰的整项占用的行（属性行到配对右花括号）；其余为 false。
/// 结构在一份抹掉字符串与注释的副本上判定，字符串里的花括号不参与计数
fn cfg_test_lines(code: &str) -> Vec<bool> {
    let lines: Vec<&str> = code.lines().collect();
    let mut drop = vec![false; lines.len()];
    let mut i = 0;
    while i < lines.len() {
        let compact: String = lines[i].chars().filter(|c| !c.is_whitespace()).collect();
        let is_item = compact.starts_with("#[cfg(")
            && (compact.contains("(test)") || compact.contains("(test,") || compact.contains(",test)"));
        if !is_item {
            i += 1;
            continue;
        }
        let start = i + 1;
        drop[i] = true;
        i += 1;
        let mut depth = 0i32;
        let mut seen_brace = false;
        let mut closed = false;
        while i < lines.len() {
            drop[i] = true;
            let opens = lines[i].matches('{').count() as i32;
            let closes = lines[i].matches('}').count() as i32;
            seen_brace |= opens > 0;
            depth += opens - closes;
            let ends = if seen_brace {
                depth <= 0
            } else {
                lines[i].trim_end().ends_with(';')
            };
            i += 1;
            if ends {
                closed = true;
                break;
            }
        }
        assert!(
            closed,
            "a `#[cfg(test)]` item starting at line {start} never closes; the scan cannot tell \
             which later lines are production code"
        );
    }
    drop
}

/// 去掉 `#[cfg(test)]` 修饰的整个条目（内容原样保留，仅这些行留空占位）
fn production_text(text: &str) -> String {
    let drop = cfg_test_lines(&code_only(text));
    let mut out = String::with_capacity(text.len());
    for (i, line) in text.lines().enumerate() {
        if !drop[i] {
            out.push_str(line);
        }
        out.push('\n');
    }
    out
}

/// 同 [`production_text`]，但字符串与注释已替换为空格，供数值扫描使用
fn production_code(text: &str) -> String {
    let code = code_only(text);
    let drop = cfg_test_lines(&code);
    let mut out = String::with_capacity(code.len());
    for (i, line) in code.lines().enumerate() {
        if !drop[i] {
            out.push_str(line);
        }
        out.push('\n');
    }
    out
}

fn skip_ws_left(b: &[u8], mut i: usize) -> Option<u8> {
    while i > 0 {
        i -= 1;
        if !b[i].is_ascii_whitespace() {
            return Some(b[i]);
        }
    }
    None
}

fn skip_ws_right(b: &[u8], mut i: usize) -> Option<u8> {
    while i < b.len() {
        if !b[i].is_ascii_whitespace() {
            return Some(b[i]);
        }
        i += 1;
    }
    None
}

/// 数值后面的类型后缀（`512u64` 之类）不算相邻标识符
fn numeric_suffix_ok(rest: &str) -> bool {
    for suffix in ["usize", "u64", "u32", "u16", "u8", "i64", "i32", "i16", "i8"] {
        if let Some(tail) = rest.strip_prefix(suffix) {
            return tail
                .as_bytes()
                .first()
                .is_none_or(|c| !(c.is_ascii_alphanumeric() || *c == b'_'));
        }
    }
    false
}

fn touches_word(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_' || c == b'.'
}

/// 独立的 `<needle>` 数值：参与乘法的（`3087 * 512`）不算裸写，按乘积口径归入 MiB 检查
fn count_standalone_number(code: &str, needle: &str) -> usize {
    let b = code.as_bytes();
    let mut found = 0;
    for (idx, _) in code.match_indices(needle) {
        let prev = skip_ws_left(b, idx);
        let next = skip_ws_right(b, idx + needle.len());
        if prev.is_some_and(|c| touches_word(c) || c == b'*') {
            continue;
        }
        if let Some(c) = next {
            if c == b'*' {
                continue;
            }
            if touches_word(c) && !numeric_suffix_ok(&code[idx + needle.len()..]) {
                continue;
            }
        }
        found += 1;
    }
    found
}

/// 写成 1 MiB 的各种形态；乘积形式按"整个表达式的值"识别，16 MiB 不算
fn count_one_mib(code: &str) -> usize {
    const FORMS: [&str; 8] = [
        "1<<20",
        "1024*1024",
        "1048576",
        "1_048_576",
        "2048*512",
        "256*4096",
        "0x100000",
        "0x10_0000",
    ];
    let compact: String = code.chars().filter(|c| !c.is_whitespace()).collect();
    let b = compact.as_bytes();
    let mut found = 0;
    for form in FORMS {
        for (idx, _) in compact.match_indices(form) {
            let prev = if idx == 0 { None } else { Some(b[idx - 1]) };
            let next = skip_ws_right(b, idx + form.len());
            if prev.is_some_and(|c| touches_word(c) || c == b'*') {
                continue;
            }
            if let Some(c) = next
                && (c == b'*' || (touches_word(c) && !numeric_suffix_ok(&compact[idx + form.len()..])))
            {
                continue;
            }
            found += 1;
        }
    }
    found
}

/// MBR 分区表区的偏移只在 `src/table/layout.rs` 声明，槽位区间由
/// `mbr_entry_range` 派生。复扫：
/// `rg -n '\b446\b' src --glob '!src/table/layout.rs' --glob '!src/**/tests/**' --glob '!src/**/tests.rs'`
#[test]
fn mbr_entry_offsets_come_from_the_layout() {
    let mut sources = Vec::new();
    files_under(&manifest().join("src"), "rs", &mut sources);
    let mut offenders = Vec::new();
    for path in &sources {
        let rel = relative(path);
        if rel == "src/table/layout.rs" || rel.contains("/tests/") || rel.ends_with("tests.rs") {
            continue;
        }
        let text = std::fs::read_to_string(path).expect("read a source file");
        for no in handwritten_446(&text) {
            offenders.push(format!("{rel}:{no}"));
        }
    }
    assert!(
        offenders.is_empty(),
        "MBR entry offsets are written by hand at {offenders:?}; use mbr_entry_range \
         (rescan: rg -n '\\b446\\b' src --glob '!src/table/layout.rs')"
    );
}

/// 手写 446 的行号（1 起）；`#[cfg(test)]` 条目里的夹具不计
fn handwritten_446(text: &str) -> Vec<usize> {
    let production = production_text(text);
    production
        .lines()
        .enumerate()
        .filter(|(_, line)| {
            let code = line.split("//").next().unwrap_or("");
            code.split(|c: char| !c.is_ascii_alphanumeric()).any(|w| w == "446")
        })
        .map(|(no, _)| no + 1)
        .collect()
}

/// 裸写 512 / 1 MiB 的登记表：每一处是不同协议字段（MBR 的 512 字节区、记录头长度、
/// PV 默认 PE 等），各自独立。新的取值点走 `SectorSize` / `MIB_BYTES` 或具体语义的
/// 具名常量，本表不扩充
const BARE_512_BASELINE: &[(&str, usize)] = &[
    ("src/cmd/info.rs", 4),
    ("src/cmd/records.rs", 1),
    ("src/dev/source.rs", 2),
    ("src/dev/topology.rs", 2),
    ("src/domain.rs", 1),
    ("src/fsid.rs", 1),
    ("src/fsops/ext.rs", 2),
    ("src/fsops/scope.rs", 1),
    ("src/fsops/shrink.rs", 2),
    ("src/fsops/swap.rs", 3),
    ("src/online/part.rs", 1),
    ("src/online/pv.rs", 1),
    ("src/online/target.rs", 5),
    ("src/table/gpt_parse.rs", 1),
    ("src/table/layout.rs", 1),
];

/// 1 MiB 的裸写只剩 `MIB_BYTES` 自己的定义
const BARE_MIB_BASELINE: &[(&str, usize)] = &[("src/support.rs", 1)];

/// 扇区字节数与 1 MiB 的裸写只允许登记在册的残留。复扫：
/// `rg -n '\b512\b|1\s*<<\s*20|1024\s*\*\s*1024|1048576' src --glob '!src/**/tests/**' --glob '!src/**/tests.rs'`
#[test]
fn sector_and_mib_literals_stay_inside_the_recorded_baseline() {
    let mut sources = Vec::new();
    files_under(&manifest().join("src"), "rs", &mut sources);
    let mut offenders = Vec::new();
    for path in &sources {
        let rel = relative(path);
        if rel.contains("/tests/") || rel.ends_with("tests.rs") {
            continue;
        }
        let text = std::fs::read_to_string(path).expect("read a source file");
        let code = production_code(&text);
        let bare_512 = count_standalone_number(&code, "512");
        let bare_mib = count_one_mib(&code);
        for (value, actual, baseline) in [
            (512, bare_512, BARE_512_BASELINE),
            (1048576, bare_mib, BARE_MIB_BASELINE),
        ] {
            let allowed = baseline
                .iter()
                .find(|(file, _)| *file == rel.as_str())
                .map_or(0, |(_, n)| *n);
            if actual > allowed {
                offenders.push(format!(
                    "{rel}: {actual} bare {value} literal(s), {allowed} recorded"
                ));
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "new bare sector-size / MiB literals at {offenders:?}; use SectorSize or MIB_BYTES, \
         or record the new independent fact in BARE_512_BASELINE / BARE_MIB_BASELINE \
         (rescan: rg -n '\\b512\\b|1\\s*<<\\s*20|1024\\s*\\*\\s*1024' src)"
    );
}

/// 数值扫描只认代码里的独立字面量：字符串、注释、乘积与更大乘积里的因子都不算
#[test]
fn numeric_baseline_scanner_ignores_projections_and_products() {
    let code = |src: &str| code_only(src);
    assert_eq!(count_standalone_number(&code("let n = 512;"), "512"), 1);
    assert_eq!(count_standalone_number(&code("let n = 512u64;"), "512"), 1);
    assert_eq!(count_standalone_number(&code("let n = 3087 * 512;"), "512"), 0);
    assert_eq!(count_standalone_number(&code(r#"let s = "512";"#), "512"), 0);
    assert_eq!(count_standalone_number(&code("// 512\n"), "512"), 0);
    assert_eq!(count_standalone_number(&code("const N: u64 = 1_512;"), "512"), 0);
    assert_eq!(count_one_mib(&code("let n = 1 << 20;")), 1);
    assert_eq!(count_one_mib(&code("let n = 1024 * 1024;")), 1);
    assert_eq!(count_one_mib(&code("let n = 16 * 1024 * 1024;")), 0);
    assert_eq!(count_one_mib(&code("let n = 2048 * 512;")), 1);
}

/// 守卫的拒侧：生产位置的第二份措辞会被报出，测试条目里的同字面量不会被误报
#[test]
fn wording_guard_catches_a_second_copy() {
    let duplicated = "fn f(e: Error) -> String {\n    format!(\"parse failed: {e}\")\n}\n";
    assert_eq!(wording_offenders("src/other.rs", duplicated).len(), 1);
    // 同一句换个排版（format! 跨行）仍是第二份措辞
    let wrapped = "fn f(e: Error) -> String {\n    format!(\n        \"parse failed: {e}\"\n    )\n}\n";
    assert_eq!(wording_offenders("src/other.rs", wrapped).len(), 1);
    // 位置参数与带名占位符是同一句
    let positional = "fn f(e: Error) -> String {\n    \
                      format!(\"cannot read {}: {}\", path, e)\n}\n";
    assert_eq!(wording_offenders("src/other.rs", positional).len(), 1);
    // 行尾续行把一句拆到两行，接回去仍是同一句
    let continued = "fn f(e: Error) -> String {\n    format!(\"parse failed: \\\n         {e}\")\n}\n";
    assert_eq!(wording_offenders("src/other.rs", continued).len(), 1);
    let prediction = "#[cfg(test)]\nmod tests {\n    fn f(e: Error) -> String {\n        \
                      format!(\"parse failed: {e}\")\n    }\n}\n";
    assert!(wording_offenders("src/other.rs", prediction).is_empty());
}

/// 守卫的拒侧：生产位置的裸 446 会被报出，测试条目里的偏移夹具不会被误报
#[test]
fn mbr_offset_guard_catches_a_second_copy() {
    assert_eq!(handwritten_446("let off = 446 + slot * 16;\n"), vec![1]);
    assert!(handwritten_446("#[cfg(test)]\nfn fixture() -> usize {\n    446\n}\n").is_empty());
}

/// 报告收尾与 `resize` 拒绝的句子各只有一个出处：调用点复述同一句，事实就有了第二种说法
const OWNED_SENTENCES: [(&str, &str); 6] = [
    ("this run changed no persistent state", "src/outcome.rs"),
    ("the target may already have been changed", "src/outcome.rs"),
    (
        "partition {part}: {sectors} sectors of {sector_size} bytes overflow the address space",
        "src/execution/mod.rs",
    ),
    ("`resize` needs :N (see diskedit help resize)", "src/cmd/resize.rs"),
    ("`resize` needs :N (the partition to resize)", "src/cmd/resize.rs"),
    ("--end and --grow-to-end are mutually exclusive", "src/cmd/resize.rs"),
];

/// 「未改变任何持久状态」由 `NOTHING_WRITTEN` 统一追加，「目标可能已被改变」只在
/// Partial 指引里组装；载荷溢出与 `--end` 两条拒绝各由一个函数或常量产生
#[test]
fn report_and_refusal_sentences_have_one_owner() {
    let mut sources = Vec::new();
    files_under(&manifest().join("src"), "rs", &mut sources);
    let mut offenders = Vec::new();
    for path in &sources {
        let rel = relative(path);
        let text = std::fs::read_to_string(path).expect("read a source file");
        for (sentence, owner) in OWNED_SENTENCES {
            if rel != owner && single_copy_violation(&text, sentence) > 0 {
                offenders.push(format!("{rel}: {sentence}"));
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "a sentence is restated at {offenders:?}; reference its single owner instead"
    );
}

/// 守卫的拒侧：代码位置复述收尾句会被报出，注释里提到同一句不算
#[test]
fn diagnostic_sentence_guard_catches_a_second_copy() {
    let sentence = "this run changed no persistent state";
    let code = format!("fn f() -> String {{\n    \"{sentence}\".to_string()\n}}\n");
    assert_eq!(single_copy_violation(&code, sentence), 1);
    let comment = format!("// {sentence}\nfn f() {{}}\n");
    assert_eq!(single_copy_violation(&comment, sentence), 0);
    // 位置参数与带名占位符是同一句
    let positional = "fn f() -> String {\n    \
                      format!(\"partition {}: {} sectors of {} bytes overflow the address space\", a, b, c)\n}\n";
    assert_eq!(
        single_copy_violation(positional, OWNED_SENTENCES[2].0),
        1,
        "a positional restatement must be caught"
    );
}

/// 一段源码在代码位置包含 `sentence` 的次数（注释与测试条目不算）。插值占位符按
/// `{}` 归一，跨行字符串接回成一行后比较
fn single_copy_violation(text: &str, sentence: &str) -> usize {
    let needle = normalize_placeholders(sentence);
    logical_lines(&production_text(text))
        .iter()
        .filter(|(_, line)| {
            let code = line.split("//").next().unwrap_or("");
            normalize_placeholders(code).contains(&needle)
        })
        .count()
}
