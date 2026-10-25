//! PV 扩容后的 LV 链式扩容。

#[cfg(target_os = "linux")]
use super::*;

/// `lvm_grow_chain` 会启动的外部命令清单。写表之前按它预检：链条在表已提交之后才跑，
/// 缺工具那时才发现，盘已改，只剩 PARTIAL 加手工补救。
/// `needs_loop`：镜像目标的分区要经 losetup 临时映射（块设备直接用现成分区节点），
/// 该工具与 pvresize 一样必须在写表之前确认存在
#[cfg(target_os = "linux")]
pub(crate) fn preflight_lvm_tools(grow_lv: bool, needs_loop: bool) -> Result<(), crate::fsops::FsError> {
    // pvs 供 pvresize 后的 dev_size 核对；grow-lv 另需 lvs/vgs 定位 LV 与 extent、lvextend 扩 LV
    let tools: &[&str] = match (grow_lv, needs_loop) {
        (true, true) => &["pvresize", "pvs", "lvs", "vgs", "lvextend", "losetup"],
        (true, false) => &["pvresize", "pvs", "lvs", "vgs", "lvextend"],
        (false, true) => &["pvresize", "pvs", "losetup"],
        (false, false) => &["pvresize", "pvs"],
    };
    tools.iter().try_for_each(|t| crate::fsops::require_tool(t))
}

/// 分区扩容成功后的 LVM 链（仅块设备）：pvresize 吸收全部新增空间；--grow-lv 把本次新增
/// 传给目标 LV（--lv 指定或该 PV 上唯一顶层 LV），向下取整到 VG extent，不消费原有空闲。
/// lvextend 按容量扩，分配源由 LVM 决定，不限于本 PV。
/// 后置条件：pvresize 后 `pvs` 的 dev_size 必须覆盖整段分区。`pv_size` 是 PV 可用
/// 尺寸（extent 对齐与 PV 元数据使其不必等于设备尺寸），只随输出记录，不作为判据
#[cfg(target_os = "linux")]
pub(crate) fn lvm_grow_chain(part_dev: &str, expected_bytes: u64, delta_bytes: u64, grow_lv: bool, want_lv: Option<&str>) -> Result<(), LvmError> {
    crate::lvm::pv_resize(part_dev)?;
    println!("pvresize {part_dev} done");
    let pv = crate::lvm::pv_size_report(part_dev)?;
    if pv.dev_size < expected_bytes {
        return Err(LvmError::Failed(format!(
            "pvresize: {part_dev} reports dev_size {} bytes, below the resized partition ({expected_bytes} bytes) — the PV did not absorb the resize",
            pv.dev_size
        )));
    }
    println!(
        "pvs {part_dev}: dev_size={} pv_size={} pe_start={} pv_pe_count={}",
        pv.dev_size, pv.pv_size, pv.pe_start, pv.pv_pe_count
    );
    if !grow_lv {
        return Ok(());
    }
    let vg = match crate::lvm::vg_of(part_dev) {
        Ok(Some(vg)) => vg,
        Ok(None) => return Err(LvmError::Failed(format!("pvresize done but {part_dev} is a PV outside any VG — nothing to extend"))),
        Err(e) => return Err(e),
    };
    let lvs = crate::lvm::lvs_on_pv(&vg, part_dev)?;
    let (name, path) = match want_lv {
        // 匹配裸 LV 名或 /dev/<vg>/<name> 路径（lv_path 精确比较，不做后缀模糊匹配）
        Some(sel) => match lvs.iter().find(|(n, p)| n == sel || p == &format!("/dev/{sel}")) {
            Some(x) => x.clone(),
            None => {
                let names = lvs.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>().join(", ");
                return Err(LvmError::Failed(format!(
                    "LV {sel:?} not found on {part_dev} in VG {vg} (top-level LVs: {names})"
                )));
            }
        },
        None => match lvs.len() {
            1 => lvs[0].clone(),
            0 => return Err(LvmError::Failed(format!("VG {vg}: no top-level LV uses {part_dev} — nothing to extend"))),
            _ => {
                let names = lvs.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>().join(", ");
                return Err(LvmError::Failed(format!(
                    "VG {vg}: multiple LVs on {part_dev} — pick one with --lv NAME (candidates: {names})"
                )));
            }
        },
    };
    let ext = crate::lvm::vg_extent_size(&vg)?;
    // 本次 resize 没有带来新空间（分区已是请求的终点）：没有可交给 LV 的增量。
    // 报错会把一次幂等重跑变成非零退出，而 pvresize 已经做完了它该做的部分
    if delta_bytes == 0 {
        println!("no new space was added by this resize — {path} left unchanged");
        return Ok(());
    }
    let n = delta_bytes / ext;
    if n == 0 {
        return Err(LvmError::Failed(format!(
            "added {delta_bytes} bytes < one VG extent ({ext} bytes) — LV left unchanged"
        )));
    }
    crate::lvm::lv_extend(&path, n)?;
    println!("lvextend -l +{n} -r {path} done (lv {name})");
    Ok(())
}