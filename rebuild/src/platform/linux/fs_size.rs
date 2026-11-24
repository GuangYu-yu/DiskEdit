//! 文件系统尺寸探测。
//!
//! 只读：外部程序只被用来读取当前大小、分配单元与最小可收缩大小，不写目标。解析
//! 失败一律返回 `Parse`，不把读不到折成 0；不支持收缩的族 `min_bytes` 为 `None`。
//!
//! 分配单元不是装饰：`resize2fs` 按块组放置，末段装不下整组元数据时整段留空
//! （1.47.x 在 400 MiB 卷扩到 900 MiB 时止于 896 MiB）。扩容判据因此是"区间末端
//! 落在最后一个单元内"，拿字节相等去比会把成功报成失败。

use diskedit_core::fs::FsKind;

use super::{Linux, PlatformError, PlatformResult};
use crate::platform::{FsSize, Output, Platform, Program};

impl Linux {
    pub(super) fn probe_size(
        &self,
        device: &std::path::Path,
        device_bytes: u64,
        kind: FsKind,
    ) -> PlatformResult<FsSize> {
        let text = device.to_string_lossy().into_owned();
        match kind {
            FsKind::Ext => self.ext_size(&text, device_bytes),
            FsKind::Ntfs => self.ntfs_size(&text),
            FsKind::Btrfs => self.btrfs_size(&text, device_bytes),
            FsKind::F2fs | FsKind::Vfat | FsKind::Exfat | FsKind::Xfs => Ok(FsSize {
                bytes: device_bytes,
                unit: 1,
                min_bytes: None,
            }),
            FsKind::Squashfs
            | FsKind::Erofs
            | FsKind::HfsPlus
            | FsKind::Apfs
            | FsKind::Bcachefs
            | FsKind::Jfs
            | FsKind::Reiserfs
            | FsKind::Udf => Err(PlatformError::unsupported_fs(format!(
                "{kind:?} 没有尺寸探测"
            ))),
        }
    }

    fn ext_size(&self, device: &str, device_bytes: u64) -> PlatformResult<FsSize> {
        let header = self.run(Program::Dumpe2fs, &["-h", device])?;
        if !header.succeeded() {
            return Err(tool_failure("dumpe2fs", &header));
        }
        let (blocks, block_size, blocks_per_group) = parse_dumpe2fs(&header.stdout_text())
            .ok_or_else(|| PlatformError::parse("dumpe2fs 输出缺少块数或块大小"))?;
        let bytes = blocks
            .checked_mul(block_size)
            .filter(|bytes| *bytes > 0)
            .ok_or_else(|| PlatformError::parse("dumpe2fs 给出的尺寸溢出"))?;
        let bytes = if bytes > device_bytes {
            device_bytes
        } else {
            bytes
        };
        // 块组是 resize2fs 的放置单元；缺 `Blocks per group` 时退回单块粒度。
        let unit = match blocks_per_group {
            Some(per_group) => block_size.saturating_mul(per_group),
            None => block_size,
        };
        let min = match self.run(Program::Resize2fs, &["-P", device]) {
            Ok(output) if output.succeeded() => parse_resize2fs_minimum(&output.stdout_text())
                .and_then(|minimum_blocks| minimum_blocks.checked_mul(block_size)),
            _ => None,
        };
        Ok(FsSize {
            bytes,
            unit,
            min_bytes: min,
        })
    }

    fn ntfs_size(&self, device: &str) -> PlatformResult<FsSize> {
        let info = self.run(
            Program::Ntfsresize,
            &["--info", "--force", "--force", device],
        )?;
        if !info.succeeded() {
            return Err(tool_failure("ntfsresize", &info));
        }
        let (bytes, unit, minimal) = parse_ntfsresize(&info.stdout_text(), &info.stderr_text())
            .ok_or_else(|| PlatformError::parse("ntfsresize --info 输出缺少当前或最小尺寸"))?;
        Ok(FsSize {
            bytes,
            unit,
            min_bytes: Some(minimal),
        })
    }

    fn btrfs_size(&self, device: &str, device_bytes: u64) -> PlatformResult<FsSize> {
        let output = self.run(
            Program::Btrfs,
            &["inspect-internal", "min-dev-size", device],
        )?;
        if !output.succeeded() {
            return Err(tool_failure("btrfs", &output));
        }
        let min = parse_min_dev_size(&output.stdout_text())
            .ok_or_else(|| PlatformError::parse("btrfs min-dev-size 输出缺少字节数"))?;
        // btrfs 按 4 KiB 扇区放置；`min-dev-size` 不报告 sectorsize，取格式下限。
        Ok(FsSize {
            bytes: device_bytes,
            unit: 4096,
            min_bytes: Some(min),
        })
    }
}

