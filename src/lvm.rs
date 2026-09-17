//! LVM PV 链式扩容（仅 Linux 运行时有效）：pvresize → 定位目标 LV → lvextend。
//! 全部经外部 lvm2 工具，报表解析用 --reportformat json（lvmreport(7) 定义的
//! JSON 报表结构，按程序解析设计，不依赖列序与名称字符集）；执行与解析分离，
//! 解析为纯函数可单测。pvresize(8)：扩容方向无前置条件（缩容才要求新末端之后
//! 无已分配 extent）；lvextend(8)：`-l +N` 按 extent 数增量扩，`-r/--resizefs`
//! 同步扩文件系统。

use crate::fsops::run;
use serde_json::Value;
use std::io;

/// 执行 lvm2 工具并取 stdout；区分"工具未安装"与"命令失败"
fn run_json(tool: &str, args: &[&str]) -> Result<String, String> {
    match run(tool, args) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            Err(format!("LVM tooling missing: {tool} not found in PATH (install lvm2)"))
        }
        Err(e) => Err(format!("{tool} failed: {e}")),
        Ok(out) if !out.status.success() => Err(format!(
            "{tool} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )),
        Ok(out) => Ok(String::from_utf8_lossy(&out.stdout).into_owned()),
    }
}

/// 取 report[].<key>[] 数组（lvm2 JSON 报表的统一入口）。
/// 遍历全部 report 元素合并：普通 lvs/pvs/vgs 恒为单元素，LVM 亦允许
/// 单命令多 report（如 lvm fullreport）
fn report_rows(json: &str, key: &str) -> Vec<Value> {
    let Ok(v) = serde_json::from_str::<Value>(json) else { return Vec::new() };
    let Some(reports) = v.get("report").and_then(|r| r.as_array()) else { return Vec::new() };
    reports
        .iter()
        .filter_map(|rep| rep.get(key).and_then(|k| k.as_array()))
        .flatten()
        .cloned()
        .collect()
}

/// 从 pvs JSON 输出解析 part_dev 所属 VG 名；无该 PV 或 vg_name 为空 → None
pub fn parse_pv_vg(json: &str, part_dev: &str) -> Option<String> {
    report_rows(json, "pv").into_iter().find_map(|row| {
        let name = row.get("pv_name")?.as_str()?;
        let vg = row.get("vg_name")?.as_str()?;
        (name == part_dev && !vg.is_empty()).then(|| vg.to_string())
    })
}

/// 从 lvs JSON 输出解析落在 part_dev 上的顶层 LV（lv_name, lv_path）。
/// devices 为 PV 列表，条目形如 "/dev/sda3(0)"、多设备逗号分隔（lvmreport 的字段
/// 输出格式，非稳定 ABI，故只做 "PV路径(" 前缀匹配不做严格解析）；方括号名
/// （[pool0]/[lvol0_pmspare] 等）是 LVM 内部卷，排除。
pub fn parse_lvs_on_pv(json: &str, part_dev: &str) -> Vec<(String, String)> {
    report_rows(json, "lv")
        .into_iter()
        .filter_map(|row| {
            let name = row.get("lv_name")?.as_str()?.to_string();
            let path = row.get("lv_path")?.as_str()?.to_string();
            let devices = row.get("devices")?.as_str()?;
            Some((name, path, devices.to_string()))
        })
        .filter(|(name, _, _)| !name.starts_with('['))
        .filter(|(_, _, devices)| {
            // 子串 "PV路径(" 精确匹配（尾部 '(' 挡住 /dev/sda3 误配 /dev/sda33）；
            // 不按逗号分割：设备名可能含逗号
            devices.contains(&format!("{part_dev}("))
        })
        .map(|(name, path, _)| (name, path))
        .collect()
}

/// 从 vgs JSON 输出解析 VG extent 大小（字节）。--units b --nosuffix 下
/// 值形如 "4194304.00"，取小数点前整部
pub fn parse_extent_size(json: &str) -> Option<u64> {
    let tok = report_rows(json, "vg")
        .into_iter()
        .next()?
        .get("vg_extent_size")?
        .as_str()?
        .to_string();
    let int_part = tok.trim().split('.').next()?;
    int_part.parse().ok()
}

