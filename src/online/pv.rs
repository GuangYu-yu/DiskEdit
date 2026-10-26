//! LVM PV 在线扩容：锁、场景校验与 pvresize。

use super::*;
use crate::dev::FileSource;

/// 从（盘名, 分区号）解析 OnlineTarget（PV 路径不来自挂载点，mnt 为 None）
pub(crate) fn resolve_pv_target(disk_name: &str, pno: u32) -> io::Result<OnlineTarget> {
    let disk_dev = PathBuf::from("/dev").join(disk_name);
    let sysroot = Path::new("/sys/block").join(disk_name);
    // 盘名与分区名都由 sysfs 目录名推得：节点必须与 sysfs 的设备号同源，
    // 否则后续 pvresize 的落点与这里读到的几何分属两台设备
    ensure_node_is_device(&disk_dev, &sysroot)?;
    let mut sysdir = None;
    for entry in fs::read_dir(&sysroot)? {
        let p = entry?.path();
        // 非目录条目（uevent/dev/size/stat 等 sysfs 属性文件）不可能是分区：对其拼
        // "partition" 会得到 ENOTDIR（os error 20），那是"这不是个目录"，与"分区属性
        // 读不出来"不同类（与 collect_neighbours 同一判据）
        if !p.is_dir() {
            continue;
        }
        // `exists()` 把任意 stat 错误折成 false，一次 EIO 会被报成"这块盘上没有该分区"，
        // 把人引去改分区号。只有 NotFound 才能当作"这一项不是分区"
        match fs::metadata(p.join("partition")) {
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e),
        }
        if sysfs_u64(&p.join("partition"))? as u32 == pno {
            sysdir = Some(p);
            break;
        }
    }
    let sysdir = sysdir
        .ok_or_else(|| io::Error::other(format!("partition {pno} not found under {}", sysroot.display())))?;
    let name = sysdir
        .file_name()
        .ok_or_else(|| io::Error::other("sysfs entry without basename"))?
        .to_string_lossy()
        .into_owned();
    let part_dev = PathBuf::from("/dev").join(&name);
    ensure_node_is_device(&part_dev, &sysdir)?;
    // 与 resolve_target 同一防线：sysfs 字节换算全部 checked（见那里的注释）
    let byte_of = |v: u64| v.checked_mul(512).ok_or_else(|| io::Error::other("sysfs sector value overflows u64 bytes"));
    let start_bytes = byte_of(sysfs_u64(&sysdir.join("start"))?)?;
    let part_len_bytes = byte_of(sysfs_u64(&sysdir.join("size"))?)?;
    let disk_cap_bytes = byte_of(sysfs_u64(&sysroot.join("size"))?)?;
    let logical_block = sysfs_u64(&sysroot.join("queue/logical_block_size"))?;
    if logical_block == 0 {
        return Err(io::Error::other("logical_block_size reported as 0 — sysfs value invalid"));
    }
    let this_dev = std::fs::read_to_string(sysdir.join("dev")).ok().and_then(|s| parse_dev_attr(&s));
    // 与 resolve_target 用同一判据排除目标自身；本函数的 sysdir 由上面同一目录遍历得出，
    // 故 dev 属性缺失时退回路径比较（那种比较在**这个**函数里是成立的）
    let others = collect_neighbours(&sysroot, |p| match this_dev {
        Some(d) => is_same_device(p, d),
        None => p == sysdir,
    })?;
    Ok(OnlineTarget {
        disk_dev,
        part_dev,
        mnt: None,
        pno,
        start_bytes,
        part_len_bytes,
        disk_cap_bytes,
        logical_block,
        others,
    })
}

/// 锁身份：锁按**取锁时刻**的拓扑派生（粒度取整盘，见 [`lock_disk`]）。路径串相同
/// 不代表拓扑相同——同一个 `/dev/sdX` 被重新绑定到另一块盘时，锁保护的是旧身份，
/// 而要动的是新设备。故取锁与锁下重解析之后各算一次，两处不一致即拒绝
pub(crate) fn lock_identity(t: &OnlineTarget) -> Result<crate::dev::TargetIdentity, Outcome> {
    // fail-closed：拓扑解析不出来即拒绝（不退到 devname-容量身份）
    crate::dev::TargetIdentity::resolve_block(&t.disk_dev).map_err(Outcome::infra)
}

