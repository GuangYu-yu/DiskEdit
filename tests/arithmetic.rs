//! 决策层算术守卫：`src/app/decide` 只允许显式检查过的算术。
//!
//! `saturating_*` 会把溢出折成一个"看起来合法"的数继续走决策，`unwrap_or_default`
//! 会把缺失折成零值；两者在判定层都会把错误变成静默的默认值。逐点替换防不住下一次
//! 新增，所以这里把规则做成扫描：判定层再出现同类写法即红。观察层只做只读展示，
//! 饱和截断在那里是"窗口截到可用范围"的明确语义，不在此守卫内。

use std::path::{Path, PathBuf};

/// 受守卫的判定层：判定与它直接消费的 LVM 决策代码。
const LAYERS: [&str; 2] = ["src/app/decide", "src/app/lvm.rs"];

/// 判定层禁止的写法。
const BANNED: [&str; 6] = [
    "saturating_add(",
    "saturating_sub(",
    "saturating_mul(",
    "saturating_div(",
    "saturating_pow(",
    "unwrap_or_default()",
];

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn rust_files(directory: &Path, out: &mut Vec<PathBuf>) {
    if directory.is_file() {
        out.push(directory.to_path_buf());
        return;
    }
    if !directory.is_dir() {
        return;
    }
    for entry in std::fs::read_dir(directory).expect("判定层目录可读") {
        let path = entry.expect("目录项可读").path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().and_then(|extension| extension.to_str()) == Some("rs") {
            out.push(path);
        }
    }
}

/// 去掉行注释后再匹配，避免注释里的示例把守卫绊红。
fn code_only(text: &str) -> String {
    text.lines()
        .map(|line| line.split("//").next().unwrap_or(""))
        .collect::<Vec<_>>()
        .join("\n")
}

fn offences(root: &Path) -> Vec<String> {
    let mut found = Vec::new();
    for layer in LAYERS {
        let mut files = Vec::new();
        rust_files(&root.join(layer), &mut files);
        for file in files {
            let text = std::fs::read_to_string(&file).expect("逻辑层源文件可读");
            let code = code_only(&text);
            for needle in BANNED {
                if code.contains(needle) {
                    found.push(format!("{}: {needle}", file.display()));
                }
            }
        }
    }
    found
}

#[test]
fn the_decision_layer_uses_explicit_checked_arithmetic() {
    let found = offences(&repo_root());
    assert!(
        found.is_empty(),
        "判定层出现了静默截断算术，改为显式检查并给出拒绝：{found:#?}"
    );
}

#[test]
fn the_guard_flags_a_synthetic_saturating_expression() {
    let root = PathBuf::from("target/fixtures/arithmetic-guard");
    drop(std::fs::remove_dir_all(&root));
    let file = root.join("src/app/decide/bypass.rs");
    std::fs::create_dir_all(file.parent().expect("有父目录")).expect("夹具目录可建");
    std::fs::write(
        &file,
        "fn f(total: u64) -> u64 { total.saturating_sub(1) }\nfn g() -> Vec<u8> { Vec::new() }\n",
    )
    .expect("夹具可写");
    let found = offences(&root);
    assert_eq!(found.len(), 1, "没有折默认值的写法不应被报红：{found:#?}");
    assert!(
        found[0].ends_with("bypass.rs: saturating_sub("),
        "{found:#?}"
    );
}
