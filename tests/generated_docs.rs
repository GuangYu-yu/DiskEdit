//! 生成物守卫：参考、能力矩阵、退出码与双语对照表必须与生成器逐字一致。
//!
//! 手改生成物，或改了命令声明/目录却没有重新生成，这里都会失败。

use std::path::PathBuf;

/// 生成物根目录：仓库根。
fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn read(relative: &str) -> String {
    let path = root().join(relative);
    std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("读取 {} 失败: {error}", path.display()))
}

/// 双语对照覆盖的文档：英文（生成或手写）与中文译本。
const BILINGUAL: &[(&str, &str)] = &[
    ("docs/en/reference.md", "docs/zh/reference.md"),
    (
        "docs/en/capability-matrix.md",
        "docs/zh/capability-matrix.md",
    ),
    ("docs/en/exit-codes.md", "docs/zh/exit-codes.md"),
    ("docs/en/internals.md", "docs/zh/internals.md"),
    ("docs/en/interop.md", "docs/zh/interop.md"),
];

#[test]
fn generated_docs_match_their_sources() {
    let write = std::env::var_os("DISKEDIT_WRITE_DOCS").is_some_and(|value| value == "1");
    let mut expected = diskedit::docsgen::files();

    // 先把生成物落盘，再读回文件计算双语对照表：否则对照表描述的是上一版内容，
    // 一次写模式运行后仍与新版不一致。
    if write {
        for (relative, text) in &expected {
            let path = root().join(relative);
            std::fs::write(&path, text)
                .unwrap_or_else(|error| panic!("写入 {} 失败: {error}", path.display()));
        }
    }

    // 双语逐段对照表：英文与中文一侧的任何增删段都会让这里失败。
    let texts: Vec<(String, String, String, String)> = BILINGUAL
        .iter()
        .map(|(en, zh)| (en.to_string(), read(en), zh.to_string(), read(zh)))
        .collect();
    let pairs: Vec<(&str, &str, &str, &str)> = texts
        .iter()
        .map(|(en, en_text, zh, zh_text)| {
            (en.as_str(), en_text.as_str(), zh.as_str(), zh_text.as_str())
        })
        .collect();
    expected.push((
        diskedit::docsgen::I18N_CATALOGUE,
        diskedit::docsgen::i18n_catalogue(&pairs),
    ));

    for (relative, expected) in expected {
        if write && relative == diskedit::docsgen::I18N_CATALOGUE {
            let path = root().join(relative);
            std::fs::write(&path, &expected)
                .unwrap_or_else(|error| panic!("写入 {} 失败: {error}", path.display()));
        }
        let path = root().join(relative);
        let actual = std::fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("读取 {} 失败: {error}", path.display()));
        assert_eq!(
            actual, expected,
            "{relative} 与生成器不一致；改的是声明就重新生成，改的是生成物就撤销手改"
        );
    }
}

/// 双语目录只放 Markdown：机器可读的对照表固定放 `docs/catalogue.json`。
/// 任何把 JSON/其他产物塞进 `docs/en`、`docs/zh` 的行为都会被这条挡下。
#[test]
fn bilingual_docs_hold_only_markdown() {
    for language in ["docs/en", "docs/zh"] {
        let directory = root().join(language);
        let entries = std::fs::read_dir(&directory)
            .unwrap_or_else(|error| panic!("读取 {} 失败: {error}", directory.display()));
        for entry in entries {
            let path = entry.expect("目录项可读").path();
            assert_eq!(
                path.extension().and_then(|extension| extension.to_str()),
                Some("md"),
                "{} 只允许 Markdown，发现 {}（对照表放 docs/catalogue.json）",
                language,
                path.display()
            );
        }
    }
    let catalogue = root().join(diskedit::docsgen::I18N_CATALOGUE);
    assert!(
        catalogue.is_file(),
        "双语对照表应位于 docs/ 下：{}",
        catalogue.display()
    );
}
