//! 报告层：只读判定结果的唯一出口。
//!
//! 结构化结果统一为单行 JSON（`layout --json`、`records list`/`verify`）；
//! 诊断、现场与预览结论用固定文本行。退出码不在这里产生，由 [`exit`] 统一映射。

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

/// `layout` 报告的顶层字段；JSON 键与 TEXT 行名都取自这里。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LayoutField {
    Label,
    Damaged,
    SectorSize,
    SizeBytes,
    DiskGuid,
    LastPart,
    Partitions,
}

impl LayoutField {
    /// 报告包含的全部顶层字段。
    pub const ALL: [Self; 7] = [
        Self::Label,
        Self::Damaged,
        Self::SectorSize,
        Self::SizeBytes,
        Self::DiskGuid,
        Self::LastPart,
        Self::Partitions,
    ];

    /// JSON 对象里的键。
    pub const fn name(self) -> &'static str {
        match self {
            Self::Label => "label",
            Self::Damaged => "damaged",
            Self::SectorSize => "sector_size",
            Self::SizeBytes => "size_bytes",
            Self::DiskGuid => "disk_guid",
            Self::LastPart => "last_part",
            Self::Partitions => "partitions",
        }
    }

    /// TEXT 里的行前缀；分区数组以 `partition ` 起行。
    pub const fn text_prefix(self) -> &'static str {
        match self {
            Self::Label => "label:",
            Self::Damaged => "damaged:",
            Self::SectorSize => "sector_size:",
            Self::SizeBytes => "size_bytes:",
            Self::DiskGuid => "disk_guid:",
            Self::LastPart => "last_part:",
            Self::Partitions => "partition ",
        }
    }
}

/// 渲染一份只读报告；`Check` 由执行层产出文本，不经过这里。
pub fn render(report: &Report) -> Option<Rendered> {
    match report {
        Report::Layout(_) | Report::Show(_) => None,
        Report::Records(records) => Some(Rendered::Json(json::records(records))),
        Report::Resize(resize) => Some(Rendered::Text(text::resize(resize))),
        Report::Add(preview) => Some(Rendered::Text(text::add_preview(preview))),
        Report::Check(_) => None,
    }
}
