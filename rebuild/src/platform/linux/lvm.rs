//! LVM 物理卷视图。
//!
//! `pvs` 的报告字段与 `pvresize` 的参数在这里固定；执行经平台端口。

use std::path::Path;

use crate::platform::{PlatformError, PlatformResult};

/// 一个物理卷的关键字段。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PvRow {
    pub name: String,
    pub size_bytes: u64,
    pub pe_start_bytes: u64,
    pub vg_name: String,
}

/// `pvs` 只读查询参数。
pub fn pvs_args(path: &Path) -> Vec<String> {
    [
        "--noheadings",
        "--units",
        "b",
        "--nosuffix",
        "-o",
        "pv_name,pv_size,pe_start,vg_name",
        &path.to_string_lossy(),
    ]
    .iter()
    .map(|value| value.to_string())
    .collect()
}

/// `pvresize` 缩容参数；大小以字节计。
pub fn pvresize_args(path: &Path, size_bytes: u64) -> Vec<String> {
    vec![
        "--setphysicalvolumesize".to_string(),
        format!("{size_bytes}B"),
        path.to_string_lossy().into_owned(),
    ]
}

/// 解析 `pvs` 的单行报告。
pub fn parse_pvs(text: &str) -> PlatformResult<Vec<PvRow>> {
    let mut rows = Vec::new();
    for (index, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() != 4 {
            return Err(PlatformError::parse(format!(
                "pvs line {} should have 4 fields, got {}",
                index + 1,
                fields.len()
            )));
        }
        rows.push(PvRow {
            name: fields[0].to_string(),
            size_bytes: parse_u64("pv_size", fields[1])?,
            pe_start_bytes: parse_u64("pe_start", fields[2])?,
            vg_name: fields[3].to_string(),
        });
    }
    Ok(rows)
}

fn parse_u64(what: &str, text: &str) -> PlatformResult<u64> {
    text.parse::<u64>()
        .map_err(|_| PlatformError::parse(format!("{what} is not a decimal byte count: {text:?}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_pvs_rows() {
        let text = "  /dev/sda2 1073741824 1048576 vg0\n";
        let rows = parse_pvs(text).expect("可解析");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].size_bytes, 1_073_741_824);
        assert_eq!(rows[0].pe_start_bytes, 1_048_576);
        assert_eq!(rows[0].vg_name, "vg0");
    }

    #[test]
    fn builds_pvresize_size_argument() {
        let args = pvresize_args(Path::new("/dev/sda2"), 1_048_576);
        assert_eq!(
            args,
            vec!["--setphysicalvolumesize", "1048576B", "/dev/sda2"]
        );
    }

    #[test]
    fn rejects_wrong_field_count() {
        assert!(parse_pvs("/dev/sda2 100\n").is_err());
    }
}
