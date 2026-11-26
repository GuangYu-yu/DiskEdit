//! 生成物守卫：参考、能力矩阵与退出码必须与生成器逐字一致。
//!
//! 手改生成物，或改了命令声明/目录却没有重新生成，这里都会失败。

use std::path::PathBuf;

/// 生成物根目录：`rebuild/`。
fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

// covers: T16
#[test]
fn generated_docs_match_their_sources() {
    let write = std::env::var_os("DISKEDIT_WRITE_DOCS").is_some_and(|value| value == "1");
    for (relative, expected) in diskedit::docsgen::files() {
        let path = root().join(relative);
        if write {
            std::fs::write(&path, &expected)
                .unwrap_or_else(|error| panic!("写入 {} 失败: {error}", path.display()));
        }
        let actual = std::fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("读取 {} 失败: {error}", path.display()));
        assert_eq!(
            actual, expected,
            "{relative} 与生成器不一致；改的是声明就重新生成，改的是生成物就撤销手改"
        );
    }
}

// covers: T16
#[test]
fn chinese_reference_covers_every_command() {
    let path = root().join("docs/zh/reference.md");
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("读取 {} 失败: {error}", path.display()));
    for command in diskedit::app::cli::declare::COMMANDS {
        let heading = format!("### `{}`", command.name);
        assert!(
            text.contains(&heading),
            "docs/zh/reference.md 缺少命令 {} 的章节",
            command.name
        );
    }
}
