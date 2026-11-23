//! 生成物与工作树的一致性。
//!
//! `docs/` 下的参考、能力矩阵与退出码表由 `app/docsgen` 渲染。设置
//! `DISKEDIT_WRITE_DOCS=1` 时改写工作树；否则逐字比对，任何手改都会失败。

use std::fs;
use std::path::PathBuf;

use diskedit::app::docsgen;

fn docs_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("docs")
}

#[test]
fn generated_docs_match_the_working_tree() {
    let root = docs_root();
    let write = std::env::var_os("DISKEDIT_WRITE_DOCS").is_some();
    let mut mismatches = Vec::new();
    for doc in docsgen::all() {
        let path = root.join(&doc.path);
        if write {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).expect("无法创建 docs 目录");
            }
            fs::write(&path, &doc.content).expect("无法写入生成物");
            continue;
        }
        match fs::read_to_string(&path) {
            Ok(actual) if actual == doc.content => {}
            Ok(_) => mismatches.push(format!("{} 与生成器输出不一致", doc.path)),
            Err(error) => mismatches.push(format!("{} 无法读取：{error}", doc.path)),
        }
    }
    assert!(
        mismatches.is_empty(),
        "生成物已过期，运行 `DISKEDIT_WRITE_DOCS=1 cargo test --test generated_docs`：\n{}",
        mismatches.join("\n")
    );
}
