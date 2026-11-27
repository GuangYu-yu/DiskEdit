//! LVM 物理卷链：只读查询、目标 LV 选择与 PV resize 与 LV extend 执行。
//!
//! 本模块只经 [`Platform`] 的端口运行外部程序，不出现可执行名或系统路径。报表解析
//! 与执行分离：解析是纯函数（可单测），执行只组装程序身份与参数。设备身份按
//! [`DeviceMatch`] 三态判定——"无法证明相同"不等于"不同"，按拒绝处理。

use std::path::{Path, PathBuf};

use diskedit_core::fs::Program;

use crate::platform::{DeviceMatch, Platform, PlatformError};

/// LVM 链失败的四态。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LvmError {
    /// 工具未安装；装包后重试有意义。
    ToolMissing(String),
    /// 请求与卷现状对不上（`--lv` 写错、PV 上没有 LV、LV 不唯一）。
    Refused(String),
    /// 目标存在但当前不可用（LV 非活动）。
    Inactive(String),
    /// 工具已运行但失败，或报表不可解析。
    Failed(String),
}

impl LvmError {
    /// 补救提示。
    pub fn hint(&self) -> String {
        match self {
            Self::ToolMissing(_) => format!("install the missing tool and re-run: {self}"),
            Self::Refused(_) => {
                format!("{self} — pick the target LV explicitly (--lv NAME) and re-run")
            }
            Self::Inactive(_) => {
                format!("{self} — activate it (lvchange -ay <lv path>) and re-run")
            }
            Self::Failed(_) => {
                "resize the PV, then extend the LV and its filesystem explicitly".to_string()
            }
        }
    }
}

impl std::fmt::Display for LvmError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ToolMissing(message)
            | Self::Refused(message)
            | Self::Inactive(message)
            | Self::Failed(message) => f.write_str(message),
        }
    }
}

/// 选定的目标 LV。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LvSelection {
    pub path: String,
    pub extent_bytes: u64,
    pub size: u64,
}

/// `pvs` 的尺寸字段。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PvSizeReport {
    pub dev_size: u64,
    pub pv_size: u64,
    pub pe_start: u64,
    pub pv_pe_count: u64,
    pub pv_pe_alloc_count: u64,
}

/// 执行一次外部程序并取 stdout；区分"工具缺失"与"命令失败"。
fn tool_stdout(
    platform: &dyn Platform,
    program: Program,
    args: &[String],
) -> Result<String, LvmError> {
    let name = platform.tool_name(program);
    if !platform.tool_available(program) {
        return Err(LvmError::ToolMissing(format!(
            "LVM tooling missing: {name} not found in PATH (install lvm2)"
        )));
    }
    let run = platform
        .run(program, args)
        .map_err(|error| LvmError::Failed(format!("{name} failed: {}", describe(&error))))?;
    match run.code {
        Some(0) => Ok(run.stdout),
        _ => Err(LvmError::Failed(format!(
            "{name} failed: {}",
            run.stderr.trim()
        ))),
    }
}

fn describe(error: &PlatformError) -> String {
    error.to_string()
}

/// 报表里"是否为同一设备"的判据。
fn matches(platform: &dyn Platform, target: &str, reported: &str) -> DeviceMatch {
    platform.compare_devices(Path::new(target), Path::new(reported))
}

/// 取 report[].<key>[] 数组，展平多个 report 元素。
fn report_rows(json: &str, key: &str) -> Result<Vec<serde_json::Value>, LvmError> {
    let value: serde_json::Value = serde_json::from_str(json)
        .map_err(|error| LvmError::Failed(format!("LVM report is not valid JSON: {error}")))?;
    let Some(reports) = value.get("report").and_then(|report| report.as_array()) else {
        return Err(LvmError::Failed(
            "LVM report has no `report` array".to_string(),
        ));
    };
    Ok(reports
        .iter()
        .filter_map(|report| report.get(key).and_then(|value| value.as_array()))
        .flatten()
        .cloned()
        .collect())
}

fn report_parsable(json: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(json)
        .ok()
        .and_then(|value| value.get("report").and_then(|r| r.as_array()).map(|_| ()))
        .is_some()
}

/// `lvmreport` 的 devices 条目形如 `dev/sda3(0)`：去掉 PE 偏移。
fn strip_pe_offset(entry: &str) -> &str {
    let text = entry.trim();
    if let Some(at) = text.rfind('(')
        && text.ends_with(')')
        && text[at + 1..text.len() - 1]
            .bytes()
            .all(|byte| byte.is_ascii_digit())
    {
        return &text[..at];
    }
    text
}

