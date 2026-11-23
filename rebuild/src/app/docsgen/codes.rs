//! 退出码表：从报告层的七个常量渲染。

use crate::app::report::exit;

use super::{Lang, header};

/// 渲染一份语言对应的退出码表。
pub fn render(lang: Lang) -> String {
    let mut out = String::new();
    out.push_str(&header(lang));
    out.push_str(match lang {
        Lang::En => {
            "# diskedit exit status\n\n\
             Seven values; the mapping lives in `app/report/exit.rs`.\n\n\
             | code | meaning |\n|---|---|\n"
        }
        Lang::Zh => {
            "# diskedit 退出状态\n\n\
             七个值；映射只在 `app/report/exit.rs` 一处产生。\n\n\
             | 代码 | 含义 |\n|---|---|\n"
        }
    });
    for (code, en, zh) in meanings() {
        let text = match lang {
            Lang::En => en,
            Lang::Zh => zh,
        };
        out.push_str(&format!("| {code} | {text} |\n"));
    }
    out.push('\n');
    out
}

/// 七个常量与它们的说明。常量本身由报告层提供，这里只补文字。
fn meanings() -> [(u8, &'static str, &'static str); 7] {
    [
        (
            exit::SUCCESS,
            "success: nothing written and no record created, or the record was cleaned up and the kernel view is in sync, or a full rollback completed",
            "成功：未写目标且未建记录，或记录已清理且视图已同步，或完整回滚已完成",
        ),
        (
            exit::INTERNAL,
            "internal error or a broken invariant before any target byte was written",
            "写目标字节前的不变量破坏或内部错误",
        ),
        (exit::USAGE, "usage or parse error", "用法与解析错误"),
        (
            exit::REFUSED,
            "request refused, record damaged, or evidence points at a foreign or damaged object",
            "请求被拒、记录损坏、证据指向外来或损坏",
        ),
        (
            exit::ENVIRONMENT,
            "I/O or environment failure after execution began, with no target bytes changed",
            "已进入执行、目标字节未变的 I/O 或环境故障",
        ),
        (
            exit::PARTIAL,
            "partial completion, a record awaits recovery, or persistent state is reached while the kernel view is stale",
            "部分完成、待恢复，或持久状态已达成但内核视图未同步",
        ),
        (exit::UNSUPPORTED, "capability unsupported", "能力不支持"),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_table_lists_the_seven_constants_in_order() {
        let codes: Vec<u8> = meanings().iter().map(|(code, _, _)| *code).collect();
        assert_eq!(codes, [0, 1, 2, 3, 4, 5, 6]);
    }

    #[test]
    fn every_meaning_appears_in_both_languages() {
        for lang in Lang::ALL {
            let text = render(lang);
            for (code, _, _) in meanings() {
                assert!(
                    text.contains(&format!("| {code} |")),
                    "{} 缺少退出码 {code}",
                    lang.dir()
                );
            }
        }
    }
}
