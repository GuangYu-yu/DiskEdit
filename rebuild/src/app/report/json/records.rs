//! `records *` 的 JSON。
//!
//! 只输出记录目录中的条目：名称、字节数与是否为活跃记录。目录位置由平台端口
//! 给出，本模块不拼路径。

use crate::platform::RecordEntry;

use super::{array, number, object, quote};

/// 渲染记录条目列表。
pub fn render(entries: &[RecordEntry]) -> String {
    object(&[("entries", array(entries.iter().map(entry).collect()))])
}

fn entry(entry: &RecordEntry) -> String {
    object(&[
        ("name", quote(&entry.name)),
        ("bytes", number(entry.bytes)),
        ("active", entry.active.to_string()),
    ])
}
