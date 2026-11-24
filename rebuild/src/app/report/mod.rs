//! 报告层：只读判定结果的唯一出口。
//!
//! 结构化结果统一为单行 JSON（`info`、`records`）；诊断与现场结论用固定文本行。
//! 退出码不在这里产生，由 [`exit`] 统一映射。

pub mod exit;
pub mod json;
pub mod text;

use super::decide::report::Report;

/// 渲染后的输出。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Rendered {
    Json(String),
    Text(String),
}

/// 渲染一份只读报告；`Check` 由执行层产出文本，不经过这里。
pub fn render(report: &Report) -> Option<Rendered> {
    match report {
        Report::Info(info) => Some(Rendered::Json(json::info(info))),
        Report::Records(records) => Some(Rendered::Json(json::records(records))),
        Report::Inspect(inspect) => Some(Rendered::Text(text::inspect(inspect))),
        Report::Check(_) => None,
    }
}