/// 从 `pvs` JSON 解析 `part_dev` 所属 VG；`Ok(None)` 表示 PV 不属于任何 VG。
fn parse_pv_vg(
    json: &str,
    part_dev: &str,
    device_matches: &dyn Fn(&str) -> DeviceMatch,
) -> Result<Option<String>, LvmError> {
    let mut saw_target = false;
    for row in report_rows(json, "pv")? {
        let Some(name) = row.get("pv_name").and_then(|value| value.as_str()) else {
            continue;
        };
        match device_matches(name) {
            DeviceMatch::Same => {
                saw_target = true;
                if let Some(vg) = row
                    .get("vg_name")
                    .and_then(|value| value.as_str())
                    .filter(|vg| !vg.is_empty())
                {
                    return Ok(Some(vg.to_string()));
                }
            }
            DeviceMatch::Different => {}
            DeviceMatch::Unknown(why) => {
                return Err(LvmError::Failed(format!(
                    "pvs: cannot prove whether {name} is {part_dev} ({why}) — refusing to trust the report"
                )));
            }
        }
    }
    if saw_target {
        Ok(None)
    } else {
        Err(LvmError::Failed(format!(
            "pvs: no report for {part_dev} — is it an LVM2 PV?"
        )))
    }
}

/// 从 `lvs` JSON 解析落在 `part_dev` 上的顶层 LV。
fn parse_lvs_on_pv(
    json: &str,
    part_dev: &str,
    device_matches: &dyn Fn(&str) -> DeviceMatch,
) -> Result<Vec<(String, String)>, LvmError> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for row in report_rows(json, "lv")? {
        let Some(name) = row
            .get("lv_name")
            .and_then(|value| value.as_str())
            .map(str::to_string)
        else {
            return Err(LvmError::Failed(
                "lvs report row has no lv_name although the report asked for it — refusing to trust it"
                    .to_string(),
            ));
        };
        if name.starts_with('[') {
            continue;
        }
        let Some(path) = row
            .get("lv_path")
            .and_then(|value| value.as_str())
            .map(str::to_string)
        else {
            return Err(LvmError::Failed(format!(
                "lvs report row for {name} has no lv_path although the report asked for it — refusing to trust it"
            )));
        };
        let Some(devices) = row.get("devices").and_then(|value| value.as_str()) else {
            return Err(LvmError::Failed(format!(
                "lvs report row for {name} has no devices although the report asked for it — refusing to trust it"
            )));
        };
        let mut on_target = false;
        for entry in devices.split(',') {
            match device_matches(strip_pe_offset(entry)) {
                DeviceMatch::Same => {
                    on_target = true;
                    break;
                }
                DeviceMatch::Different => {}
                DeviceMatch::Unknown(why) => {
                    return Err(LvmError::Failed(format!(
                        "lvs: cannot prove whether {entry} is {part_dev} ({why}) — refusing to trust the report"
                    )));
                }
            }
        }
        if on_target && seen.insert((name.clone(), path.clone())) {
            out.push((name, path));
        }
    }
    Ok(out)
}

/// 从 `vgs` JSON 解析 VG extent 字节数；非正整数按不可解析拒绝（下一步要拿它作除数）。
fn parse_extent_size(json: &str) -> Result<u64, LvmError> {
    let row = report_rows(json, "vg")?
        .into_iter()
        .next()
        .ok_or_else(|| LvmError::Failed("vgs: report has no VG row".to_string()))?;
    let token = row
        .get("vg_extent_size")
        .and_then(|value| value.as_str())
        .ok_or_else(|| LvmError::Failed("vgs: no vg_extent_size in report".to_string()))?;
    let int_part = token.trim().split('.').next().unwrap_or("");
    match int_part.parse() {
        Ok(extent @ 1..) => Ok(extent),
        _ => Err(LvmError::Failed(format!(
            "vgs: vg_extent_size is not a positive integer: {token}"
        ))),
    }
}

/// 字节/计数字段：十进制整数，或带全零小数部分的定点渲染。
fn parse_num_field(field: &str) -> Option<u64> {
    let (int, frac) = match field.split_once('.') {
        Some((int, frac)) => (int, Some(frac)),
        None => (field, None),
    };
    if int.is_empty() || !int.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    if let Some(frac) = frac
        && (frac.is_empty() || !frac.bytes().all(|byte| byte == b'0'))
    {
        return None;
    }
    int.parse().ok()
}