/// 在线路径的独占所有权：设备已挂载 / 被 DM 持有，`O_EXCL` 不可能成功，但**同一把
/// TargetLock** 仍必须持有——否则在线写表与离线 mutation 可以互相穿插。
/// 粒度取**整盘**：分区表属于盘，且只有盘粒度才能与"以 `/dev/sdX` 为目标的离线
/// 调用"落进同一个锁名；锁是 advisory 的，它约束的是本工具的所有入口，不是别人。
/// 身份一并交回，供锁下重解析复核（见 [`lock_identity`]）
pub(crate) fn lock_disk(
    t: &OnlineTarget,
) -> Result<(crate::targetlock::TargetLock, crate::dev::TargetIdentity), Outcome> {
    let ident = lock_identity(t)?;
    // 取锁的失败按 `Fail` 的三个变体折叠成出口语义（唯一映射点在 outcome 模块），
    // 本处不借 `finish` 做转换——那是"结束一次操作"的入口，语义不同
    let lock = crate::targetlock::TargetLock::acquire(&ident, None).map_err(|f| f.into_outcome())?;
    Ok((lock, ident))
}

/// 锁下重解析出的目标是否仍落在取锁时的同一身份上。不符即拒绝：在 A 的锁下
/// 操作 B，两个写者会同时以为自己是独占的
pub(crate) fn verify_lock_identity(
    held: &crate::dev::TargetIdentity,
    fresh: &OnlineTarget,
) -> Result<(), Outcome> {
    match lock_identity(fresh) {
        Ok(id) if id.lock_path() == held.lock_path() => Ok(()),
        Ok(_) => Err(Outcome::infra(format!(
            "the device behind {} changed identity during locking — the lock protects a different target now; retry",
            fresh.disk_dev.display()
        ))),
        Err(o) => Err(o),
    }
}

/// 在线路径的现场闸口。本路径不建 journal（sfdisk / FS 工具用自己的 fd 写盘，
/// 不经过 `WriteSource::write_at`，挂 journal 只会得到空壳），因此**首次写盘之前**
/// 必须确认目标上没有任何未收尾的现场：有 ⇒ 拒绝（busy 文案与离线闸口同源）。
/// 代价也在此声明：本路径中断后没有 journal / checkpoint 可查，若 sfdisk 自身
/// 在写主/备 GPT 结构之间被撕裂，undo 与续传都覆盖不到，只能由外部工具修复。
/// 判定发生在锁下、与首次写盘之间无窗口；有了这道闸，命令成功后 main 的
/// drop_journal 只会碰到"不存在"的候选——绝无"删掉别人可回滚凭据"的可能
pub(crate) fn refuse_if_scene_active(t: &OnlineTarget) -> Result<(), Outcome> {
    // 只读打开**分区**节点：闸口只读现场（journal / checkpoint 候选），不申请写权限。
    // 身份按分区节点解析时，盘级键与旧版按分区命名的历史候选会被一并列出；用整盘身份
    // 会漏掉后者（self_key == disk_key 时那条历史候选不生成），而本闸口是唯一会检查
    // 它的入口——undo / abandon 要求整盘目标
    let part = crate::dev::FileSource::open_read_only(&t.part_dev)
        .map_err(|e| Outcome::infra(format!("open failed: {e}")))?;
    // 历史 GUID 命名的 checkpoint 只能从整盘的表读：分区节点的 0 偏移不是 GPT 头
    let disk = crate::dev::FileSource::open_read_only(&t.disk_dev)
        .map_err(|e| Outcome::infra(format!("open failed: {e}")))?;
    // 只传主语：后半句 "writes outside the undo journal" 由 refuse_if_active 统一拼
    //（与之并列的调用点传的是命令名，如 "resizefs" / "check"）。
    // 在线路径的两种入口共用本闸口：挂载中的块设备，与被 device-mapper 持有的
    // LVM PV——两者的续跑出路都是先退出在线形态（卸载 / 停用卷组）再走离线分支，
    // busy 文案里的"重跑续跑"只有在离线分支才真正可达，故在此附上提示
    crate::transaction::TransactionManager::refuse_if_active_at(
        &part,
        &disk,
        "online resize",
    )
    .map_err(|f| f.context("a re-run would take the online path again and cannot resume the job — unmount the target (for an LVM PV: deactivate its volume group) to reach the resume path"))
    .map_err(|f| f.into_outcome())
}

