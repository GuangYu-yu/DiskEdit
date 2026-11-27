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
// 旧版 `docs/zh/catalogue.json` 用逐段映射硬约束双语文档；该文件在 T16 重构中移除。
// 这里恢复同等强度的机器对账：中文生成物必须逐段镜像英文生成物的结构——
// 命令骨架同序、顶层节齐全、每个命令的段块数一致、表格行数一致。
fn sections(text: &str) -> Vec<(String, Vec<String>)> {
    let mut out: Vec<(String, Vec<String>)> = Vec::new();
    for line in text.lines() {
        if line.starts_with('#') {
            out.push((line.to_string(), Vec::new()));
        } else if let Some((_, blocks)) = out.last_mut() {
            if !line.trim().is_empty() {
                blocks.push(line.to_string());
            }
        }
    }
    out
}

fn command_headings(sections: &[(String, Vec<String>)]) -> Vec<String> {
    sections
        .iter()
        .filter_map(|(h, _)| {
            h.trim_start_matches('#')
                .trim()
                .strip_prefix('`')
                .and_then(|rest| rest.strip_suffix('`'))
                .map(str::to_string)
        })
        .collect()
}

fn table_rows(text: &str) -> usize {
    text.lines()
        .filter(|l| l.trim_start().starts_with('|'))
        .count()
}

#[test]
fn chinese_reference_mirrors_the_generated_structure() {
    let en = std::fs::read_to_string(root().join("docs/en/reference.md")).unwrap();
    let zh = std::fs::read_to_string(root().join("docs/zh/reference.md")).unwrap();
    let en_secs = sections(&en);
    let zh_secs = sections(&zh);
    // 命令骨架：同名、同序、一个不缺
    let en_cmds = command_headings(&en_secs);
    let zh_cmds = command_headings(&zh_secs);
    assert_eq!(
        en_cmds, zh_cmds,
        "docs/zh/reference.md 的命令骨架与英文生成物不同——双语必须逐段对齐"
    );
    // 顶层节：英文四个节，中文都要有对应物（标题翻译，但数量与次序一致）
    let en_top: Vec<&(String, Vec<String>)> = en_secs
        .iter()
        .filter(|(h, _)| h.starts_with("## "))
        .collect();
    let zh_top: Vec<&(String, Vec<String>)> = zh_secs
        .iter()
        .filter(|(h, _)| h.starts_with("## "))
        .collect();
    assert_eq!(
        en_top.len(),
        zh_top.len(),
        "docs/zh/reference.md 顶层节数量与英文不一致"
    );
    // 每个命令节：段块数一致（旧 catalogue.json 的逐段硬约束的等价物）
    for (heading, en_blocks) in &en_secs {
        if !heading.starts_with("### ") {
            continue;
        }
        let zh_blocks = &zh_secs
            .iter()
            .find(|(h, _)| h == heading)
            .expect("命令节在中文侧缺失")
            .1;
        assert_eq!(
            en_blocks.len(),
            zh_blocks.len(),
            "docs/zh/reference.md 的 {heading} 段块数与英文不一致——翻译漏段或英文新增未译"
        );
    }
}

#[test]
fn chinese_capability_and_exit_docs_cover_the_generated_tables() {
    for en_rel in ["docs/en/capability-matrix.md", "docs/en/exit-codes.md"] {
        let zh_rel = en_rel.replace("docs/en/", "docs/zh/");
        let en = std::fs::read_to_string(root().join(en_rel))
            .unwrap_or_else(|error| panic!("读取 {en_rel} 失败: {error}"));
        let zh = std::fs::read_to_string(root().join(&zh_rel))
            .unwrap_or_else(|error| panic!("读取 {zh_rel} 失败: {error}"));
        assert_eq!(
            table_rows(&en),
            table_rows(&zh),
            "{zh_rel} 表格行数与 {en_rel} 不一致——双语表格必须逐行对齐"
        );
    }
}