/// 该分区所属 VG 名；Ok(None) = 有 PV 标签但不属任何 VG（或工具返回空），
/// Err = 基础设施失败（lvm2 缺失、命令失败、JSON 不可解析）
pub fn vg_of(part_dev: &str) -> Result<Option<String>, String> {
    // -o 字段名取自 man pvs
    let out = run_json("pvs", &["--reportformat", "json", "-o", "pv_name,vg_name", part_dev])?;
    match parse_pv_vg(&out, part_dev) {
        Some(vg) => Ok(Some(vg)),
        // 有 PV 行但无 VG：以解析出的 pv 行数组非空为准，不做字符串匹配
        None if !report_rows(&out, "pv").is_empty() => Ok(None),
        None => Err(format!("pvs: no report for {part_dev} — is it an LVM2 PV?")),
    }
}

/// 落在该 PV 上的顶层 LV（lv_name, lv_path）
pub fn lvs_on_pv(vg: &str, part_dev: &str) -> Result<Vec<(String, String)>, String> {
    let out = run_json("lvs", &["--reportformat", "json", "-o", "lv_name,lv_path,devices", vg])?;
    Ok(parse_lvs_on_pv(&out, part_dev))
}

/// VG extent 大小（字节）
pub fn vg_extent_size(vg: &str) -> Result<u64, String> {
    let out = run_json("vgs", &["--reportformat", "json", "--units", "b", "--nosuffix", "-o", "vg_extent_size", vg])?;
    parse_extent_size(&out).ok_or_else(|| "vgs: unparsable vg_extent_size in output".to_string())
}

/// PV 吸收分区新增的全部空间（扩容方向无前置条件，pvresize(8)）
pub fn pv_resize(part_dev: &str) -> Result<(), String> {
    run_json("pvresize", &[part_dev]).map(|_| ())
}

/// LV 增量扩 n 个 extent 并同步扩文件系统（lvextend -l +N -r）
pub fn lv_extend(lv_path: &str, extents: u64) -> Result<(), String> {
    run_json("lvextend", &["-l", &format!("+{extents}"), "-r", lv_path]).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    const PVS_JSON: &str = r#"{
      "report": [ { "pv": [ {"pv_name":"/dev/sda3","vg_name":"vg0"},
                            {"pv_name":"/dev/sdb1","vg_name":""} ] } ]
    }"#;

    const LVS_JSON: &str = r#"{
      "report": [ { "lv": [
        {"lv_name":"root","lv_path":"/dev/vg0/root","devices":"/dev/sda3(0)"},
        {"lv_name":"data","lv_path":"/dev/vg0/data","devices":"/dev/sda3(128),/dev/sdb1(0)"},
        {"lv_name":"other","lv_path":"/dev/vg0/other","devices":"/dev/sdb1(0)"},
        {"lv_name":"[pool0]","lv_path":"","devices":"/dev/sda3(256)"}
      ] } ]
    }"#;

    #[test]
    fn pv_vg_matches_exact_device() {
        assert_eq!(parse_pv_vg(PVS_JSON, "/dev/sda3"), Some("vg0".into()));
        // PV 无 VG → None
        assert_eq!(parse_pv_vg(PVS_JSON, "/dev/sdb1"), None);
        // 非 PV 设备 → None
        assert_eq!(parse_pv_vg(PVS_JSON, "/dev/sdc1"), None);
    }

    #[test]
    fn lvs_match_prefix_not_substring() {
        let lvs = parse_lvs_on_pv(LVS_JSON, "/dev/sda3");
        let names: Vec<&str> = lvs.iter().map(|(n, _)| n.as_str()).collect();
        // "other" 在 sdb1 上；"[pool0]" 是内部卷——都不该出现
        assert_eq!(names, vec!["root", "data"]);
    }

    #[test]
    fn extent_size_strips_decimal() {
        let json = r#"{"report":[{"vg":[{"vg_extent_size":"4194304.00"}]}]}"#;
        assert_eq!(parse_extent_size(json), Some(4194304));
        assert_eq!(parse_extent_size("{}"), None);
    }

    #[test]
    fn multi_report_rows_are_merged() {
        // 单命令多 report（lvm fullreport 形态）：行分属不同 report 元素，合并后可见
        let json = r#"{"report":[{"lv":[{"lv_name":"a"}]},{"lv":[{"lv_name":"b"}]}]}"#;
        let names: Vec<String> = report_rows(json, "lv")
            .iter()
            .filter_map(|r| r.get("lv_name").and_then(|v| v.as_str()).map(|s| s.to_string()))
            .collect();
        assert_eq!(names, vec!["a", "b"]);
    }
}