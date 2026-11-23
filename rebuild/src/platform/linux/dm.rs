//! 设备映射视图：读取 `/sys/block/dm-*/dm/name` 与映射表。
//!
//! 名称与表的解析是纯文本处理；执行 `dmsetup` 由后续执行层经平台端口发起。

use crate::platform::{PlatformError, PlatformResult};

/// 一条设备映射目标。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DmTarget {
    pub start_sector: u64,
    pub sectors: u64,
    pub kind: String,
    pub params: String,
}

/// 列出当前存在的映射名。
pub fn names() -> PlatformResult<Vec<String>> {
    let block_dir = super::sysfs_root().join("block");
    let entries = match std::fs::read_dir(&block_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(PlatformError::io(&block_dir, error)),
    };
    let mut names = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| PlatformError::io(&block_dir, error))?;
        let name = match entry.file_name().to_str() {
            Some(name) => name.to_string(),
            None => continue,
        };
        if !name.starts_with("dm-") {
            continue;
        }
        if let Some(mapped) = super::read_optional_attr(&entry.path().join("dm/name"))? {
            if !mapped.is_empty() {
                names.push(mapped);
            }
        }
    }
    names.sort();
    Ok(names)
}

/// 某个映射名是否存在。
pub fn is_present(name: &str) -> PlatformResult<bool> {
    Ok(names()?.iter().any(|candidate| candidate == name))
}

/// `dmsetup table` 的参数。
pub fn table_args(name: &str) -> Vec<String> {
    vec!["table".to_string(), name.to_string()]
}

/// 解析 `dmsetup table` 的每一行。
pub fn parse_table(text: &str) -> PlatformResult<Vec<DmTarget>> {
    let mut targets = Vec::new();
    for (index, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() < 4 {
            return Err(PlatformError::parse(format!(
                "dmsetup 第 {} 行字段不足",
                index + 1
            )));
        }
        targets.push(DmTarget {
            start_sector: parse_u64("起始扇区", fields[0])?,
            sectors: parse_u64("扇区数", fields[1])?,
            kind: fields[2].to_string(),
            params: fields[3..].join(" "),
        });
    }
    Ok(targets)
}

fn parse_u64(what: &str, text: &str) -> PlatformResult<u64> {
    text.parse::<u64>()
        .map_err(|_| PlatformError::parse(format!("{what}不是十进制整数：{text:?}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_table_rows() {
        let text = "0 2048 linear 8:1 0\n2048 4096 crypt aes-xts 16 0 8:2 0\n";
        let rows = parse_table(text).expect("可解析");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].kind, "linear");
        assert_eq!(rows[1].start_sector, 2048);
        assert_eq!(rows[1].params, "aes-xts 16 0 8:2 0");
    }

    #[test]
    fn rejects_short_rows() {
        assert!(parse_table("0 2048 linear\n").is_err());
    }
}