fn parse_pv_size(
    stdout: &str,
    part_dev: &str,
    device_matches: &dyn Fn(&str) -> DeviceMatch,
) -> Result<PvSizeReport, LvmError> {
    let mut rows = stdout.lines().filter(|line| !line.trim().is_empty());
    let row = match (rows.next(), rows.next()) {
        (Some(row), None) => row,
        (None, _) => {
            return Err(LvmError::Failed(
                "pvs: empty report — cannot verify the PV size".to_string(),
            ));
        }
        (Some(_), Some(_)) => {
            return Err(LvmError::Failed(
                "pvs: expected exactly one PV row — cannot verify the PV size".to_string(),
            ));
        }
    };
    let fields: Vec<&str> = row.split('|').collect();
    if fields.len() != 6 {
        return Err(LvmError::Failed(format!(
            "pvs: expected 6 fields (pv_name,dev_size,pv_size,pe_start,pv_pe_count,pv_pe_alloc_count), got {} in {row:?}",
            fields.len()
        )));
    }
    match device_matches(fields[0].trim()) {
        DeviceMatch::Same => {}
        DeviceMatch::Different => {
            return Err(LvmError::Failed(format!(
                "pvs: report row is for {}, not {part_dev} — cannot verify the PV size",
                fields[0].trim()
            )));
        }
        DeviceMatch::Unknown(why) => {
            return Err(LvmError::Failed(format!(
                "pvs: cannot prove whether {} is {part_dev} ({why}) — refusing to trust the report",
                fields[0].trim()
            )));
        }
    }
    let num = |index: usize, field: &str| {
        parse_num_field(fields[index].trim()).ok_or_else(|| {
            LvmError::Failed(format!(
                "pvs: unparsable {field} {:?} in {row:?}",
                fields[index]
            ))
        })
    };
    let report = PvSizeReport {
        dev_size: num(1, "dev_size")?,
        pv_size: num(2, "pv_size")?,
        pe_start: num(3, "pe_start")?,
        pv_pe_count: num(4, "pv_pe_count")?,
        pv_pe_alloc_count: num(5, "pv_pe_alloc_count")?,
    };
    if report.dev_size == 0 || report.pv_pe_count == 0 {
        return Err(LvmError::Failed(format!(
            "pvs: report describes a zero-size PV ({row:?}) — the device is unreadable or mapped twice; cannot trust the report"
        )));
    }
    Ok(report)
}

/// 已分配段的统计。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct AllocSegments {
    allocated_extents: u64,
    max_end: u64,
}

fn parse_alloc_end(
    stdout: &str,
    part_dev: &str,
    device_matches: &dyn Fn(&str) -> DeviceMatch,
) -> Result<AllocSegments, LvmError> {
    let mut allocated_extents = 0u64;
    let mut max_end = 0u64;
    let mut rows = 0usize;
    for line in stdout.lines().filter(|line| !line.trim().is_empty()) {
        rows += 1;
        let fields: Vec<&str> = line.split('|').collect();
        if fields.len() != 4 {
            return Err(LvmError::Failed(format!(
                "pvs --segments: expected 4 fields (pv_name,pvseg_start,pvseg_size,lv_name), got {} in {line:?}",
                fields.len()
            )));
        }
        match device_matches(fields[0].trim()) {
            DeviceMatch::Same => {}
            DeviceMatch::Different => {
                return Err(LvmError::Failed(format!(
                    "pvs --segments: report row is for {}, not {part_dev} — cannot bound the allocated area",
                    fields[0].trim()
                )));
            }
            DeviceMatch::Unknown(why) => {
                return Err(LvmError::Failed(format!(
                    "pvs --segments: cannot prove whether {} is {part_dev} ({why}) — refusing to trust the report",
                    fields[0].trim()
                )));
            }
        }
        let num = |index: usize, field: &str| {
            parse_num_field(fields[index].trim()).ok_or_else(|| {
                LvmError::Failed(format!(
                    "pvs --segments: unparsable {field} {:?} in {line:?}",
                    fields[index]
                ))
            })
        };
        let start = num(1, "pvseg_start")?;
        let size = num(2, "pvseg_size")?;
        if fields[3].trim().is_empty() {
            continue;
        }
        let end = start.checked_add(size).ok_or_else(|| {
            LvmError::Failed(format!("pvs --segments: extent end overflows in {line:?}"))
        })?;
        allocated_extents = allocated_extents.checked_add(size).ok_or_else(|| {
            LvmError::Failed(format!(
                "pvs --segments: extent count overflows in {line:?}"
            ))
        })?;
        max_end = max_end.max(end);
    }
    if rows == 0 {
        return Err(LvmError::Failed(
            "pvs --segments: empty report — cannot bound the allocated area".to_string(),
        ));
    }
    Ok(AllocSegments {
        allocated_extents,
        max_end,
    })
}

