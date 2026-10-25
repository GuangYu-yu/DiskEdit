//! LVM PV 链式扩容（仅 Linux 运行时有效）：pvresize → 定位目标 LV → lvextend。
//! 全部经外部 lvm2 工具，报表解析用 --reportformat json（lvmreport(7) 定义的
//! JSON 报表结构，按程序解析设计，不依赖列序与名称字符集）；执行与解析分离，
//! 解析为纯函数可单测。pvresize(8)：扩容方向无前置条件（缩容才要求新末端之后
//! 无已分配 extent）；lvextend(8)：`-l +N` 按 extent 数增量扩，`-r/--resizefs`
//! 同步扩文件系统。
//!
//! PV 缩容本工具不做——链恰好与扩容反向：先 lvreduce -r 缩 LV + FS，再
//! pvresize --setphysicalvolumesize 缩 PV 元数据，最后改分区表。pvresize 拒绝
//! 时需先 pvmove 把 PE 前移，后者是 LVM 内部元数据重构，不在分区工具的范围内

use crate::fsops::{run, FsError};
use serde_json::Value;

/// LVM 链失败的两态：工具缺失是环境缺件（装 lvm2 后重试有意义），工具已运行的
/// 非零退出/报表不可解析则无法断言 LVM 元数据被动到什么程度。出口通道
/// （Infra/Failed/Pending）由调用方按 durable boundary 决定，这里只携带事实
#[derive(Debug)]
pub enum LvmError {
    /// 工具未安装（lvm2 包）
    ToolMissing(String),
    /// 工具已运行但非零退出，或输出不可解析
    Failed(String),
}

impl LvmError {
    /// 补救提示：工具名在错误正文里（可能是 lvm2 的 pvresize，也可能是镜像路径所需的
    /// losetup），故提示直接沿用正文，不按变体猜包名
    pub fn hint(&self) -> String {
        match self {
            Self::ToolMissing(_) => format!("install the missing tool and re-run: {self}"),
            Self::Failed(_) => "pvresize <part>; lvextend -l +<ext> -r <lv> (see pvresize(8))".to_string(),
        }
    }
}

impl std::fmt::Display for LvmError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ToolMissing(m) | Self::Failed(m) => f.write_str(m),
        }
    }
}

impl From<FsError> for LvmError {
    fn from(e: FsError) -> Self {
        match e {
            // 工具缺失原样保留分类；其余变体在 LvmError 无对应通道，统一压成 Failed
            FsError::ToolMissing(m) => Self::ToolMissing(m),
            e => Self::Failed(e.to_string()),
        }
    }
}

