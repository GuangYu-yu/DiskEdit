//! SIZE 请求归一：类型化的 [`SizeSpec`] →（绝对目标字节数, grow 标记）。

use crate::cli::SizeSpec;
use crate::outcome::RunOutcome;

/// SIZE 请求 →（绝对目标字节数, grow 标记），GPT/MBR resize 共用。调用点都在各自锁下
/// 用同一份请求重算——`Delta`/`Percent` 锚定"当前尺寸"、`Grow` 锚定右侧空闲，锁前算出的
/// 绝对值在窗口内可能已作废。`Absolute`/`Delta`/`Percent` 都以 `cur_bytes` 为锚
/// （调用方保证它是锁下的当前分区字节数）；`Grow` 是唯一表示"吃满右侧空闲"的写法
pub(crate) fn resolve_size_request(size: SizeSpec, cur_bytes: u64) -> Result<(Option<u64>, bool), RunOutcome> {
    match size {
        SizeSpec::Grow => Ok((None, true)),
        SizeSpec::Absolute(bytes) => Ok((Some(bytes), false)),
        SizeSpec::Delta { grow: true, bytes } => cur_bytes
            .checked_add(bytes)
            .map(|t| (Some(t), false))
            .ok_or_else(|| RunOutcome::refused("SIZE overflows the partition size")),
        SizeSpec::Delta { grow: false, bytes } => cur_bytes
            .checked_sub(bytes)
            .map(|t| (Some(t), false))
            .ok_or_else(|| RunOutcome::refused(format!("SIZE exceeds the current size {cur_bytes}"))),
        SizeSpec::Percent { grow, num, denom } => {
            // 百分比增量：锚定当前分区字节数，先乘后除（u128 防溢出）避免丢余数，
            // 再向下取整到 1MiB，保证结果落在扇区/对齐界内。分母一并进除数，
            // 小数百分比（+1.5%）不必先折成整数百分比
            let raw = (cur_bytes as u128)
                .checked_mul(num as u128)
                .and_then(|x| x.checked_div(100u128 * denom as u128))
                .filter(|x| *x <= u64::MAX as u128)
                .ok_or_else(|| RunOutcome::refused("SIZE overflows the partition size"))?
                as u64;
            let delta = raw / (1024 * 1024) * (1024 * 1024);
            let target = if grow {
                cur_bytes
                    .checked_add(delta)
                    .ok_or_else(|| RunOutcome::refused("SIZE overflows the partition size"))?
            } else {
                cur_bytes
                    .checked_sub(delta)
                    .ok_or_else(|| RunOutcome::refused(format!("SIZE exceeds the current size {cur_bytes}")))?
            };
            Ok((Some(target), false))
        }
    }
}