/// `lvs` 一行五字段。
#[derive(Clone, Debug, PartialEq, Eq)]
struct LvRow {
    name: String,
    vg: String,
    path: String,
    size: u64,
    attr: String,
}

fn parse_lv_rows(stdout: &str) -> Result<Vec<LvRow>, LvmError> {
    let mut rows = Vec::new();
    for line in stdout.lines().filter(|line| !line.trim().is_empty()) {
        let fields: Vec<&str> = line.split('|').collect();
        if fields.len() != 5 {
            return Err(LvmError::Failed(format!(
                "lvs: expected 5 fields (lv_name,vg_name,lv_path,lv_size,lv_attr), got {} in {line:?}",
                fields.len()
            )));
        }
        let size = parse_num_field(fields[3].trim()).ok_or_else(|| {
            LvmError::Failed(format!(
                "lvs: unparsable lv_size {:?} in {line:?}",
                fields[3]
            ))
        })?;
        let (name, vg, path, attr) = (
            fields[0].trim(),
            fields[1].trim(),
            fields[2].trim(),
            fields[4].trim(),
        );
        if name.is_empty() || vg.is_empty() {
            return Err(LvmError::Failed(format!(
                "lvs: empty lv_name/vg_name in {line:?}"
            )));
        }
        rows.push(LvRow {
            name: name.to_string(),
            vg: vg.to_string(),
            path: path.to_string(),
            size,
            attr: attr.to_string(),
        });
    }
    Ok(rows)
}

/// `lv_attr` 第五位记激活状态。
fn attr_is_active(attr: &str) -> bool {
    attr.as_bytes().get(4) == Some(&b'a')
}

/// 一条 LV 的当前尺寸、所在 VG 与激活状态。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LvState {
    pub name: String,
    pub vg: String,
    pub size: u64,
    pub active: bool,
}

/// 该分区所属 VG；`Ok(None)` 表示 PV 不属于任何 VG。
pub fn vg_of(platform: &dyn Platform, part_dev: &str) -> Result<Option<String>, LvmError> {
    let stdout = tool_stdout(
        platform,
        Program::Pvs,
        &[
            "--reportformat".to_string(),
            "json".to_string(),
            "-o".to_string(),
            "pv_name,vg_name".to_string(),
            part_dev.to_string(),
        ],
    )?;
    if !report_parsable(&stdout) {
        return Err(LvmError::Failed(
            "pvs: unparsable report in output".to_string(),
        ));
    }
    parse_pv_vg(&stdout, part_dev, &|reported| {
        matches(platform, part_dev, reported)
    })
}

/// 落在该 PV 上的顶层 LV。
pub fn lvs_on_pv(
    platform: &dyn Platform,
    vg: &str,
    part_dev: &str,
) -> Result<Vec<(String, String)>, LvmError> {
    let stdout = tool_stdout(
        platform,
        Program::Lvs,
        &[
            "--reportformat".to_string(),
            "json".to_string(),
            "-o".to_string(),
            "lv_name,lv_path,devices".to_string(),
            vg.to_string(),
        ],
    )?;
    if !report_parsable(&stdout) {
        return Err(LvmError::Failed(
            "lvs: unparsable report in output".to_string(),
        ));
    }
    parse_lvs_on_pv(&stdout, part_dev, &|reported| {
        matches(platform, part_dev, reported)
    })
}

/// VG extent 大小（字节）。
pub fn vg_extent_size(platform: &dyn Platform, vg: &str) -> Result<u64, LvmError> {
    let stdout = tool_stdout(
        platform,
        Program::Vgs,
        &[
            "--reportformat".to_string(),
            "json".to_string(),
            "--units".to_string(),
            "b".to_string(),
            "--nosuffix".to_string(),
            "-o".to_string(),
            "vg_extent_size".to_string(),
            vg.to_string(),
        ],
    )?;
    parse_extent_size(&stdout)
}

