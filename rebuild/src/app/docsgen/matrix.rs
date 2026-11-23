//! 能力矩阵：从核心档案表逐格渲染内容类别与五种操作的支持情况。

use diskedit_core::fs::catalog::{Profile, Support, Verb};
use diskedit_core::fs::profile::ContentKind;

use super::{Lang, header};
use crate::app::cli::spec::SPECS;

const VERBS: [Verb; 5] = [
    Verb::Grow,
    Verb::Shrink,
    Verb::Attrs,
    Verb::Check,
    Verb::Mkfs,
];

/// 渲染一份语言对应的能力矩阵。
pub fn render(lang: Lang) -> String {
    let mut out = String::new();
    out.push_str(&header(lang));
    out.push_str(match lang {
        Lang::En => {
            "# diskedit capability matrix\n\n\
             One row per content kind the tool identifies. A cell is the support class\n\
             reported by the filesystem catalogue; the steps each class runs are declared\n\
             there as well.\n\n\
             | content | grow | shrink | attributes | check | mkfs |\n\
             |---|---|---|---|---|---|\n"
        }
        Lang::Zh => {
            "# diskedit 能力矩阵\n\n\
             本工具可识别的每一种内容各占一行。单元格给出文件系统档案表报告的支持类别；\n\
             每类能力执行的步骤序列同样由该表声明。\n\n\
             | 内容 | 扩容 | 收缩 | 属性 | 检查 | 建卷 |\n\
             |---|---|---|---|---|---|\n"
        }
    });
    for kind in ContentKind::ALL {
        let profile = diskedit_core::fs::catalog::profile(kind);
        out.push_str(&format!("| `{}` |", kind.as_str()));
        for verb in VERBS {
            out.push_str(&format!(" {} |", cell(&profile, verb, lang)));
        }
        out.push('\n');
    }
    out.push('\n');
    out.push_str(&format!("## {}\n\n", tool_heading(lang)));
    out.push_str(match lang {
        Lang::En => "External programs the catalogue refers to, by identity:\n\n",
        Lang::Zh => "档案表引用到的外部程序（按身份）：\n\n",
    });
    let names: Vec<String> = diskedit_core::fs::Program::ALL
        .iter()
        .map(|program| format!("`{program:?}`"))
        .collect();
    out.push_str(&names.join(", "));
    out.push_str("\n\n");
    render_gate(&mut out, lang);
    out
}

/// 渲染运行闸口：入口 × 目标形态 × 平台。
fn render_gate(out: &mut String, lang: Lang) {
    out.push_str("## Runtime gate\n\n");
    out.push_str(match lang {
        Lang::En => {
            "Writing is supported on Linux only. Elsewhere the dispatch gate refuses any\n\
             command that would write; read-only entries still run. Image files never need\n\
             root for table writes; block devices do.\n\n\
             | command | image file | block device | other platforms |\n\
             |---|---|---|---|\n"
        }
        Lang::Zh => {
            "写入只在 Linux 上支持；其他平台拒绝会写入的命令，只读入口仍可运行。镜像的\n\
             表区写入从不需要 root，块设备需要。\n\n\
             | 命令 | 镜像文件 | 块设备 | 其他平台 |\n\
             |---|---|---|---|\n"
        }
    });
    for spec in SPECS {
        let writes = spec.confirmation;
        let (image, block, other) = match (writes, lang) {
            (true, Lang::En) => ("yes", "yes (root)", "refused (6)"),
            (true, Lang::Zh) => ("可以", "可以（需 root）", "拒绝（6）"),
            (false, Lang::En) => ("yes", "yes", "yes"),
            (false, Lang::Zh) => ("可以", "可以", "可以"),
        };
        out.push_str(&format!(
            "| `{}` | {image} | {block} | {other} |\n",
            spec.name
        ));
    }
    out.push('\n');
}

fn cell(profile: &Profile, verb: Verb, lang: Lang) -> &'static str {
    match (profile.support(verb), lang) {
        (Support::Tools(_), Lang::En) => "supported",
        (Support::Tools(_), Lang::Zh) => "支持",
        (Support::NotApplicable, Lang::En) => "not applicable",
        (Support::NotApplicable, Lang::Zh) => "不适用",
        (Support::Unsupported(_), Lang::En) => "unsupported",
        (Support::Unsupported(_), Lang::Zh) => "不支持",
    }
}

fn tool_heading(lang: Lang) -> &'static str {
    match lang {
        Lang::En => "Programs",
        Lang::Zh => "程序",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_content_kind_appears_in_both_languages() {
        for lang in Lang::ALL {
            let text = render(lang);
            for kind in ContentKind::ALL {
                assert!(
                    text.contains(&format!("| `{}` |", kind.as_str())),
                    "{} 缺少 {}",
                    lang.dir(),
                    kind.as_str()
                );
            }
        }
    }

    #[test]
    fn every_entry_appears_in_the_runtime_gate() {
        for lang in Lang::ALL {
            let text = render(lang);
            for spec in SPECS {
                assert!(
                    text.contains(&format!("| `{}` |", spec.name)),
                    "{} 的运行闸口缺少 {}",
                    lang.dir(),
                    spec.name
                );
            }
        }
    }

    #[test]
    fn every_program_identity_is_listed() {
        for lang in Lang::ALL {
            let text = render(lang);
            for program in diskedit_core::fs::Program::ALL {
                assert!(
                    text.contains(&format!("`{program:?}`")),
                    "{} 缺少程序 {:?}",
                    lang.dir(),
                    program
                );
            }
        }
    }
}
