//! SIZE 参数解析与 grow/shrink 请求归一。

use super::*;

/// 尺寸请求的**形状**（与盘无关）：位置参数 SIZE / `--size` 绝对值 / `--grow-to-end`。
/// 在线路径把它带进锁内、用锁下重取的当前分区尺寸重新解析——`+N`/`+N%` 锚定"当前尺寸"、
/// `grow` 锚定右侧空闲，锁前解析出的绝对值在窗口内可能已作废
#[derive(Clone, Default)]
pub(crate) struct SizeRequest {
    pub(crate) pos: Option<String>,
    pub(crate) abs: Option<u64>,
    pub(crate) grow_to_end: bool,
}

/// SIZE 请求 →（绝对目标字节数, grow 标记），GPT/MBR resize 共用。绝对值/扩/缩都锚定
/// `cur_bytes`（调用方保证它是锁下的当前分区字节数）；两者皆缺 = 未指定 SIZE
pub(crate) fn resolve_size_request(req: &SizeRequest, cur_bytes: u64) -> Result<(Option<u64>, bool), crate::outcome::Fail> {
    let mut grow_to_end = req.grow_to_end;
    let mut target: Option<u64> = req.abs;
    if let Some(s) = req.pos.as_deref() {
        if s == "grow" {
            grow_to_end = true;
        } else {
            let (v, kind, pct) = parse_size_delta(s)
                .ok_or_else(|| Fail::refused(format!("bad SIZE {s:?} (use 10G | +2G | -500M | +10% | grow; see diskedit help resize)")))?;
            if pct {
                // 百分比增量：锚定当前分区字节数，先乘后除（u128 防溢出）避免丢余数，
                // 再向下取整到 1MiB，保证结果落在扇区/对齐界内
                let raw = (cur_bytes as u128).checked_mul(v as u128)
                    .and_then(|x| x.checked_div(100))
                    .filter(|x| *x <= u64::MAX as u128)
                    .ok_or_else(|| Fail::refused(format!("{s} overflows partition size")))? as u64;
                let delta = raw / (1024 * 1024) * (1024 * 1024);
                target = Some(match kind {
                    1 => cur_bytes.checked_add(delta).ok_or_else(|| Fail::refused(format!("{s} overflows partition size")))?,
                    _ => cur_bytes.checked_sub(delta).ok_or_else(|| Fail::refused(format!("{s} exceeds current size {cur_bytes}")))?,
                });
            } else {
                target = Some(match kind {
                    0 => v,
                    1 => cur_bytes.checked_add(v).ok_or_else(|| Fail::refused(format!("{s} overflows partition size")))?,
                    _ => cur_bytes.checked_sub(v).ok_or_else(|| Fail::refused(format!("{s} exceeds current size {cur_bytes}")))?,
                });
            }
        }
    }
    if target.is_none() && !grow_to_end {
        return Err(Fail::refused("specify a SIZE (e.g. +20G, -500M, 10G, grow; see diskedit help resize)".to_string()));
    }
    Ok((target, grow_to_end))
}