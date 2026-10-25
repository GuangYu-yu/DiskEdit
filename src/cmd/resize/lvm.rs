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
/// 容量折算成 extent 扩目标 LV（--lv 指定或该 PV 上唯一顶层 LV）。
/// lvextend 按容量扩，分配源由 LVM 决定，不限于本 PV。
/// 后置条件：pvresize 后 `pvs` 的 dev_size 必须覆盖整段分区。`pv_size` 是 PV 可用
/// 尺寸（extent 对齐与 PV 元数据使其不必等于设备尺寸），只随输出记录，不作为判据。
/// 增量是 0 时（中断重跑：分区已是新尺寸）不猜扩多少——见下面 `delta_bytes == 0` 的分支
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
        "pvs {part_dev}: dev_size={} pv_size={} pe_start={} pv_pe_count={} pv_pe_alloc_count={}",
        pv.dev_size, pv.pv_size, pv.pe_start, pv.pv_pe_count, pv.pv_pe_alloc_count
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
    let free = pv.pv_pe_count.saturating_sub(pv.pv_pe_alloc_count);
    // 本次运行的增量在中断重跑时是 0（分区已是新尺寸），而 PV 上剩下的未分配 extent
    // 并**不能**反推当初要扩多少——它们可能本来就被有意留着。两种猜法各错一半：按增量
    // 猜（0）会让 LV 永久少一截却报成功，按剩余量猜会把没被请求的空间并进 LV。缺的是
    // 一个盘上没有的事实，于是报出来由人决定，不替用户二选一
    if delta_bytes == 0 {
        if free == 0 {
            println!("{path} already covers what {part_dev} offers — nothing to extend");
            return Ok(());
        }
        return Err(LvmError::Failed(format!(
            "{part_dev} already carries its new size, so this run has no amount to add, while {free} extent(s) on it are unallocated — the amount an interrupted resize meant for {path} is not recorded on disk, and extending by whatever is free would also take space nobody asked for. If the LV is already the size you want, nothing is missing; otherwise extend it explicitly (lvextend -l +<N> -r {path}) and rerun to confirm"
        )));
    }
    // 增量是本次请求本身。想扩的量超出 PV 能给的量时不能只扩一半就报成功：
    // 那正是"表已经变了、LV 少一截、命令说完成"的形态，只是换了个触发条件
    let want = delta_bytes / ext;
    if want == 0 {
        return Err(LvmError::Failed(format!(
            "added {delta_bytes} bytes, less than one extent ({ext} bytes) — LV left unchanged"
        )));
    }
    if want > free {
        return Err(LvmError::Failed(format!(
            "added {delta_bytes} bytes = {want} extents of {ext} bytes, but {part_dev} has only {free} unallocated extent(s) — {path} would stay {} extent(s) short, so it was left unchanged",
            want - free
        )));
    }
    let n = want;
    crate::lvm::lv_extend(&path, n)?;
    println!("lvextend -l +{n} -r {path} done (lv {name})");
    Ok(())
}