fn tool_failure(program: &str, output: &Output) -> PlatformError {
    PlatformError::tools(program, output.stderr_text().trim().to_string())
}

/// 解析 `dumpe2fs -h` 的块数、块大小与每组块数。
///
/// `Blocks per group` 缺失时返回 `None`，调用方退回单块粒度。
pub(super) fn parse_dumpe2fs(text: &str) -> Option<(u64, u64, Option<u64>)> {
    let mut blocks = None;
    let mut block_size = None;
    let mut blocks_per_group = None;
    for line in text.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        match key.trim() {
            "Block count" => blocks = value.parse::<u64>().ok(),
            "Block size" => block_size = value.parse::<u64>().ok(),
            "Blocks per group" => blocks_per_group = value.parse::<u64>().ok(),
            _ => {}
        }
    }
    Some((blocks?, block_size?, blocks_per_group))
}

/// 解析 `resize2fs -P` 的最小块数。
pub(super) fn parse_resize2fs_minimum(text: &str) -> Option<u64> {
    text.lines()
        .find(|line| line.contains("minimum size"))
        .and_then(|line| line.rsplit_once(':'))
        .and_then(|(_, value)| value.trim().parse::<u64>().ok())
}

/// 解析 `ntfsresize --info` 的当前字节数、簇大小与最小字节数。
///
/// 输出可能落在 stdout 或 stderr；两处都读。`Cluster size` 缺失时粒度退回 1，
/// 扩容判据因此更严格，不会把未覆盖末端的卷报成已扩到位。
pub(super) fn parse_ntfsresize(stdout: &str, stderr: &str) -> Option<(u64, u64, u64)> {
    let text = if stdout.trim().is_empty() {
        stderr
    } else {
        stdout
    };
    let mut current = None;
    let mut minimal = None;
    let mut cluster = None;
    for line in text.lines() {
        let lower = line.to_ascii_lowercase();
        if lower.starts_with("current volume size:") {
            current = first_u64(line);
        } else if lower.starts_with("you might resize at") {
            minimal = first_u64(line);
        } else if lower.starts_with("cluster size") {
            cluster = first_u64(line);
        }
    }
    Some((current?, cluster.unwrap_or(1), minimal?))
}

/// 解析 `btrfs inspect-internal min-dev-size` 输出的字节数。
pub(super) fn parse_min_dev_size(text: &str) -> Option<u64> {
    first_u64(text)
}

fn first_u64(text: &str) -> Option<u64> {
    let digits: String = text
        .chars()
        .skip_while(|character| !character.is_ascii_digit())
        .take_while(|character| character.is_ascii_digit())
        .collect();
    digits.parse::<u64>().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dumpe2fs_reads_block_geometry() {
        let text = "Filesystem volume name:   <none>\nBlock count:              262144\nBlock size:               4096\nBlocks per group:         32768\n";
        assert_eq!(parse_dumpe2fs(text), Some((262_144, 4096, Some(32_768))));
        assert_eq!(
            parse_dumpe2fs("Block count: 8\nBlock size: 4096\n"),
            Some((8, 4096, None))
        );
        assert_eq!(parse_dumpe2fs("Block size: 4096\n"), None);
    }

    #[test]
    fn resize2fs_reads_minimum_blocks() {
        assert_eq!(
            parse_resize2fs_minimum("Estimated minimum size of the filesystem: 16384\n"),
            Some(16_384)
        );
        assert_eq!(parse_resize2fs_minimum("no numbers here"), None);
    }

    #[test]
    fn ntfsresize_reads_both_sizes() {
        let text = "Cluster size           : 4096 bytes\nCurrent volume size: 1073741824 bytes (1074 MB)\nYou might resize at 536870912 bytes or 512 MB\n";
        assert_eq!(
            parse_ntfsresize(text, ""),
            Some((1_073_741_824, 4096, 536_870_912))
        );
        assert_eq!(
            parse_ntfsresize("", text),
            Some((1_073_741_824, 4096, 536_870_912))
        );
        assert_eq!(
            parse_ntfsresize(
                "Current volume size: 8 bytes\nYou might resize at 4 bytes\n",
                ""
            ),
            Some((8, 1, 4))
        );
    }

    #[test]
    fn min_dev_size_reads_first_number() {
        assert_eq!(
            parse_min_dev_size("114532352 bytes (109.23 MiB)\n"),
            Some(114_532_352)
        );
        assert_eq!(parse_min_dev_size("none"), None);
    }

    #[test]
    fn exit_kind_reports_failure() {
        assert_ne!(
            crate::platform::ExitKind::Failed(1),
            crate::platform::ExitKind::Succeeded
        );
    }
}