/// 按路径取 LV 的当前尺寸，并核对它仍由这块 PV 承载。
pub fn lv_state(
    platform: &dyn Platform,
    part_dev: &str,
    lv_path: &str,
) -> Result<LvState, LvmError> {
    let vg = vg_of(platform, part_dev)?.ok_or_else(|| {
        LvmError::Refused(format!(
            "{part_dev} is a PV outside any VG — nothing to extend"
        ))
    })?;
    let on_pv = lvs_on_pv(platform, &vg, part_dev)?;
    if !on_pv.iter().any(|(_, path)| path == lv_path) {
        let names = on_pv
            .iter()
            .map(|(name, _)| name.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        return Err(LvmError::Refused(format!(
            "LV {lv_path} is no longer one of the top-level LVs on {part_dev} (VG {vg}: {names})"
        )));
    }
    let stdout = tool_stdout(
        platform,
        Program::Lvs,
        &[
            "--noheadings".to_string(),
            "--nosuffix".to_string(),
            "--units".to_string(),
            "b".to_string(),
            "--separator".to_string(),
            "|".to_string(),
            "-o".to_string(),
            "lv_name,vg_name,lv_path,lv_size,lv_attr".to_string(),
            vg.to_string(),
        ],
    )?;
    let rows = parse_lv_rows(&stdout)?;
    let mut hit: Vec<LvRow> = rows.into_iter().filter(|row| row.path == lv_path).collect();
    if hit.len() != 1 {
        return Err(LvmError::Failed(format!(
            "lvs: expected exactly one row for {lv_path}, got {}",
            hit.len()
        )));
    }
    let row = hit.pop().expect("the length check leaves exactly one row");
    if row.vg != vg {
        return Err(LvmError::Failed(format!(
            "lvs: {lv_path} is reported in VG {} rather than {vg} — refusing to extend it",
            row.vg
        )));
    }
    Ok(LvState {
        name: row.name,
        vg: row.vg,
        size: row.size,
        active: attr_is_active(&row.attr),
    })
}

/// 采集 PV 的尺寸字段。
pub fn pv_size_report(platform: &dyn Platform, part_dev: &str) -> Result<PvSizeReport, LvmError> {
    let stdout = tool_stdout(
        platform,
        Program::Pvs,
        &[
            "--noheadings".to_string(),
            "--nosuffix".to_string(),
            "--units".to_string(),
            "b".to_string(),
            "--separator".to_string(),
            "|".to_string(),
            "-o".to_string(),
            "pv_name,dev_size,pv_size,pe_start,pv_pe_count,pv_pe_alloc_count".to_string(),
            part_dev.to_string(),
        ],
    )?;
    parse_pv_size(&stdout, part_dev, &|reported| {
        matches(platform, part_dev, reported)
    })
}

/// 已分配 extent 的最小合法末端字节数 `pe_start + max_end × extent`。
pub fn pv_allocated_end_bytes(platform: &dyn Platform, part_dev: &str) -> Result<u64, LvmError> {
    let report = pv_size_report(platform, part_dev)?;
    if report.pv_pe_alloc_count == 0 {
        return Ok(report.pe_start);
    }
    let vg = vg_of(platform, part_dev)?.ok_or_else(|| {
        LvmError::Refused(format!(
            "{part_dev} reports {} allocated extents but belongs to no VG — cannot bound the allocated area",
            report.pv_pe_alloc_count
        ))
    })?;
    let extent = vg_extent_size(platform, &vg)?;
    let stdout = tool_stdout(
        platform,
        Program::Pvs,
        &[
            "--segments".to_string(),
            "--noheadings".to_string(),
            "--nosuffix".to_string(),
            "--separator".to_string(),
            "|".to_string(),
            "-o".to_string(),
            "pv_name,pvseg_start,pvseg_size,lv_name".to_string(),
            part_dev.to_string(),
        ],
    )?;
    let segments = parse_alloc_end(&stdout, part_dev, &|reported| {
        matches(platform, part_dev, reported)
    })?;
    if segments.allocated_extents != report.pv_pe_alloc_count {
        return Err(LvmError::Failed(format!(
            "pvs --segments accounts for {} allocated extents but pvs reports {} — cannot bound the allocated area",
            segments.allocated_extents, report.pv_pe_alloc_count
        )));
    }
    segments
        .max_end
        .checked_mul(extent)
        .and_then(|bytes| bytes.checked_add(report.pe_start))
        .ok_or_else(|| LvmError::Failed("pvs --segments: allocated end overflows".to_string()))
}

/// 只读选择目标顶层 LV：`--lv` 精确匹配裸名或 `vg/lv`；缺省要求该 PV 上只有一个顶层 LV。
pub fn select_lv(
    platform: &dyn Platform,
    part_dev: &str,
    want_lv: Option<&str>,
) -> Result<LvSelection, LvmError> {
    let vg = vg_of(platform, part_dev)?.ok_or_else(|| {
        LvmError::Refused(format!(
            "{part_dev} is a PV outside any VG — nothing to extend"
        ))
    })?;
    let lvs = lvs_on_pv(platform, &vg, part_dev)?;
    let (_, path) = match want_lv {
        Some(selection) => match lvs.iter().find(|(name, path)| {
            name == selection || path == selection || path.ends_with(&format!("/{selection}"))
        }) {
            Some(found) => found.clone(),
            None => {
                let names = lvs
                    .iter()
                    .map(|(name, _)| name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                return Err(LvmError::Refused(format!(
                    "LV {selection:?} not found on {part_dev} in VG {vg} (top-level LVs: {names})"
                )));
            }
        },
        None => match lvs.len() {
            1 => lvs[0].clone(),
            0 => {
                return Err(LvmError::Refused(format!(
                    "VG {vg}: no top-level LV uses {part_dev} — nothing to extend"
                )));
            }
            _ => {
                let names = lvs
                    .iter()
                    .map(|(name, _)| name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                return Err(LvmError::Refused(format!(
                    "VG {vg}: multiple LVs on {part_dev} — pick one with --lv NAME (candidates: {names})"
                )));
            }
        },
    };
    let extent_bytes = vg_extent_size(platform, &vg)?;
    let state = lv_state(platform, part_dev, &path)?;
    if !state.active {
        return Err(LvmError::Inactive(format!(
            "LV {path} is not active — --grow-lv extends an active LV only (lvextend -r resizes the live filesystem)"
        )));
    }
    Ok(LvSelection {
        path,
        extent_bytes,
        size: state.size,
    })
}

/// 扩容方向的 PV 链：先让 PV 吸收全部新增空间，必要时再扩目标 LV。
pub struct GrowChain<'a> {
    pub part_dev: &'a Path,
    pub expected_bytes: u64,
    pub delta_bytes: u64,
    pub grow_lv: bool,
    pub want_lv: Option<&'a str>,
}

/// 执行 PV 链；`grow_lv` 为真时按本次增量折算 extent 并扩 LV。
pub fn grow_chain(platform: &dyn Platform, chain: &GrowChain<'_>) -> Result<(), LvmError> {
    let dev = chain.part_dev.to_str().ok_or_else(|| {
        LvmError::Failed("the partition device path is not valid UTF-8".to_string())
    })?;
    let selection = if chain.grow_lv {
        Some(select_lv(platform, dev, chain.want_lv)?)
    } else {
        None
    };
    tool_stdout(platform, Program::Pvresize, &[dev.to_string()])?;
    let report = pv_size_report(platform, dev)?;
    if report.dev_size < chain.expected_bytes {
        return Err(LvmError::Failed(format!(
            "PV resize: {dev} reports dev_size {} bytes, below the resized partition ({} bytes) — the PV did not absorb the resize",
            report.dev_size, chain.expected_bytes
        )));
    }
    let Some(selection) = selection else {
        return Ok(());
    };
    let free = report
        .pv_pe_count
        .checked_sub(report.pv_pe_alloc_count)
        .ok_or_else(|| {
            LvmError::Failed(format!(
                "{dev} reports {} allocated extents out of {} — the PV report is inconsistent",
                report.pv_pe_alloc_count, report.pv_pe_count
            ))
        })?;
    if chain.delta_bytes == 0 {
        if free == 0 {
            return Ok(());
        }
        return Err(LvmError::Failed(format!(
            "{dev} already carries its new size, so this run has no amount to add, while {free} extent(s) on it are unallocated — the amount an interrupted resize meant for {} is not recorded on disk, and extending by whatever is free would also take space nobody asked for. If the LV is already the size you want, nothing is missing; otherwise extend it explicitly (lvextend -l +<N> -r {}) and rerun to confirm",
            selection.path, selection.path
        )));
    }
    let want = chain.delta_bytes / selection.extent_bytes;
    if want == 0 {
        return Err(LvmError::Failed(format!(
            "added {} bytes, less than one extent ({} bytes) — LV left unchanged",
            chain.delta_bytes, selection.extent_bytes
        )));
    }
    if want > free {
        return Err(LvmError::Failed(format!(
            "added {} bytes = {want} extents of {} bytes, but {dev} has only {free} unallocated extent(s) — {} left unchanged",
            chain.delta_bytes, selection.extent_bytes, selection.path
        )));
    }
    tool_stdout(
        platform,
        Program::Lvextend,
        &[
            "-l".to_string(),
            format!("+{want}"),
            "-r".to_string(),
            selection.path.clone(),
        ],
    )?;
    let after = lv_state(platform, dev, &selection.path)?;
    let grown = want.checked_mul(selection.extent_bytes).ok_or_else(|| {
        LvmError::Failed("the requested LV growth overflows the address space".to_string())
    })?;
    let target = selection.size.checked_add(grown).ok_or_else(|| {
        LvmError::Failed("the recorded LV target size overflows the address space".to_string())
    })?;
    if after.size < target {
        return Err(LvmError::Failed(format!(
            "lvextend -l +{want} reported success, but {} is {} bytes (expected at least {target})",
            selection.path, after.size
        )));
    }
    Ok(())
}

/// 缩容方向的 PV 链：把 PV 元数据缩到 `bytes` 字节。
pub fn shrink_pv(platform: &dyn Platform, part_dev: &Path, bytes: u64) -> Result<(), LvmError> {
    let dev = part_dev.to_str().ok_or_else(|| {
        LvmError::Failed("the partition device path is not valid UTF-8".to_string())
    })?;
    tool_stdout(
        platform,
        Program::Pvresize,
        &[
            "--yes".to_string(),
            "--setphysicalvolumesize".to_string(),
            format!("{bytes}b"),
            dev.to_string(),
        ],
    )?;
    Ok(())
}

/// 目标分区设备的持有者：镜像经 loop 临时映射，块设备直接用分区节点。
pub struct PartitionDevice {
    pub path: PathBuf,
    _binding: Option<crate::platform::LoopBinding>,
}

/// 为 LVM 链取出分区的块设备视图。
pub fn partition_device(
    platform: &dyn Platform,
    target: &Path,
    is_block: bool,
    partition: u32,
    start_bytes: u64,
    bytes: u64,
) -> Result<PartitionDevice, LvmError> {
    if is_block {
        let path = platform
            .partition_device(target, partition)
            .map_err(|error| LvmError::Failed(error.to_string()))?;
        Ok(PartitionDevice {
            path,
            _binding: None,
        })
    } else {
        let binding = platform
            .bind_loop(target, start_bytes, bytes)
            .map_err(|error| LvmError::Failed(error.to_string()))?;
        let path = binding.device().to_path_buf();
        Ok(PartitionDevice {
            path,
            _binding: Some(binding),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PVS_JSON: &str = r#"{
      "report": [ { "pv": [ {"pv_name":"dev/sda3","vg_name":"vg0"},
                            {"pv_name":"dev/sdb1","vg_name":""} ] } ]
    }"#;

    const LVS_JSON: &str = r#"{
      "report": [ { "lv": [
        {"lv_name":"root","lv_path":"dev/vg0/root","devices":"dev/sda3(0)"},
        {"lv_name":"data","lv_path":"dev/vg0/data","devices":"dev/sda3(128),dev/sdb1(0)"},
        {"lv_name":"other","lv_path":"dev/vg0/other","devices":"dev/sdb1(0)"},
        {"lv_name":"[pool0]","lv_path":"","devices":"dev/sda3(256)"}
      ] } ]
    }"#;

    fn exact(target: &str) -> impl Fn(&str) -> DeviceMatch + '_ {
        move |reported| {
            if reported == target {
                DeviceMatch::Same
            } else {
                DeviceMatch::Different
            }
        }
    }

    #[test]
    fn pv_vg_matches_exact_device() {
        let same = exact("dev/sda3");
        assert_eq!(
            parse_pv_vg(PVS_JSON, "dev/sda3", &same).unwrap(),
            Some("vg0".into())
        );
        let same = exact("dev/sdb1");
        assert_eq!(parse_pv_vg(PVS_JSON, "dev/sdb1", &same).unwrap(), None);
        let same = exact("dev/sdc1");
        assert!(parse_pv_vg(PVS_JSON, "dev/sdc1", &same).is_err());
    }

    #[test]
    fn lvs_match_prefix_not_substring() {
        let lvs = parse_lvs_on_pv(LVS_JSON, "dev/sda3", &exact("dev/sda3")).unwrap();
        let names: Vec<&str> = lvs.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(names, vec!["root", "data"]);
    }

    #[test]
    fn unknown_device_identity_is_refused() {
        let m = |name: &str| {
            if name == "dev/sda3" {
                DeviceMatch::Same
            } else {
                DeviceMatch::Unknown("stat failed".into())
            }
        };
        let error = parse_lvs_on_pv(LVS_JSON, "dev/sda3", &m).expect_err("身份不可证明");
        assert!(error.to_string().contains("cannot prove"), "{error}");
    }

    #[test]
    fn pe_offset_is_stripped_from_report_entries() {
        assert_eq!(strip_pe_offset("dev/sda3(0)"), "dev/sda3");
        assert_eq!(strip_pe_offset(" dev/sda3(128) "), "dev/sda3");
        assert_eq!(strip_pe_offset("dev/mapper/vg-lv"), "dev/mapper/vg-lv");
        assert_eq!(strip_pe_offset("dev/weird(name)"), "dev/weird(name)");
    }

    #[test]
    fn allocated_segments_skip_free_rows() {
        let rows = "dev/sda3|0|128|root\ndev/sda3|128|64|data\ndev/sda3|192|64|\n";
        let segments = parse_alloc_end(rows, "dev/sda3", &exact("dev/sda3")).unwrap();
        assert_eq!(
            segments,
            AllocSegments {
                allocated_extents: 192,
                max_end: 192,
            }
        );
    }

    #[test]
    fn allocated_segments_reject_unexpected_shape() {
        for bad in [
            "dev/sda3|0|128\n",
            "dev/sda3|0|x|root\n",
            "",
            "dev/sdb1|0|128|root\n",
        ] {
            assert!(
                parse_alloc_end(bad, "dev/sda3", &exact("dev/sda3")).is_err(),
                "{bad:?} must be refused"
            );
        }
    }

    #[test]
    fn extent_size_strips_decimal() {
        let json = r#"{"report":[{"vg":[{"vg_extent_size":"4194304.00"}]}]}"#;
        assert_eq!(parse_extent_size(json), Ok(4_194_304));
        assert!(parse_extent_size("{}").is_err());
        let zero = r#"{"report":[{"vg":[{"vg_extent_size":"0"}]}]}"#;
        assert!(parse_extent_size(zero).is_err());
    }

    #[test]
    fn a_malformed_report_is_an_error_not_an_empty_list() {
        assert!(report_rows("not json", "pv").is_err());
        assert!(report_rows("{}", "pv").is_err());
        assert!(
            report_rows(r#"{"report":[{"pv":[]}]}"#, "pv")
                .unwrap()
                .is_empty()
        );
    }

    const PVS_PLAIN: &str = "dev/sda3|21474836480.00|21474834432.00|1048576.00|5119|5119\n";

    #[test]
    fn pv_size_row_is_parsed_strictly() {
        let report = parse_pv_size(PVS_PLAIN, "dev/sda3", &exact("dev/sda3")).unwrap();
        assert_eq!(
            report,
            PvSizeReport {
                dev_size: 21_474_836_480,
                pv_size: 21_474_834_432,
                pe_start: 1_048_576,
                pv_pe_count: 5_119,
                pv_pe_alloc_count: 5_119,
            }
        );
    }

    #[test]
    fn pv_size_rows_that_cannot_be_verified_are_refused() {
        let matcher = exact("dev/sda3");
        for bad in [
            "",
            "dev/sda3|1|2|3\n",
            "dev/sda3|1|2|3|4\ndev/sdb1|1|2|3|4\n",
            "dev/sda3|1|2|3|4x\n",
            "dev/sda3|1.5|2|3|4\n",
            "dev/sdb1|1|2|3|4\n",
            "dev/sda3|0|0|0|0|0\n",
        ] {
            assert!(
                parse_pv_size(bad, "dev/sda3", &matcher).is_err(),
                "{bad:?} must be refused"
            );
        }
        let unknown = |_: &str| DeviceMatch::Unknown("stat failed".into());
        assert!(parse_pv_size(PVS_PLAIN, "dev/sda3", &unknown).is_err());
    }

    #[test]
    fn multi_report_rows_are_merged() {
        let json = r#"{"report":[{"lv":[{"lv_name":"a"}]},{"lv":[{"lv_name":"b"}]}]}"#;
        let names: Vec<String> = report_rows(json, "lv")
            .unwrap()
            .iter()
            .filter_map(|row| row.get("lv_name").and_then(|value| value.as_str()))
            .map(str::to_string)
            .collect();
        assert_eq!(names, vec!["a", "b"]);
    }

    #[test]
    fn duplicate_report_rows_collapse() {
        let json = r#"{"report":[
            {"lv":[{"lv_name":"lv1","lv_path":"dev/vg/lv1","devices":"dev/loop0p1(0)"}]},
            {"lv":[{"lv_name":"lv1","lv_path":"dev/vg/lv1","devices":"dev/loop0p1(0)"}]}
        ]}"#;
        assert_eq!(
            parse_lvs_on_pv(json, "dev/loop0p1", &exact("dev/loop0p1"))
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn attr_activation_is_the_fifth_character() {
        assert!(attr_is_active("-wi-a-----"));
        assert!(!attr_is_active("-wi-------"));
        assert!(!attr_is_active(""));
    }
}
