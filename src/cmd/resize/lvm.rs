//! PV 扩容后的 LV 链式扩容。

#[cfg(target_os = "linux")]
use super::*;
#[cfg(target_os = "linux")]
use crate::movepart::{LvmPostFacts, LvmSelection};

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

/// 只读定位 `part_dev` 上的目标顶层 LV：`--lv` 精确匹配裸名或 `/dev/<vg>/<name>`，
/// 缺省要求该 PV 上只有一个顶层 LV。返回选择时的名字、路径、VG extent 字节数与
/// LV 当前字节数——后者是 `lvextend -l +N` 的基线，随 checkpoint 落盘。
/// 全部是只读查询、与 pvresize 的结果无关，因此由调用方在写盘前先做。
/// "PV 不在任何 VG / 没有 LV / LV 不唯一 / 名字对不上"都是请求与卷现状不匹配
/// （`LvmError::Refused`），写盘之前拒绝即可让人改参数重跑
#[cfg(target_os = "linux")]
fn select_lv(part_dev: &str, want_lv: Option<&str>) -> Result<LvmSelection, LvmError> {
    let vg = match crate::lvm::vg_of(part_dev) {
        Ok(Some(vg)) => vg,
        Ok(None) => return Err(LvmError::Refused(format!("{part_dev} is a PV outside any VG — nothing to extend"))),
        Err(e) => return Err(e),
    };
    let lvs = crate::lvm::lvs_on_pv(&vg, part_dev)?;
    let (name, path) = match want_lv {
        Some(sel) => match lvs.iter().find(|(n, p)| n == sel || p == &format!("/dev/{sel}")) {
            Some(x) => x.clone(),
            None => {
                let names = lvs.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>().join(", ");
                return Err(LvmError::Refused(format!(
                    "LV {sel:?} not found on {part_dev} in VG {vg} (top-level LVs: {names})"
                )));
            }
        },
        None => match lvs.len() {
            1 => lvs[0].clone(),
            0 => return Err(LvmError::Refused(format!("VG {vg}: no top-level LV uses {part_dev} — nothing to extend"))),
            _ => {
                let names = lvs.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>().join(", ");
                return Err(LvmError::Refused(format!(
                    "VG {vg}: multiple LVs on {part_dev} — pick one with --lv NAME (candidates: {names})"
                )));
            }
        },
    };
    let extent_bytes = crate::lvm::vg_extent_size(&vg)?;
    // 当前尺寸同时复核 LV 仍在该 VG 且仍由这块 PV 承载（身份判据与收尾链同一实现）
    let state = crate::lvm::lv_state(part_dev, &path)?;
    // `lvextend -r` 要有活动 LV 才能同步扩文件系统。激活状态属于系统运行态：本工具
    // 不代为激活，也不在事后恢复，因此在写表之前就拒绝，而不是提交之后再失败
    if !state.active {
        return Err(LvmError::Inactive(format!(
            "LV {path} is not active — --grow-lv extends an active LV only (lvextend -r resizes the live filesystem)"
        )));
    }
    Ok(LvmSelection { name, path, extent_bytes, size: state.size })
}

/// 首次写表之前的 LV 选择预检（只读）：`--lv` 写错、PV 上没有 LV、LV 不唯一都要在
/// 分区变大之前拒绝。提交之后再发现就只剩"表已改、pvresize 可能已跑"的现场，用户
/// 唯一的出路是手工收尾。选择结果随行到执行半程，导出 checkpoint 里的 LVM 事实；
/// 收尾链在提交之后按那份事实续做（见 [`lvm_grow_chain`]），两处共用 [`select_lv`]
/// 这一实现
#[cfg(target_os = "linux")]
pub(crate) fn preflight_lv_selection(
    src: &crate::dev::FileSource,
    part: u32,
    is_block: bool,
    start_bytes: u64,
    want_lv: Option<&str>,
) -> Result<LvmSelection, LvmError> {
    // 分区节点按已打开句柄派生（块设备走 sysfs、镜像经 loop 临时映射），不按命令行
    // 字符串再拼一次——收尾链取节点用的是同一实现
    if is_block {
        let node = crate::fsops::find_block_partition_node(src, part, start_bytes)?;
        return select_lv(&node, want_lv);
    }
    crate::fsops::with_partition_device_for_lvm(src, part, |dev| select_lv(dev, want_lv))
}

/// 写表之前的 LVM 预检错误 → 出口语义。此刻一次目标写盘都没有发生，三种成因因此
/// 都能走"确定未写盘"的通道：工具缺失与非零退出的只读查询归环境故障（`Infra`，
/// 改参数无解），"请求与卷现状对不上"归拒绝（`Refused`，改 `--lv` 有解）。
/// 提交之后的链失败不走这里——那时已越过 durable boundary，只能作为待办上报
#[cfg(target_os = "linux")]
pub(crate) fn lvm_preflight_fail(e: LvmError) -> Fail {
    match e {
        LvmError::ToolMissing(m) | LvmError::Failed(m) => Fail::infra(m),
        LvmError::Refused(m) | LvmError::Inactive(m) => Fail::refused(m),
    }
}

