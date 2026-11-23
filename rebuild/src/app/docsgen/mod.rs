//! 文档生成器：命令参考、能力矩阵与退出码表都从声明与档案表渲染。
//!
//! 生成物与源码同源，`tests/generated_docs.rs` 比对工作树；手改生成物会让比对失败。
//! 散文留在产品文档里，这里只渲染能从代码读出的事实。

pub mod codes;
pub mod matrix;
pub mod reference;

/// 生成语言。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lang {
    En,
    Zh,
}

impl Lang {
    /// 两种语言，按目录顺序。
    pub const ALL: [Lang; 2] = [Lang::En, Lang::Zh];

    /// `docs/` 下的语言目录名。
    pub const fn dir(self) -> &'static str {
        match self {
            Self::En => "en",
            Self::Zh => "zh",
        }
    }
}

/// 一份生成物：相对 `docs/` 的路径与完整内容。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GeneratedDoc {
    pub path: String,
    pub content: String,
}

/// 全部生成物，按语言的目录顺序排列。
pub fn all() -> Vec<GeneratedDoc> {
    let mut docs = Vec::new();
    for lang in Lang::ALL {
        docs.push(GeneratedDoc {
            path: format!("{}/reference.md", lang.dir()),
            content: reference::render(lang),
        });
        docs.push(GeneratedDoc {
            path: format!("{}/capability-matrix.md", lang.dir()),
            content: matrix::render(lang),
        });
        docs.push(GeneratedDoc {
            path: format!("{}/exit-codes.md", lang.dir()),
            content: codes::render(lang),
        });
    }
    docs
}

/// 把文件头写明生成来源的说明行按语言拼出来。
fn header(lang: Lang) -> String {
    match lang {
        Lang::En => "Generated from `app/cli/spec.rs` and `diskedit_core::fs::catalog` by `app/docsgen`.\n\
                     Regenerate with `DISKEDIT_WRITE_DOCS=1 cargo test --test generated_docs`.\n\
                     CI compares the working tree with the generator output. Do not edit by hand.\n\n"
            .to_owned(),
        Lang::Zh => "由 `app/docsgen` 从 `app/cli/spec.rs` 与 `diskedit_core::fs::catalog` 生成。\n\
                     用 `DISKEDIT_WRITE_DOCS=1 cargo test --test generated_docs` 重新生成。\n\
                     CI 比对工作树与生成物；不要手工编辑。\n\n"
            .to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_language_renders_three_documents() {
        let docs = all();
        assert_eq!(docs.len(), Lang::ALL.len() * 3);
        for lang in Lang::ALL {
            for name in ["reference.md", "capability-matrix.md", "exit-codes.md"] {
                let path = format!("{}/{name}", lang.dir());
                assert!(docs.iter().any(|doc| doc.path == path), "缺少生成物 {path}");
            }
        }
    }

    #[test]
    fn generated_documents_are_non_empty() {
        for doc in all() {
            assert!(!doc.content.trim().is_empty(), "{} 为空", doc.path);
        }
    }
}