/// 执行 lvm2 工具并取 stdout；区分"工具未安装"与"命令失败"。
/// `run` 已固定 `LC_ALL=C`，报表文本可直接按程序解析
fn run_tool(tool: &str, args: &[&str]) -> Result<String, LvmError> {
    match run(tool, args) {
        // 工具缺失要装包，与"命令跑失败"是两条不同的补救路径，分开报
        Err(FsError::ToolMissing(_)) => Err(LvmError::ToolMissing(format!(
            "LVM tooling missing: {tool} not found in PATH (install lvm2)"
        ))),
        Err(e) => Err(LvmError::Failed(format!("{tool} failed: {e}"))),
        Ok(out) if !out.status.success() => Err(LvmError::Failed(format!(
            "{tool} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ))),
        Ok(out) => Ok(String::from_utf8_lossy(&out.stdout).into_owned()),
    }
}

/// lvm2 JSON 报表的外层结构是否可解析。解析失败与"报表里没有行"必须分开：
/// 前者是工具输出损坏，后者才是"没有这个对象"
fn report_parsable(json: &str) -> bool {
    serde_json::from_str::<Value>(json)
        .ok()
        .and_then(|v| v.get("report").and_then(|r| r.as_array()).map(|_| ()))
        .is_some()
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

/// 报告里的设备名与目标设备是不是同一块设备：路径字符串只用于定位与诊断，
/// 不能单独授权可能破坏 LVM 元数据的操作（同一路径在两次检查之间可能已被重绑）。
///
/// 三态：块设备按 `rdev` 比对；非块设备按 `(dev, ino)` 比对；任一侧取不到元数据
/// 即 `Unknown`——"无法证明相同"不等于"不同"，调用方据此拒绝而不是放行
pub enum DeviceMatch {
    Same,
    Different,
    Unknown(String),
}

/// `lvmreport` 的 devices 条目形如 `/dev/sda3(0)`：去掉 PE 偏移再比对。
/// 名字本身含 `(...)` 且括号内不是纯数字时原样保留
fn strip_pe_offset(entry: &str) -> &str {
    let t = entry.trim();
    if let Some(i) = t.rfind('(')
        && t.ends_with(')')
        && t[i + 1..t.len() - 1].bytes().all(|b| b.is_ascii_digit())
    {
        return &t[..i];
    }
    t
}

fn device_match(target: &str, reported: &str) -> DeviceMatch {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    let a = match std::fs::metadata(target) {
        Ok(m) => m,
        Err(e) => return DeviceMatch::Unknown(format!("cannot stat {target}: {e}")),
    };
    let b = match std::fs::metadata(reported) {
        Ok(m) => m,
        Err(e) => return DeviceMatch::Unknown(format!("cannot stat {reported}: {e}")),
    };
    let same = if a.file_type().is_block_device() && b.file_type().is_block_device() {
        a.rdev() == b.rdev()
    } else {
        // 镜像/普通文件：同一条路径的两个写法指向同一个 inode
        a.dev() == b.dev() && a.ino() == b.ino()
    };
    if same { DeviceMatch::Same } else { DeviceMatch::Different }
}

/// 从 pvs JSON 输出解析 part_dev 所属 VG 名。
/// `Ok(None)` = 报告的正是本设备、但它不属于任何 VG；
/// `Err` = 报告里没有可证明为本设备的行，或存在无法判定身份的行
pub fn parse_pv_vg(json: &str, part_dev: &str) -> Result<Option<String>, LvmError> {
    parse_pv_vg_with(json, part_dev, &|reported| device_match(part_dev, reported))
}

fn parse_pv_vg_with(
    json: &str,
    part_dev: &str,
    matches: &dyn Fn(&str) -> DeviceMatch,
) -> Result<Option<String>, LvmError> {
    let mut saw_target = false;
    for row in report_rows(json, "pv") {
        let Some(name) = row.get("pv_name").and_then(|v| v.as_str()) else { continue };
        match matches(name) {
            DeviceMatch::Same => {
                saw_target = true;
                if let Some(vg) = row.get("vg_name").and_then(|v| v.as_str()).filter(|vg| !vg.is_empty()) {
                    return Ok(Some(vg.to_string()));
                }
            }
            DeviceMatch::Different => {}
            DeviceMatch::Unknown(why) => {
                return Err(LvmError::Failed(format!(
                    "pvs: cannot prove whether {name} is {part_dev} ({why}) — refusing to trust the report"
                )))
            }
        }
    }
    if saw_target {
        Ok(None)
    } else {
        Err(LvmError::Failed(format!("pvs: no report for {part_dev} — is it an LVM2 PV?")))
    }
}

/// 从 lvs JSON 输出解析落在 part_dev 上的顶层 LV（lv_name, lv_path）。
/// devices 为 PV 列表，条目形如 "/dev/sda3(0)"、多设备逗号分隔（lvmreport 的字段
/// 输出格式，非稳定 ABI）；方括号名（[pool0]/[lvol0_pmspare] 等）是 LVM 内部卷，排除。
///
/// 身份按 [`DeviceMatch`] 三态判定：任一条目无法证明是否为目标设备即整体拒绝，
/// 否则"无法证明"会被静默当成"不在本设备上"，进而漏掉持有该 PV 的 LV
pub fn parse_lvs_on_pv(json: &str, part_dev: &str) -> Result<Vec<(String, String)>, LvmError> {
    parse_lvs_on_pv_with(json, part_dev, &|reported| device_match(part_dev, reported))
}

fn parse_lvs_on_pv_with(
    json: &str,
    part_dev: &str,
    matches: &dyn Fn(&str) -> DeviceMatch,
) -> Result<Vec<(String, String)>, LvmError> {
    // report 数组逐对象展平（report_rows），同一 LV 可能在多个 report 对象中各出现一次
    // （如 duplicate PV 扫描）；同 VG 内两个同名 LV 不可能存在，逐 (name,path) 去重，
    // 否则单 LV 自动选择会被重复行误判成多 LV 拒绝
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for row in report_rows(json, "lv") {
        let Some(name) = row.get("lv_name").and_then(|v| v.as_str()).map(str::to_string) else { continue };
        if name.starts_with('[') {
            continue;
        }
        let Some(path) = row.get("lv_path").and_then(|v| v.as_str()).map(str::to_string) else { continue };
        let Some(devices) = row.get("devices").and_then(|v| v.as_str()) else { continue };
        let mut on_target = false;
        for entry in devices.split(',') {
            let dev = strip_pe_offset(entry);
            match matches(dev) {
                DeviceMatch::Same => {
                    on_target = true;
                    break;
                }
                DeviceMatch::Different => {}
                DeviceMatch::Unknown(why) => {
                    return Err(LvmError::Failed(format!(
                        "lvs: cannot prove whether {dev} is {part_dev} ({why}) — refusing to trust the report"
                    )))
                }
            }
        }
        if on_target && seen.insert((name.clone(), path.clone())) {
            out.push((name, path));
        }
    }
    Ok(out)
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
    // extent 大小要当下一步除法的分母（lvm_grow_chain 的 delta_bytes / ext）：
    // "0" 是畸形报告不是合法 VG，放过去就是除零 panic（release 也一样），这里拒绝成 None
    match int_part.parse() {
        Ok(ext @ 1..) => Some(ext),
        _ => None,
    }
}

/// `pvs --units b` 采集的单个 PV 行。`dev_size` 是底层设备字节数，`pv_size` 是
/// LVM 认定的 PV 可用字节数——extent 对齐与 PV 元数据使后者不必然等于前者，
/// 故两者分别保留；后置条件只用 `dev_size`
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PvSizeReport {
    pub dev_size: u64,
    pub pv_size: u64,
    pub pe_start: u64,
    pub pv_pe_count: u64,
    /// 已分配给 LV 的 extent 数；`pv_pe_count - pv_pe_alloc_count` 是这块 PV 上
    /// 还没交给任何 LV 的 extent——它是持久事实，而"本次运行新增了多少"不是
    pub pv_pe_alloc_count: u64,
}

/// 报表里的字节/计数字段：十进制整数，或带全零小数部分的定点渲染。
/// 非零小数在字节单位下不是可表示的整数值，按不可解析拒绝而不是四舍五入
fn parse_num_field(field: &str) -> Option<u64> {
    let (int, frac) = match field.split_once('.') {
        Some((i, f)) => (i, Some(f)),
        None => (field, None),
    };
    if int.is_empty() || !int.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    if let Some(f) = frac
        && (f.is_empty() || !f.bytes().all(|b| b == b'0'))
    {
        return None;
    }
    int.parse().ok()
}

/// `pvs` 五字段输出的严格解析：恰好一行、`|` 分隔五段、名字按三态身份比对、
/// 其余四段为数字。任一条件不成立都是"无法验证"——不能当作扩容成功
fn parse_pv_size_with(
    stdout: &str,
    part_dev: &str,
    matches: &dyn Fn(&str) -> DeviceMatch,
) -> Result<PvSizeReport, LvmError> {
    let mut rows = stdout.lines().filter(|l| !l.trim().is_empty());
    let row = match (rows.next(), rows.next()) {
        (Some(row), None) => row,
        (None, _) => return Err(LvmError::Failed("pvs: empty report — cannot verify the PV size".to_string())),
        (Some(_), Some(_)) => {
            return Err(LvmError::Failed("pvs: expected exactly one PV row — cannot verify the PV size".to_string()))
        }
    };
    let fields: Vec<&str> = row.split('|').collect();
    if fields.len() != 6 {
        return Err(LvmError::Failed(format!(
            "pvs: expected 6 fields (pv_name,dev_size,pv_size,pe_start,pv_pe_count,pv_pe_alloc_count), got {} in {row:?}",
            fields.len()
        )));
    }
    let name = fields[0].trim();
    match matches(name) {
        DeviceMatch::Same => {}
        DeviceMatch::Different => {
            return Err(LvmError::Failed(format!(
                "pvs: report row is for {name}, not {part_dev} — cannot verify the PV size"
            )))
        }
        DeviceMatch::Unknown(why) => {
            return Err(LvmError::Failed(format!(
                "pvs: cannot prove whether {name} is {part_dev} ({why}) — refusing to trust the report"
            )))
        }
    }
    let num = |i: usize, field: &str| {
        parse_num_field(fields[i].trim())
            .ok_or_else(|| LvmError::Failed(format!("pvs: unparsable {field} {:?} in {row:?}", fields[i])))
    };
    Ok(PvSizeReport {
        dev_size: num(1, "dev_size")?,
        pv_size: num(2, "pv_size")?,
        pe_start: num(3, "pe_start")?,
        pv_pe_count: num(4, "pv_pe_count")?,
        pv_pe_alloc_count: num(5, "pv_pe_alloc_count")?,
    })
}

/// 采集 PV 的尺寸字段。命令形态固定为
/// `pvs --noheadings --nosuffix --units b --separator '|' -o <五字段> <part_dev>`；
/// 输出与这个形态有任何偏差都按"无法验证"返回 `Failed`，不降级为成功
pub fn pv_size_report(part_dev: &str) -> Result<PvSizeReport, LvmError> {
    let out = run_tool(
        "pvs",
        &[
            "--noheadings", "--nosuffix", "--units", "b", "--separator", "|",
            "-o", "pv_name,dev_size,pv_size,pe_start,pv_pe_count,pv_pe_alloc_count", part_dev,
        ],
    )?;
    parse_pv_size_with(&out, part_dev, &|reported| device_match(part_dev, reported))
}

/// 该分区所属 VG 名；Ok(None) = 有 PV 标签但不属任何 VG（或工具返回空），
/// Err = 基础设施失败（lvm2 缺失、命令失败、JSON 不可解析）
pub fn vg_of(part_dev: &str) -> Result<Option<String>, LvmError> {
    // -o 字段名取自 man pvs
    let out = run_tool("pvs", &["--reportformat", "json", "-o", "pv_name,vg_name", part_dev])?;
    // 与 lvs_on_pv 同一判据：解析不出不等于"这块盘不是 PV"
    if !report_parsable(&out) {
        return Err(LvmError::Failed("pvs: unparsable report in output".to_string()));
    }
    parse_pv_vg(&out, part_dev)
}

/// 落在该 PV 上的顶层 LV（lv_name, lv_path）
pub fn lvs_on_pv(vg: &str, part_dev: &str) -> Result<Vec<(String, String)>, LvmError> {
    let out = run_tool("lvs", &["--reportformat", "json", "-o", "lv_name,lv_path,devices", vg])?;
    // "确认无行"与"解析不出"必须分开：后者折叠成空表会被调用方当成
    // "no top-level LV"，把故障引向错误的出路（挑 --lv / 判定无从扩）
    if !report_parsable(&out) {
        return Err(LvmError::Failed("lvs: unparsable report in output".to_string()));
    }
    parse_lvs_on_pv(&out, part_dev)
}

/// VG extent 大小（字节）
pub fn vg_extent_size(vg: &str) -> Result<u64, LvmError> {
    let out = run_tool("vgs", &["--reportformat", "json", "--units", "b", "--nosuffix", "-o", "vg_extent_size", vg])?;
    parse_extent_size(&out).ok_or_else(|| LvmError::Failed("vgs: unparsable vg_extent_size in output".to_string()))
}

/// PV 吸收分区新增的全部空间（扩容方向无前置条件，pvresize(8)）
pub fn pv_resize(part_dev: &str) -> Result<(), LvmError> {
    run_tool("pvresize", &[part_dev]).map(|_| ())
}

/// LV 增量扩 n 个 extent 并同步扩文件系统（lvextend -l +N -r）
pub fn lv_extend(lv_path: &str, extents: u64) -> Result<(), LvmError> {
    run_tool("lvextend", &["-l", &format!("+{extents}"), "-r", lv_path]).map(|_| ())
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

    /// 纯解析测试不依赖真实设备节点：字符串相等即 Same，其余 Different
    fn exact(target: &str) -> impl Fn(&str) -> DeviceMatch + '_ {
        move |reported| if reported == target { DeviceMatch::Same } else { DeviceMatch::Different }
    }

    #[test]
    fn pv_vg_matches_exact_device() {
        let same = exact("/dev/sda3");
        assert_eq!(parse_pv_vg_with(PVS_JSON, "/dev/sda3", &same).unwrap(), Some("vg0".into()));
        // PV 无 VG → Ok(None)
        let same = exact("/dev/sdb1");
        assert_eq!(parse_pv_vg_with(PVS_JSON, "/dev/sdb1", &same).unwrap(), None);
        // 非 PV 设备 → 报告里没有可证明为本设备的行
        let same = exact("/dev/sdc1");
        assert!(parse_pv_vg_with(PVS_JSON, "/dev/sdc1", &same).is_err());
    }

    #[test]
    fn lvs_match_prefix_not_substring() {
        let lvs = parse_lvs_on_pv_with(LVS_JSON, "/dev/sda3", &exact("/dev/sda3")).unwrap();
        let names: Vec<&str> = lvs.iter().map(|(n, _)| n.as_str()).collect();
        // "other" 在 sdb1 上；"[pool0]" 是内部卷——都不该出现
        assert_eq!(names, vec!["root", "data"]);
    }

    /// 无法证明某个 devices 条目是不是目标设备时整体拒绝：静默当作"不同"
    /// 会漏掉持有该 PV 的 LV，随即在错误的 LV 集合上做授权判断
    #[test]
    fn unknown_device_identity_is_refused() {
        let m = |name: &str| {
            if name == "/dev/sda3" { DeviceMatch::Same } else { DeviceMatch::Unknown("stat failed".into()) }
        };
        let e = match parse_lvs_on_pv_with(LVS_JSON, "/dev/sda3", &m) {
            Ok(_) => panic!("expected an identity error"),
            Err(e) => e.to_string(),
        };
        assert!(e.contains("cannot prove"), "{e}");
    }

    /// devices 条目去掉 `(PE 偏移)` 后比对；非数字括号原样保留
    #[test]
    fn pe_offset_is_stripped_from_report_entries() {
        assert_eq!(strip_pe_offset("/dev/sda3(0)"), "/dev/sda3");
        assert_eq!(strip_pe_offset(" /dev/sda3(128) "), "/dev/sda3");
        assert_eq!(strip_pe_offset("/dev/mapper/vg-lv"), "/dev/mapper/vg-lv");
        assert_eq!(strip_pe_offset("/dev/weird(name)"), "/dev/weird(name)");
    }

    #[test]
    fn extent_size_strips_decimal() {
        let json = r#"{"report":[{"vg":[{"vg_extent_size":"4194304.00"}]}]}"#;
        assert_eq!(parse_extent_size(json), Some(4194304));
        assert_eq!(parse_extent_size("{}"), None);
    }

    const PVS_PLAIN: &str = "/dev/sda3|21474836480.00|21474834432.00|1048576.00|5119\n";

    #[test]
    fn pv_size_row_is_parsed_strictly() {
        let r = parse_pv_size_with(PVS_PLAIN, "/dev/sda3", &exact("/dev/sda3")).unwrap();
        assert_eq!(
            r,
            PvSizeReport {
                dev_size: 21_474_836_480,
                pv_size: 21_474_834_432,
                pe_start: 1_048_576,
                pv_pe_count: 5_119,
                pv_pe_alloc_count: 5_119,
            }
        );
    }

    /// 空输出、多行、字段数不符、非数字、非整字节、身份不符都按"无法验证"拒绝
    #[test]
    fn pv_size_rows_that_cannot_be_verified_are_refused() {
        let m = exact("/dev/sda3");
        for bad in [
            "",
            "/dev/sda3|1|2|3\n",
            "/dev/sda3|1|2|3|4\n/dev/sdb1|1|2|3|4\n",
            "/dev/sda3|1|2|3|4x\n",
            "/dev/sda3|1.5|2|3|4\n",
            "/dev/sdb1|1|2|3|4\n",
        ] {
            assert!(parse_pv_size_with(bad, "/dev/sda3", &m).is_err(), "{bad:?} must be refused");
        }
        // 无法证明身份同样拒绝
        let unknown = |_: &str| DeviceMatch::Unknown("stat failed".into());
        assert!(parse_pv_size_with(PVS_PLAIN, "/dev/sda3", &unknown).is_err());
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

    #[test]
    fn duplicate_report_rows_collapse() {
        // 同一 LV 经多 report 对象重复出现（duplicate PV 扫描的 CI 实测形态）：
        // 去重后单 LV 自动选择不得误判为多 LV
        let json = r#"{"report":[
            {"lv":[{"lv_name":"lv1","lv_path":"/dev/vg/lv1","devices":"/dev/loop0p1(0)"}]},
            {"lv":[{"lv_name":"lv1","lv_path":"/dev/vg/lv1","devices":"/dev/loop0p1(0)"}]}
        ]}"#;
        assert_eq!(parse_lvs_on_pv_with(json, "/dev/loop0p1", &exact("/dev/loop0p1")).unwrap().len(), 1);
    }
}