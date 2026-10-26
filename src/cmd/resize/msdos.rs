//! MBR 目标的 grow 路径。

use super::*;

/// 已越过不可回滚点之后的失败：任何分类都不再能断言"本次未写盘"，
/// 出口语义一律压成 `Failed`（报告带"盘可能已改变"的警示）。
/// 越过点的两种形态：表项已写（grow 臂）与 FS 已缩（shrink 臂）
fn after_irreversible(f: Fail) -> Fail {
    match f {
        Fail::Refused(m) | Fail::Infra(m) => Fail::failed(m),
        Fail::Failed(m) => Fail::Failed(m),
    }
}

/// MBR 分区被扩到更大之后的收尾：FS 步（swap 重建 / FS 扩容）→ 内核重读 → 统一收尾。
/// grow-to-end 与显式 SIZE 扩容的后置条件完全相同，故两条路径共用本函数——
/// FS 步是后置条件的一部分，缺了会"分区变大、文件系统没变大"却报成功。
/// 调用点均已把新表项写盘，故表写入不再可回滚：外部 FS 工具一旦写完，回滚表项
/// 就会留下"表小于内容"的自相矛盾（与 GPT 侧 finalize_growth 同一不变量）。
/// p 是**扩容前**解析出的条目（swap 头部探测用它原区间，原尺寸是 LVM 位移的基线）
pub(crate) fn mbr_grow_finish(
    a: &Args,
    src: &mut WriteSource,
    p: &table::MbrPartition,
    is_pv: bool,
    is_block: bool,
) -> crate::outcome::Outcome {
    let part = p.num;
    let ss = src.sector_size;
    let cur_bytes = lba_bytes(p.size_lba as u64, ss);
    let mut pending: Vec<crate::outcome::Pending> = Vec::new();
    // --no-fs：分区层之外的后置条件整体出局，与 GPT 路径同语义。
    // FS 步之前重读内核分区表：resize2fs/xfs_growfs/mkswap 都以 /dev 节点的内核尺寸为准，
    // 而离线块设备写表后内核仍持旧长度（不会自动重读），按旧尺寸扩容会空跑并以 0 退出。
    // 重读失败时没有可信的 FS 尺寸，改记待办交给出口（与 GPT 侧 finalize_growth 同一判据）
    if !a.no_fs && kernel_resync(src) {
        // 区域取自**当前**表（本轮可能已写表）：起点与长度都以盘上现状为准。
        // 此处已越过写盘，判定失败不再有"本次未写盘"的出口语义，故 Refused 按 Failed 如实报
        let (base, len) = crate::gpt_policy::partition_bytes(src, part)
            .map_err(after_irreversible)
            .unwrap_or_else(|f| bail_fail(f));
        match fsops::grow_target_at(src, part, base, len) {
            // 走到这里的是写盘前已放行、没有可扩 FS 的那几种：首启 overlay、PV，以及页格式
            // 在本机激活不了的 swap——`unknown` 已由 check_grow_step 拦下，本支至多因盘在
            // 两步之间被改动而见到它。
            // 仍要探一遍"元数据是 swap 却激活不了"那类真实待办——探测用扩容前的区间：
            // swap 签名恒在分区首 32K 内，起点未变，原长度足够容纳
            Ok(fsops::Growable::OverlayPending | fsops::Growable::SwapUnactivatable | fsops::Growable::NoFilesystem(_)) => {
                match movepart::swap_rebuild_pending(
                    src, part, lba_bytes(p.start_lba as u64, ss), lba_bytes(p.size_lba as u64, ss),
                ) {
                    Ok(Some(missed)) => pending.push(missed),
                    Ok(None) => {}
                    // 探测读失败属设备故障，且此刻表多半已写：Failed 的"盘可能已改变"
                    // 警示比伪造一条 Pending 如实
                    Err(e) => bail_fail(Fail::from(e)),
                }
            }
            // swap：内容可弃，表项已扩 → mkswap 重建使新空间生效（UUID/卷标保持；
            // 离线路径仅镜像，块设备走在线路径且 active swap 已被守卫拒绝）
            Ok(fsops::Growable::Target(t)) if t.fstype == fsid::FsKind::Swap => {
                // swap 的承诺就是 mkswap：外部工具写盘不可回滚，先落屏障
                src.set_mutation(crate::dev::Mutation::ExternalFsTool);
                src.mark_non_reversible().unwrap_or_else(|e| bail_fail(Fail::from(e)));
                let ident = movepart::read_swap_identity(src, p.start_lba as u64, p.size_lba as u64, ss);
                // 与 movepart 的两个重建点同一口径：保不住原 UUID 必须说给用户听，
                // 否则 fstab / resume 的 UUID= 会静默失效（旧签名本就没有 UUID，或读不出来）
                let had_uuid = ident.0.is_some();
                match fsops::recreate_swap(src, part, ident) {
                    Ok(()) if had_uuid => println!("swap recreated (UUID preserved)"),
                    Ok(()) => println!("swap recreated (no UUID could be read from the old signature — mkswap wrote a fresh one)"),
                    Err(e) => pending.push(crate::outcome::Pending::new(
                        part,
                        crate::outcome::PendingKind::Swap,
                        e.to_string(),
                        fsops::rescue_hint(Some(fsid::FsKind::Swap), &dev::part_dev_hint(src, part, lba_bytes(p.start_lba as u64, ss))),
                    )),
                }
            }
            Ok(fsops::Growable::Target(t)) => {
                // 外部 FS 工具（resize2fs/xfs_growfs/…）写盘不可回滚：FS 自述尺寸随即大于
                // 旧分区尺寸，此后回滚表项即"表与内容自相矛盾"，先落屏障
                src.set_mutation(crate::dev::Mutation::ExternalFsTool);
                src.mark_non_reversible().unwrap_or_else(|e| bail_fail(Fail::from(e)));
                match t.resize_fs(src) {
                    // 扩容已成功，卸载的成败是另一条后置条件：它不改写成功这一事实，
                    // 只作为一条待办随结果浮出
                    Ok(Some(leak)) => pending.push(leak.into_pending(part)),
                    Ok(None) => {}
                    Err(e) => pending.push(crate::outcome::Pending::new(
                        part,
                        crate::outcome::PendingKind::Fs,
                        e.to_string(),
                        fsops::rescue_hint(Some(t.fstype), &dev::part_dev_hint(src, part, lba_bytes(p.start_lba as u64, ss))),
                    )),
                }
            }
            // 判定失败：做不了的后置条件 ⇒ PARTIAL；设备读不出来 ⇒ 报失败
            Err(e) => match Fail::from(e) {
                Fail::Refused(why) => pending.push(crate::outcome::Pending::new(
                    part, crate::outcome::PendingKind::Fs, why, String::new(),
                )),
                other => bail_fail(after_irreversible(other)),
            },
        }
    } else if !a.no_fs {
        pending.push(crate::outcome::Pending::new(
            part,
            crate::outcome::PendingKind::Fs,
            "the kernel still reports the old partition size, so the filesystem was not grown — refresh the kernel view (partprobe / partx) and re-run".to_string(),
            String::new(),
        ));
    }
    if pending.is_empty() {
        // 表已写 → 统一收尾（内核重读 + 报告）
        let o = settle_layout(crate::outcome::Outcome::applied_with(Vec::new()), src);
        return finish_resize(a, o, Some(src), part, is_pv, is_block, cur_bytes);
    }
    let mut o = crate::outcome::Outcome::applied_with(pending);
    if !kernel_resync(src) {
        o.mark_kernel_stale();
    }
    o.report();
    // PV 的 pvresize/lvextend 链是本次操作的后置条件之一，不因 swap 等外层待办而跳过：
    // finish_resize 只在非 PV 的未完成结果上早退，PV 则照常进链，并以两者中更严重的
    // 退出码收口。链自身的失败在 resize_done 内另报，与上面的待办清单不是同一条
    if is_pv {
        return finish_resize(a, o, Some(src), part, is_pv, is_block, cur_bytes);
    }
    o
}

