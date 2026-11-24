//! `records *` 的 JSON。
//!
//! 条目按目录里实际的文件投影：路径、字节数、是否活跃，以及在严格解码后得到的
//! 标识与结构判定。读不出的条目照样列出，判定写 `unreadable`，不折成空值。

use crate::app::execute::records::{RecordRow, VerifyReport};

use super::{array, number, object, optional, quote};

/// 渲染记录条目列表。
pub fn render(rows: &[RecordRow]) -> String {
    object(&[("entries", array(rows.iter().map(row).collect()))])
}

/// 渲染一次校验：命中的条目不单列，`records` 仍是整份目录视图。
pub fn render_verify(report: &VerifyReport) -> String {
    object(&[
        ("id", quote(&report.token)),
        ("verdict", quote(report.verdict.as_str())),
        ("records", array(report.rows.iter().map(row).collect())),
    ])
}

/// 渲染没有目标时的列表：状态根不建索引，只能按目标或容器枚举。
pub fn render_unavailable() -> String {
    object(&[
        ("index", quote("unavailable")),
        ("records", array(Vec::new())),
        (
            "note",
            quote("records are not indexed; pass TARGET or use `records scan`"),
        ),
    ])
}

/// 渲染一次容器扫描。
pub fn render_scan(root: &str, rows: &[RecordRow]) -> String {
    object(&[
        ("root", quote(root)),
        ("complete", "true".to_owned()),
        ("records", array(rows.iter().map(row).collect())),
    ])
}

fn row(row: &RecordRow) -> String {
    object(&[
        ("path", quote(&row.path.display().to_string())),
        ("name", quote(&row.name)),
        ("bytes", number(row.bytes)),
        ("active", row.active.to_string()),
        ("id", optional(row.id.as_deref())),
        ("state", quote(row.state.as_str())),
    ])
}