/// 分区扩容成功后的 LVM 链（仅块设备）：pvresize 吸收全部新增空间；`--grow-lv` 把本次新增
/// 容量折算成 extent 扩目标 LV（`--lv` 指定或该 PV 上唯一顶层 LV）。
/// lvextend 按容量扩，分配源由 LVM 决定，不限于本 PV。
/// 后置条件：pvresize 后 `pvs` 的 dev_size 必须覆盖整段分区。`pv_size` 是 PV 可用
/// 尺寸（extent 对齐与 PV 元数据使其不必等于设备尺寸），只随输出记录，不作为判据。
///
/// `facts` 是 checkpoint 里记下的收尾事实（表已提交、链未完成时的现场）：续跑按它
/// 判定该不该扩、扩到哪——`lv_size_before` / `lv_target` 是同一次作业的基线，盘上没有
/// 第二份。没有现场事实时（在线路径无 checkpoint、或分区层本来就没有变化）才按本次
/// 增量折算
#[cfg(target_os = "linux")]
pub(crate) fn lvm_grow_chain(
    part_dev: &str,
    expected_bytes: u64,
    delta_bytes: u64,
    grow_lv: bool,
    want_lv: Option<&str>,
    facts: Option<&LvmPostFacts>,
) -> Result<(), LvmError> {
    // pv_resize 只拿到路径：节点必须与 sysfs 记录的设备号同源，否则 PV 标签与元数据区
    // 会写到另一台设备上。判据与 find_block_partition_node 共用同一实现
    let node = std::path::Path::new(part_dev);
    let sysdir = crate::dev::sysfs_node(node).map_err(|e| {
        LvmError::Failed(format!("cannot resolve the sysfs node of {part_dev} for the LVM chain: {e}"))
    })?;
    crate::dev::ensure_node_is_device(node, &sysdir)
        .map_err(|e| LvmError::Failed(format!("refusing the LVM chain on {part_dev}: {e}")))?;
    // 没有现场事实时的 LV 选择是只读判定，先于 pvresize：名字写错、该 PV 上没有 LV 或
    // LV 不唯一都在 pvresize 生效之前拒绝。有事实的续跑不重选——记录里的 LV 才是作业目标
    let fallback = match (facts, grow_lv) {
        (None, true) => Some(select_lv(part_dev, want_lv)?),
        _ => None,
    };
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
    if let Some(f) = facts {
        return match &f.lv_path {
            // 只做 pvresize 的作业：PV 的新空间已吸收，没有 LV 级的量
            None => Ok(()),
            Some(path) => {
                let free = pv.pv_pe_count.saturating_sub(pv.pv_pe_alloc_count);
                finish_lv_growth(part_dev, path, f, free)
            }
        };
    }
    // 走到这里说明本次运行有可折算的增量（在线路径无 checkpoint，或分区层刚提交）
    let Some(sel) = fallback else {
        return Ok(());
    };
    let (path, ext, current) = (sel.path, sel.extent_bytes, sel.size);
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
    crate::lvm::lv_extend(&path, want)?;
    println!("lvextend -l +{want} -r {path} done (lv {})", sel.name);
    // 目标按记录的增量算出，回读确认它真的到了那一尺寸：lvextend 的退出码 0 不等于
    // LV 尺寸变了（LVM 对空操作也可能成功返回）
    let after = crate::lvm::lv_state(part_dev, &path)?;
    let target = current + want * ext;
    if after.size < target {
        return Err(LvmError::Failed(format!(
            "lvextend -l +{want} reported success, but {path} is {} bytes (expected at least {target})",
            after.size
        )));
    }
    Ok(())
}

/// 按 checkpoint 记录的事实续做 LV 收尾：先核对 LV 身份、VG extent 与当前尺寸，
/// 只有"恰好停在基线"才扩到记录的目标；已在目标或已过目标都视为收尾完成。
/// 介于基线与目标之间是既无法解释也无法安全补做的落点——拒绝而不是再补一段
#[cfg(target_os = "linux")]
fn finish_lv_growth(part_dev: &str, lv_path: &str, f: &LvmPostFacts, free: u64) -> Result<(), LvmError> {
    // `lv_state` 同时核对 LV 仍在该 VG、仍由这块 PV 承载（路径字符串不是授权依据）
    let st = crate::lvm::lv_state(part_dev, lv_path)?;
    let ext = crate::lvm::vg_extent_size(&st.vg)?;
    if ext != f.extent_bytes {
        return Err(LvmError::Failed(format!(
            "VG {} reports an extent size of {ext} bytes, but the checkpoint recorded {} — the VG geometry changed since the job started; refusing to extend {lv_path}",
            st.vg, f.extent_bytes
        )));
    }
    if st.size >= f.lv_target {
        println!("{lv_path} is already {} bytes (recorded target {}) — lvextend skipped", st.size, f.lv_target);
        return Ok(());
    }
    if st.size != f.lv_size_before {
        return Err(LvmError::Failed(format!(
            "{lv_path} is {} bytes, neither the recorded baseline {} nor the recorded target {} — refusing to guess how much to add",
            st.size, f.lv_size_before, f.lv_target
        )));
    }
    let want = (f.lv_target - f.lv_size_before) / f.extent_bytes;
    if want > free {
        return Err(LvmError::Failed(format!(
            "{want} extent(s) are needed to reach the recorded target, but {part_dev} has only {free} unallocated extent(s) — {lv_path} was left unchanged"
        )));
    }
    crate::lvm::lv_extend(lv_path, want)?;
    let after = crate::lvm::lv_state(part_dev, lv_path)?;
    if after.size < f.lv_target {
        return Err(LvmError::Failed(format!(
            "lvextend -l +{want} reported success, but {lv_path} is {} bytes (recorded target {})",
            after.size, f.lv_target
        )));
    }
    println!("lvextend -l +{want} -r {lv_path} done (reached the recorded target {})", f.lv_target);
    Ok(())
}