/// 新尺寸的几何前置检查：对齐 → 容量 → 邻接 → GPT 可用区上界，两条在线写表路径共用。
/// 缩容（仅 btrfs）也走这里：起点不变、区间只会更短，容量与邻接两条恒成立，
/// 需要它们的是 grow；对齐对两个方向都必要——sfdisk 会把未对齐尺寸静默截到扇区界。
/// 必须全部通过才允许 part_resize 落盘：写表之后这些条件已无法"事后退出"。
/// `disk` 是锁下的只读整盘句柄：可用区上界只能从**当下重读的表**取，sysfs 快照
/// 里没有这一项
pub(crate) fn check_new_range(t: &OnlineTarget, new_len_bytes: u64, disk: &FileSource) -> Result<(), Fail> {
    if new_len_bytes == 0 || !new_len_bytes.is_multiple_of(t.logical_block) {
        return Err(Fail::refused(format!(
            "size {new_len_bytes} not aligned to logical block size {}",
            t.logical_block
        )));
    }
    let end = t
        .start_bytes
        .checked_add(new_len_bytes)
        .ok_or_else(|| Fail::refused("size overflow".to_string()))?;
    if end > t.disk_cap_bytes {
        return Err(Fail::refused(format!(
            "new end {end} exceeds disk capacity {}",
            t.disk_cap_bytes
        )));
    }
    // MBR 的起点/长度字段各是 u32 LBA：离线路径在写表前按**长度**（size_lba）拒绝，
    // 在线把同一长度交给 sfdisk 的 size 字段，越过 u32 会在落盘时被截断成另一个区间
    let new_len_sectors = new_len_bytes / t.logical_block;
    if new_len_sectors > u32::MAX as u64
        && crate::table::parse_mbr(disk).map_err(Fail::from)?.is_some()
    {
        return Err(Fail::refused(format!(
            "new length {new_len_sectors} sectors exceeds the MBR size field (u32 sectors) — MBR cannot address it"
        )));
    }
    if !range_free(t.start_bytes, new_len_bytes, &t.others) {
        return Err(Fail::refused("new size would overlap an adjacent partition".to_string()));
    }
    // 盘尾的备份 GPT 与条目数组不在可用区内：落到那段字节上的表项会让备份头与
    // 分区内容互相覆盖。上界取表自述的 last_usable_lba，与离线入口同一判据；
    // 表中自身几何待修复（头副本不一致 / 保护 MBR 不自洽）时不给上界：哪个头的
    // last_usable_lba 才是本盘的正是修复要回答的问题，故拒绝
    if let Some(max_end) = crate::gpt_policy::usable_end_bytes(disk)?
        && end > max_end
    {
        return Err(Fail::refused(format!(
            "new end {end} exceeds the last usable block at {max_end} — the backup GPT and its entry array sit past it; \
             grow offline (`diskedit resize`) so the backup GPT is relocated first"
        )));
    }
    Ok(())
}