/// resize 的 MBR 分支（仅主分区 1..4；逻辑分区与扩展容器不支持）。原位纯扩缩：扩须右侧
/// 空闲足够（MBR 无搬移能力），缩走与 GPT 相同的"FS 先缩 → 写表"守卫链。块设备复用在线
/// 路径（基于 sysfs + sfdisk，与表类型无关）
pub(crate) fn cmd_resize_msdos(a: &Args, pref: PartSelector, req: &SizeRequest, src_ro: &FileSource) -> crate::outcome::Outcome {
    // MBR resize 没有搬移能力：--allow-move 承诺的"搬开挡路分区"不存在，
    // 静默按不可搬移处理会让来自脚本的调用只看到一条"空间不足"
    if a.allow_move {
        bail_fail(Fail::refused("--allow-move is not supported for MBR resize — MBR cannot relocate blocking partitions".to_string()));
    }
    // 离线路径不消费 chunk：在线检查（resize_online）只在块设备上执行，镜像文件
    // 与非 Linux 平台会绕过它落到这里，取值校验也随之跳过
    if a.seen.contains(&"--chunk-size") {
        bail_fail(Fail::refused(
            "--chunk-size has no effect for MBR resize — MBR cannot relocate blocking partitions".to_string(),
        ));
    }
    let mbr = table::parse_mbr(src_ro)
        .unwrap_or_else(|e| bail_fail(Fail::infra(format!("parse failed: {e}"))))
        .unwrap_or_else(|| bail_fail(Fail::refused("no MBR on target".to_string())));
    // 只读快照
    let part = crate::gpt_policy::resolve_part_in(crate::gpt_policy::TableEntries::Mbr(&mbr), pref)
        .unwrap_or_else(|f| bail_fail(f));
    let p = match mbr.iter().find(|p| p.num == part) {
        Some(p) => p,
        None => bail_fail(Fail::refused(format!("partition {part} not found (MBR resize covers primary partitions 1..4 only)"))),
    };
    if p.is_container {
        bail_fail(Fail::refused("extended partition container cannot be resized (logical partitions are out of scope)".to_string()));
    }
    let ss = src_ro.sector_size;
    let cur_bytes = lba_bytes(p.size_lba as u64, ss);
    let fstype = fsid::identify(src_ro, lba_bytes(p.start_lba as u64, ss), lba_bytes(p.size_lba as u64, ss))
        .unwrap_or_else(|e| bail_fail(Fail::infra(format!("identify failed: {e}"))));
    let is_pv = fstype == Some(fsid::FsKind::Lvm2Pv);

    let (target, grow_to_end) = resolve_size_request(req, cur_bytes).unwrap_or_else(|f| bail_fail(f));
    let shrinking = target.is_some_and(|t| t < cur_bytes);

    check_pv_intent(part, fstype, is_pv, shrinking, a.grow_lv, a.no_fs).unwrap_or_else(|f| bail_fail(f));
    let is_block = src_ro.is_block;

    // 块设备在线路径：与 GPT 同一份实现，随表类型变的只有右侧空闲数。目标长度同样
    // 交给 mbr_decider 在在线自己的锁下重解析（理由见 gpt_decider）
    #[cfg(target_os = "linux")]
    if let Some(o) = resize_online(a, src_ro, part, is_pv, mbr_decider(req, part, is_pv)) {
        return o;
    }

    // 离线路径
    let mut src = open_target_for_write(a).unwrap_or_else(|f| bail_fail(f));
    // 锁下重取权威几何（与 GPT 分支同一原则）：只读阶段与取得独占权之间盘可以被别人
    // 改写（sfdisk/fdisk 不守本工具的锁），用锁前的 mbr/p/free 驱动写分支就是把过期
    // 决定写进盘——resize_mdos_entry 不复核邻接，过期的 free 没有第二道防线
    let mbr = table::parse_mbr(&src)
        .unwrap_or_else(|e| bail_fail(Fail::infra(format!("parse failed: {e}"))))
        .unwrap_or_else(|| bail_fail(Fail::refused("no MBR on target".to_string())));
    // 锁下快照
    let part = crate::gpt_policy::resolve_part_in(crate::gpt_policy::TableEntries::Mbr(&mbr), pref)
        .unwrap_or_else(|f| bail_fail(f));
    let p = match mbr.iter().find(|p| p.num == part) {
        Some(p) => p,
        None => bail_fail(Fail::refused(format!("partition {part} not found (MBR resize covers primary partitions 1..4 only)"))),
    };
    if p.is_container {
        bail_fail(Fail::refused("extended partition container cannot be resized (logical partitions are out of scope)".to_string()));
    }
    // FS 类型按锁下现状重取：识别与取锁之间分区可被重新格式化（mkfs 不守本工具的锁），
    // 拿旧类型选工具就是把 ext4 的工具链砸到 xfs 上；PV 判据随之重算
    let fstype = fsid::identify(&src, lba_bytes(p.start_lba as u64, ss), lba_bytes(p.size_lba as u64, ss))
        .unwrap_or_else(|e| bail_fail(Fail::infra(format!("identify failed: {e}"))));
    let is_pv = fstype == Some(fsid::FsKind::Lvm2Pv);
    // SIZE 锚点按锁下的新分区尺寸重算；grow 标记是请求的形状，锁前锁后同值（见 GPT 分支）
    let cur_bytes = lba_bytes(p.size_lba as u64, ss);
    let (target, _) = resolve_size_request(req, cur_bytes).unwrap_or_else(|f| bail_fail(f));
    let shrinking = target.is_some_and(|t| t < cur_bytes);
    check_pv_intent(part, fstype, is_pv, shrinking, a.grow_lv, a.no_fs).unwrap_or_else(|f| bail_fail(f));
    // PV 链的工具必须在写表之前齐备（与 GPT 分支同一处、同一判据）：pvresize/pvs 及
    // grow-lv 的 lvs/vgs/lvextend 只在表提交之后启动，缺工具那时才发现就只剩 PARTIAL
    #[cfg(target_os = "linux")]
    if is_pv {
        preflight_lvm_tools(a.grow_lv, !is_block).unwrap_or_else(|e| bail_fail(Fail::from(e)));
    }
    // 占用复核在锁下（离线选择时的探测在锁前）：首次落盘前确认分区仍空闲，与 GPT 分支闸同源。
    // 起点不变、末端不变或只增的请求不挪动上层 DM 栈映射的扇区，按 Extend 放行；缩容要求无持有者
    let edit = if grow_to_end || target.is_some_and(|b| b >= cur_bytes) {
        crate::fsops::WriteKind::Extend
    } else {
        crate::fsops::WriteKind::Overwrite
    };
    crate::fsops::ensure_idle_before_write(&src, part, lba_bytes(p.start_lba as u64, ss), edit).unwrap_or_else(|f| bail_fail(f));
    let total_sectors = src.size / ss;
    // 后置条件含 FS 调整：请求在扩（`grow` 即便右侧无空闲，也仍要把 FS 扩满现分区，见下面
    // free == 0 那支）就必须在写表之前问出"里面是什么、那一步做得了吗"——判据与 GPT 路径
    // 同一处。缩容有下面的 check_shrink 守卫链，no-op 两不动，故都不在此列
    let extends = grow_to_end || target.is_some_and(|b| b / ss > p.size_lba as u64);
    if extends && !a.no_fs {
        fsops::grow_target_at(&src, part, lba_bytes(p.start_lba as u64, ss), lba_bytes(p.size_lba as u64, ss))
            .and_then(|g| fsops::check_grow_step(g, is_block))
            .unwrap_or_else(|e| bail_fail(Fail::from(e)));
    }
    if grow_to_end || target.is_some_and(|b| b > cur_bytes) {
        // MBR 没有 checkpoint：中断之后盘上已经是"部分执行"的现场，重跑会从现状再涨一次，
        // 所以这里不能给一条"重跑即续跑"的暗示
        eprintln!("note: MBR has no checkpoint — if this grow is interrupted, inspect the partition table and filesystem size before re-running");
    }
    if grow_to_end {
        let free = free_right_msdos(&mbr, p, total_sectors);
        // 请求的分区增长无法实现：与 GPT 的 `resize grow` 及在线路径同语义拒绝，而不是
        // 只把 FS 扩满现分区再以 0 退出——那会让调用方以为分区表也已更新
        if free == 0 {
            bail_fail(Fail::refused(
                "grow would not extend the partition — no free space to its right (MBR cannot relocate blocking partitions); \
                 use `resizefs` to fill the filesystem inside the current partition".to_string(),
            ));
        }
        let new_size_lba = p.size_lba as u64 + free;
        if new_size_lba > u32::MAX as u64 {
            bail_fail(Fail::refused(format!("new size {new_size_lba} sectors exceeds MBR 32-bit LBA limit")));
        }
        table::resize_mdos_entry(&mut src, part, new_size_lba as u32)
            .unwrap_or_else(|f| bail_fail(f));
        crate::movepart::fault_mbr_after_table_write();
        mbr_grow_finish(a, &mut src, p, is_pv, is_block)
    } else {
        let Some(bytes) = target else { crate::args::usage() };
        if bytes < ss {
            bail_fail(Fail::refused(format!("size {bytes} < one sector ({ss})")));
        }
        let new_size_lba = bytes / ss;
        if new_size_lba > u32::MAX as u64 {
            bail_fail(Fail::refused(format!("size {new_size_lba} sectors exceeds MBR 32-bit LBA limit")));
        }
        // 越界与右侧空闲都已判定，note 不再跟在一条拒绝后面
        if new_size_lba == p.size_lba as u64 {
            // 请求的终点就是现状：表与 FS 都不动（GPT 路径对未变尺寸同样判 FsAction::Leave）。
            // 不跑 FS 步，请求"什么都不改"就不会因外部工具缺失而拿到非零退出码
            crate::support::note_sector_truncation("resizing to", bytes, ss);
            println!("partition {part} is already {cur_bytes} bytes — nothing to do");
            // 表与 FS 都没动：事务已完成且没有待办
            return crate::outcome::Outcome::applied_with(Vec::new());
        }
        if new_size_lba > p.size_lba as u64 {
            let want = new_size_lba - p.size_lba as u64;
            if want > free_right_msdos(&mbr, p, total_sectors) {
                bail_fail(Fail::refused("not enough contiguous free space to the right (MBR resize cannot relocate blocking partitions)".to_string()));
            }
            crate::support::note_sector_truncation("resizing to", bytes, ss);
            table::resize_mdos_entry(&mut src, part, new_size_lba as u32)
                .unwrap_or_else(|f| bail_fail(after_irreversible(f)));
            // 分区层做完 → 与 grow-to-end 同一条收尾
            mbr_grow_finish(a, &mut src, p, is_pv, is_block)
        } else {
            // 缩：与 movepart GPT 路径同守卫链——FS 先缩成功才写表。
            // --no-fs 与缩容不可共存（分区末端会切进未缩的 FS 元数据），与 GPT 同判据
            if a.no_fs {
                bail_fail(Fail::refused("--no-fs cannot shrink: the filesystem has to be shrunk first, otherwise the new partition end would cut into filesystem metadata".to_string()));
            }
            // FS 收缩的前置检查与 GPT 路径同一处（fsops::check_shrink）：能否缩 / 类型是否
            // 认得 / 工具是否齐备只写一份
            fsops::check_shrink(fstype, is_block, fsops::needs_loop(&fsops::DeviceScope::Partition(part), is_block))
                .unwrap_or_else(|e| bail_fail(Fail::from(e)));
            if let Some(min) = fsops::fs_min_bytes(&src, part, fstype)
                .unwrap_or_else(|e| bail_fail(Fail::from(e).context("min-size probe failed")))
                && bytes < min
            {
                bail_fail(Fail::refused(format!("target size {bytes} < minimum FS size {min} bytes (resize2fs -P)")));
            }
            // 缩容预检（方向、可缩性、工具齐备、下界）全过，note 不再跟在一条拒绝后面
            crate::support::note_sector_truncation("resizing to", bytes, ss);
            // 与 grow 臂同判据：外部 FS 工具写盘不可回滚，undo journal 从此只能如实记屏障
            src.set_mutation(crate::dev::Mutation::ExternalFsTool);
            src.mark_non_reversible().unwrap_or_else(|e| bail_fail(Fail::from(e)));
            // shrink_fs 的契约按旧分区尺寸比对（FS 小于分区时实为良性扩容，见其文档）；
            // 此处传入的是新分区尺寸，≤ 旧分区尺寸由缩容方向（new_size_lba < p.size_lba）保证
            let requested = new_size_lba * ss;
            let shrunk = fsops::shrink_fs(&src, part, fstype, requested)
                .unwrap_or_else(|e| bail_fail(Fail::from(e).context("FS shrink failed")));
            // 成功行同时给出实际落点与请求值：工具按自身粒度取整，差异要看得见
            if let Some(line) = shrunk.verify.ok_line(requested) {
                println!("{line}");
            }
            table::resize_mdos_entry(&mut src, part, new_size_lba as u32)
                .map_err(after_irreversible)
                .unwrap_or_else(|f| bail_fail(f));
            // FS 已缩之后浮出的两条待办：回读不出来（缩容可能已完成、落点未证实）与卸载
            // 没做成（清理未完成）——都不改写缩容这一事实，只作为待办浮出
            let mut pending: Vec<_> = shrunk.verify.pending(part).into_iter().collect();
            if let Some(leak) = shrunk.leak {
                pending.push(leak.into_pending(part));
            }
            let o = settle_layout(crate::outcome::Outcome::applied_with(pending), &src);
            finish_resize(a, o, Some(&mut src), part, is_pv, is_block, cur_bytes)
        }
    }
}