/// PV 分区在线扩容：只动分区层，pvresize/lvextend 归调用方。第二返回值是
/// 锁下取得的实际旧尺寸（前置失败时为 0，调用方只在 complete 分支消费）。
/// 第三返回值是本函数持有的独占锁：随后的 pvresize/lvextend 仍在同一次操作里，
/// 调用方须把它带到 LVM 链结束——在这里就释放会让那条链落在锁外
pub fn resize_pv(
    disk_name: &str,
    pno: u32,
    decide: impl FnOnce(&FileSource) -> Result<Option<u64>, Fail>,
) -> (Outcome, u64, Option<crate::targetlock::TargetLock>) {
    // 解析只读 sysfs / 路径，尚未写盘 → Infra。尚未取得独占权，旧尺寸不可信
    let t = match resolve_pv_target(disk_name, pno) {
        Ok(t) => t,
        Err(e) => return (Outcome::infra(e.to_string()), 0, None),
    };
    // 此后每一步都是写盘（写表 / 内核重读），独占权从解析出目标起就取得
    let (owned, locked_ident) = match lock_disk(&t) {
        Ok(v) => v,
        Err(o) => return (o, 0, None),
    };
    // 独占权在手后重取 sysfs 快照：resolve 必须先于取锁（锁身份取自它的结果），
    // 而外部工具不守本工具的锁——取锁耗时期间邻居/容量/本分区尺寸都可能已变。
    // 下面的 shrink 判定与 check_new_range 的邻接、容量判据必须来自锁下的新鲜事实
    let t = match resolve_pv_target(disk_name, pno) {
        Ok(t) => t,
        Err(e) => return (Outcome::infra(e.to_string()), 0, None),
    };
    // 身份复核：锁按取锁时刻的拓扑派生，窗口内同一路径被重绑到别的设备时，
    // 路径串比对看不出来，锁保护的已不是我们要动的这块盘
    if let Err(o) = verify_lock_identity(&locked_ident, &t) {
        return (o, 0, None);
    }
    let old_len_bytes = t.part_len_bytes;
    // 现场闸口：本路径不建 journal，目标上若有未收尾的现场必须在此拒绝
    if let Err(o) = refuse_if_scene_active(&t) {
        return (o, old_len_bytes, None);
    }
    // 活动 swap 的锁下复核（命令层那次是锁前粗查）：swapon 不守本工具的锁，
    // 窗口内被激活的 swap 若放行写表，改的就是活动 swap 的底层分区。
    // /proc/swaps 读不出来时无从断言"无 swap"——按环境故障拒绝而非放行
    match crate::online::swap_active(disk_name, pno) {
        Ok(true) => {
            return (Outcome::refused(format!("partition {pno} became active swap during locking — run swapoff first")), old_len_bytes, None);
        }
        Ok(false) => {}
        Err(e) => return (Outcome::infra(format!("cannot read /proc/swaps: {e}")), old_len_bytes, None),
    }
    // 锁下重取真相共用一个只读整盘句柄：PV 判据复核与"请求 → 目标长度"的解析
    // 都锚定独占权之后的盘上内容。PV 判据的复核（命令层那次是锁前粗查）：识别与
    // 取锁之间分区可被重新格式化，过期的 is_pv 会把 sfdisk 的表写入砸到已不是 PV 的分区上
    let disk = match FileSource::open_read_only(&t.disk_dev) {
        Ok(s) => s,
        Err(e) => return (Outcome::infra(format!("open failed: {e}")), old_len_bytes, None),
    };
    match crate::fsid::identify(&disk, t.start_bytes, t.part_len_bytes) {
        Ok(Some(crate::fsid::FsKind::Lvm2Pv)) => {}
        Ok(ft) => return (Outcome::refused(format!(
            "partition {pno} is no longer an LVM PV (identified as {}) — re-run `diskedit resize` to re-classify the target",
            crate::fsid::fs_name(ft)
        )), old_len_bytes, None),
        Err(e) => return (Outcome::infra(format!("identify failed: {e}")), old_len_bytes, None),
    }
    // 目标长度在锁下解析（见 `resize_online_planned` 的说明）；PV 路径无"仅扩 FS"一说，
    // `Ok(None)` 视作无绝对目标 = 分区层无变化
    let new_len_bytes = match decide(&disk) {
        Ok(Some(b)) => b,
        Ok(None) => t.part_len_bytes,
        Err(f) => return (f.into_outcome(), old_len_bytes, None),
    };
    // 相等值 = 分区层无事可做，按 complete 返回：调用方照常走完 PV 链，
    // delta 为 0 时 pvresize/lvextend 只同步现状。相等与否必须由锁下事实判定——
    // 锁前快照在窗口内可能已过期，用它短路会跳过本该做的写表
    if new_len_bytes == t.part_len_bytes {
        return (Outcome::applied_with(Vec::new()), old_len_bytes, Some(owned));
    }
    if new_len_bytes < t.part_len_bytes {
        return (Outcome::refused(format!(
            "PV partition can only grow here (current {} bytes); PV shrink needs the lvreduce/pvresize chain",
            t.part_len_bytes
        )), old_len_bytes, None);
    }
    if let Err(f) = check_new_range(&t, new_len_bytes, &disk) {
        return (f.into_outcome(), old_len_bytes, None);
    }
    if let Err(e) = part_resize(&t, new_len_bytes) {
        // 表已写、内核同步失败：PV 链（pvresize/lvextend）因此没做，两条事实一并报。
        // 只报"内核视图过期"会让 20 少说一半——分区已经大了，PV 还是旧尺寸
        let detail = e.detail().to_string();
        if matches!(&e, PartResizeError::Partial(_)) {
            return (
                e.into_outcome_with(vec![pv_chain_pending(pno, format!(
                    "partition resized but the PV chain was skipped: {detail}"
                ))]),
                old_len_bytes,
                None,
            );
        }
        return (e.into_outcome(), old_len_bytes, None);
    }
    (
        match verify_part_size(&t, new_len_bytes) {
            Ok(()) => Outcome::applied_with(Vec::new()),
            // 表已写、内核报的尺寸却不同 → 内核视图过期（后续 PV 链不得基于旧尺寸）
            Err(e) => {
                eprintln!("warning: partition table updated but kernel reports a different size: {e}");
                let mut o = Outcome::applied_with(vec![pv_chain_pending(pno, format!(
                    "partition resized but the PV chain was skipped: {e}"
                ))]);
                o.mark_kernel_stale();
                o
            }
        },
        old_len_bytes,
        // 表已写：后面的 PV 链仍属本次操作，锁随返回值继续持有
        Some(owned),
    )
}

/// PV 链未执行的后置条件：分区边界已经改了，`pvresize` / `lvextend` 还没跟上。
/// 种类名与 `cmd/resize/finish.rs` 的失败路径同名——同一件待办在两处出口叫两个名字，
/// 人会以为是两件事
fn pv_chain_pending(part: u32, detail: String) -> Pending {
    Pending::new(
        part,
        PendingKind::Other("lvm chain (pvresize/lvextend)"),
        detail,
        "re-run `diskedit resize` for this partition once the kernel partition view matches the table",
    )
}
