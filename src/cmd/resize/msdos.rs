//! MBR 目标的 grow 路径。

use super::*;

/// 已越过不可回滚点之后的失败：任何分类都无法断言"本次未写盘"，
/// 出口语义一律压成 `Partial`（报告带"盘可能已改变"的警示）。
fn after_irreversible(f: RunOutcome) -> RunOutcome {
    match f {
        RunOutcome::Refused(m) | RunOutcome::Internal(m) | RunOutcome::Usage(m) => RunOutcome::partial(m),
        RunOutcome::NoMutation { cause } | RunOutcome::Unsupported { cause } => RunOutcome::partial(cause),
        other @ RunOutcome::Partial { .. } | other @ RunOutcome::Applied { .. } => other,
    }
}

/// 分区层提交前的区间：表写入之后盘上只剩新区间，续跑靠它判定"分区层是否已提交"。
/// 与 GPT 计划/应用路径（movepart::shift 的 bind_old_range）同一语义，写表之前落盘；
/// 已有值不因续跑时的盘上几何改写（见 RecordJournal::bind_old_range）
fn record_old_range(src: &mut WriteSource, p: &table::MbrPartition) -> Result<(), crate::outcome::RunOutcome> {
    let Some(journal) = src.record_journal_mut() else { return Ok(()) };
    let start = u64::from(p.start_lba);
    let end = start + u64::from(p.size_lba) - 1;
    journal
        .bind_old_range((start, end))
        .map_err(|e| RunOutcome::no_mutation(e.record_failure("the pre-commit range")))
}

/// MBR 表项改长度：一次读定快照 → 纯计算新 LBA0 → 装配成表步骤 → 执行器落盘。
///
/// 与 `delete`/`add` 同一条链：命令层不自己调 `write_at`，写前按三扇区复核、
/// 写后读回、逐段落盘都由 `execution::table` 承担。`kind` 只记录方向，不影响写入字节。
fn resize_mbr_entry(
    src: &mut WriteSource,
    part: u32,
    new_size_lba: u32,
) -> Result<(), RunOutcome> {
    table::require_writable_mbr(src)?;
    let regions = table::mbr_compare_regions(src)?;
    let snapshot = table::table_snapshot(src, &regions)?;
    let Some(lba0) = snapshot.bytes.first() else {
        return Err(RunOutcome::internal("the table snapshot is empty — LBA0 is missing"));
    };
    let total = src.size / src.sector_size;
    let kind = match mbr_entry_size(lba0, part) {
        Some(old) if new_size_lba >= old => table::TableWriteKind::Grow,
        Some(_) => table::TableWriteKind::Shrink,
        // 槽位判据由纯函数给出更准确的拒绝；这里先按扩容方向装步骤
        None => table::TableWriteKind::Grow,
    };
    let lba0 = table::resize_mdos_entry_image(lba0, part, new_size_lba, src.sector_size, total)?;
    let content = crate::assembly::TableContent::Mbr { kind, lba0: &lba0 };
    let plan = crate::assembly::confirmed_table_step(src, content, snapshot)?;
    crate::execution::execute_plan_complete(src, &plan, None, &mut crate::execution::NoProgress)
}

/// 槽位里的长度字段；槽位越界或扇区快照不完整时返回 `None`（由纯判定给出拒绝原因）
fn mbr_entry_size(lba0: &[u8], part: u32) -> Option<u32> {
    let index = part.checked_sub(1)?;
    if index >= 4 {
        return None;
    }
    let off = 446 + index as usize * 16;
    let bytes: [u8; 4] = lba0.get(off + 12..off + 16)?.try_into().ok()?;
    Some(u32::from_le_bytes(bytes))
}

/// MBR 分区被扩到更大之后的收尾：FS 步（swap 重建 / FS 扩容）→ 内核重读 → 统一收尾。
/// grow-to-end 与显式 SIZE 扩容的后置条件完全相同，故两条路径共用本函数——
/// FS 步是后置条件的一部分，缺了会"分区变大、文件系统没变大"却报成功。
/// 外部 FS 工具一旦写完，回滚表项就会留下"表小于内容"的自相矛盾
/// （与 GPT 侧 finalize_growth 同一不变量）。
/// p 是锁下解析出的条目：新跑时是**扩容前**的区间（swap 头部探测用它原区间，原尺寸
/// 是 LVM 位移的基线）；记录型续跑时是已提交的新区间——FS 步按盘上新区间扩满，
/// LVM 链按记录里的基线收尾
pub(crate) fn mbr_grow_finish(
    a: &ResizeRequest,
    src: &mut WriteSource,
    p: &table::MbrPartition,
    is_pv: bool,
    is_block: bool,
) -> crate::outcome::RunOutcome {
    let part = p.num;
    let ss = src.sector_size;
    let cur_bytes = lba_bytes(p.size_lba as u64, ss);
    let mut pending: Vec<crate::outcome::Pending> = Vec::new();
    // --no-fs：分区层之外的后置条件整体出局，与 GPT 路径同语义。
    // FS 步之前重读内核分区表：resize2fs/xfs_growfs/mkswap 都以 /dev 节点的内核尺寸为准，
    // 而离线块设备写表后内核仍持旧长度（不会自动重读），按旧尺寸扩容会空跑并以 0 退出。
    // 重读失败时没有可信的 FS 尺寸，改记待办交给出口（与 GPT 侧 finalize_growth 同一判据）
    if !a.no_fs && kernel_resync(src) {
        // 区域取自**当前**表（本次运行可能已写表）：起点与长度都以盘上现状为准。
        // 此处已越过写盘，判定失败没有"本次未写盘"的出口语义，故 Refused 按 Partial 如实报
        let (base, len) = crate::gpt_policy::partition_bytes(src, part)
            .map_err(after_irreversible)
            .unwrap_or_else(|f| bail_fail(f));
        match fsops::grow_target_at(src, part, base, len) {
            // 走到这里的是写盘前已放行、没有可扩 FS 的那几种：首启 overlay、PV，以及页格式
            // 在本机激活不了的 swap——`unknown` 已由 check_grow_step 拦下，本支至多因盘在
            // 两步之间被改动而见到它。
            // 仍要探一遍"元数据是 swap 却激活不了"那类真实待办——探测用扩容前的区间：
            // swap 签名恒在分区首 32K 内，起点未变，原长度足够容纳
            Ok(g0 @ (fsops::Growable::OverlayPending | fsops::Growable::SwapUnactivatable | fsops::Growable::NoFilesystem(_))) => {
                match movepart::swap_rebuild_pending(
                    src, part, lba_bytes(p.start_lba as u64, ss), lba_bytes(p.size_lba as u64, ss),
                ) {
                    Ok(Some(missed)) => pending.push(missed),
                    // 没有可扩 FS 的几态共用一份分类：日志行与待办都从那里来
                    Ok(None) => {
                        let p = movepart::grow_post(part, &g0);
                        println!("{}", p.log);
                        if let Some(item) = p.pending {
                            pending.push(item);
                        }
                    }
                    // 探测读失败属设备故障，且此刻表多半已写：Partial 的"盘可能已改变"
                    // 警示比伪造一条 Pending 如实
                    Err(e) => bail_fail(RunOutcome::from(e)),
                }
            }
            // swap：内容可弃，表项已扩 → mkswap 重建使新空间生效（UUID/卷标保持；
            // 离线路径仅镜像，块设备走在线路径且 active swap 已被守卫拒绝）
            Ok(fsops::Growable::Target(t)) if t.fstype == fsid::ContentKind::Swap => {
                // swap 的承诺就是 mkswap：外部工具写盘不可回滚，先落屏障
                src.mark_non_reversible().unwrap_or_else(|e| bail_fail(RunOutcome::from(e.into_io())));
                let ident = movepart::read_swap_identity(src, p.start_lba as u64, p.size_lba as u64, ss);
                // 与 movepart 的两个重建点同一口径：文案由 recreate_swap 按实际写入返回，
                // 读不出来时它在动手前就拒绝，不会把随机 UUID 说成"保住了"
                match fsops::recreate_swap(src, part, &ident) {
                    Ok(note) => println!("swap recreated ({note})"),
                    Err(e) => pending.push(crate::outcome::Pending::new(
                        part,
                        crate::outcome::PendingKind::Swap,
                        e.to_string(),
                        fsops::rescue_hint(
                            Some(fsid::ContentKind::Swap),
                            &dev::part_dev_hint(src, part, lba_bytes(p.start_lba as u64, ss)),
                            None,
                        ),
                    )),
                }
            }
            Ok(fsops::Growable::Target(t)) => {
                // 外部 FS 工具（resize2fs/xfs_growfs/…）写盘不可回滚：FS 自述尺寸随即大于
                // 旧分区尺寸，此后回滚表项即"表与内容自相矛盾"，先落屏障
                src.mark_non_reversible().unwrap_or_else(|e| bail_fail(RunOutcome::from(e.into_io())));
                match t.resize_fs(src) {
                    // 扩容已成功，卸载的成败是另一条后置条件：它不改写成功这一事实，
                    // 只作为一条待办随结果浮出
                    Ok(Some(leak)) => pending.push(leak.into_pending(part)),
                    Ok(None) => {}
                    Err(e) => pending.push(crate::outcome::Pending::new(
                        part,
                        crate::outcome::PendingKind::Fs,
                        match &e {
                            // 拒绝类失败发生在任何 FS 字节之前（btrfs 多设备闸在工具启动前
                            // 返回）：此时可断言两件事——表是刚写下的新表，文件系统仍是旧尺寸。
                            // 其余失败可能已改动 FS，不补这句
                            fsops::FsError::UnsupportedFs { .. } => format!(
                                "the partition table is already written and the filesystem keeps its previous size — {e}"
                            ),
                            _ => e.to_string(),
                        },
                        fsops::rescue_hint(
                            Some(t.fstype),
                            &dev::part_dev_hint(src, part, lba_bytes(p.start_lba as u64, ss)),
                            e.unsupported_reason(),
                        ),
                    )),
                }
            }
            // 判定失败：做不了的后置条件 ⇒ PARTIAL；设备读不出来 ⇒ 报失败
            Err(e) => match RunOutcome::from(e) {
                RunOutcome::Refused(why) => pending.push(crate::outcome::Pending::new(
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
        let record = src.record_journal().map(|j| j.record());
        let o = settle_layout(crate::outcome::RunOutcome::applied_with(Vec::new()), src, record);
        return finish_resize(a, o, Some(src), part, is_pv, is_block, None, cur_bytes);
    }
    let mut o = crate::outcome::RunOutcome::applied_with(pending);
    if !kernel_resync(src) {
        o.mark_kernel_stale();
    }
    o = o.report_final();
    // PV 的 pvresize/lvextend 链是本次操作的后置条件之一，不因 swap 等外层待办而跳过：
    // finish_resize 只在非 PV 的未完成结果上早退，PV 则照常进链，并以两者中更严重的
    // 退出码收口。链自身的失败在 resize_done 内另报，与上面的待办清单不是同一条
    if is_pv {
        return finish_resize(a, o, Some(src), part, is_pv, is_block, None, cur_bytes);
    }
    o
}

/// resize 的 MBR 分支（仅主分区 1..4；逻辑分区与扩展容器不支持）。原位扩容：扩须右侧
/// 空闲足够（MBR 无搬移能力），缩容由 `resize-part` 承担（本命令直接拒绝）。块设备复用在线
/// 路径（基于 sysfs + sfdisk，与表类型无关）
pub(crate) fn cmd_resize_msdos(a: &ResizeRequest, pref: PartSelector, src_ro: &FileSource) -> crate::outcome::RunOutcome {
    // MBR resize 没有搬移能力：--allow-move 承诺的"搬开挡路分区"不存在，
    // 静默按不可搬移处理会让来自脚本的调用只看到一条"空间不足"
    if a.allow_move {
        bail_fail(RunOutcome::refused("--allow-move is not supported for MBR resize — MBR cannot relocate blocking partitions".to_string()));
    }
    // 离线路径不消费 chunk：在线检查（resize_online）只在块设备上执行，镜像文件
    // 与非 Linux 平台会绕过它落到这里，取值校验也随之跳过
    if a.chunk_given {
        bail_fail(RunOutcome::refused(
            "--chunk has no effect for MBR resize — MBR cannot relocate blocking partitions".to_string(),
        ));
    }
    let mbr = table::parse_mbr(src_ro)
        .unwrap_or_else(|e| {
            bail_fail(crate::outcome::mbr_parse_failure(&e, format!("parse failed: {e}")))
        })
        .unwrap_or_else(|| bail_fail(RunOutcome::refused("no MBR on target".to_string())));
    // 只读快照
    let part = crate::gpt_policy::resolve_part_in(crate::gpt_policy::TableEntries::Mbr(&mbr), pref)
        .unwrap_or_else(|f| bail_fail(f));
    let p = match mbr.iter().find(|p| p.num == part) {
        Some(p) => p,
        None => bail_fail(RunOutcome::refused(format!("partition {part} not found (MBR resize covers primary partitions 1..4 only)"))),
    };
    if p.is_container {
        bail_fail(RunOutcome::refused("extended partition container cannot be resized (logical partitions are out of scope)".to_string()));
    }
    let ss = src_ro.sector_size;
    let cur_bytes = lba_bytes(p.size_lba as u64, ss);
    let fstype = fsid::identify(src_ro, lba_bytes(p.start_lba as u64, ss), lba_bytes(p.size_lba as u64, ss))
        .unwrap_or_else(|e| bail_fail(RunOutcome::no_mutation(format!("identify failed: {e}"))));
    let is_pv = fstype == fsid::ContentKind::Lvm2Pv;

    let (target, grow_to_end) = resolve_size_request(a.size, cur_bytes).unwrap_or_else(|f| bail_fail(f));
    let shrinking = target.is_some_and(|t| t < cur_bytes);
    // 缩容不走 `resize`，在线/离线两条路径同一答案
    if shrinking {
        bail_fail(super::refuse_shrink());
    }

    check_pv_intent(part, Some(fstype), is_pv, shrinking, a.grow_lv, a.no_fs).unwrap_or_else(|f| bail_fail(f));
    let is_block = src_ro.is_block;

    // 块设备在线路径：与 GPT 同一份实现，随表类型变的只有右侧空闲数。目标长度同样
    // 交给 mbr_decider 在在线自己的锁下重解析（理由见 gpt_decider）
    #[cfg(target_os = "linux")]
    if let Some(o) = resize_online(a, src_ro, part, is_pv, mbr_decider(a.size, part, is_pv)) {
        return o;
    }

    // 离线路径
    // 记录按可续跑语义打开：MBR 与 GPT 共用同一族记录，表写之后的 FS/PV 收尾同样是
    // 本次作业的一部分，重跑要能接着做
    let (mut src, entry) = open_target_resize(a, a.grow_lv).unwrap_or_else(|f| bail_fail(f));
    let resumed = entry.resumed;
    // 锁下重取权威几何（与 GPT 分支同一原则）：只读阶段与取得独占权之间盘可以被别人
    // 改写（sfdisk/fdisk 不守本工具的锁），用锁前的 mbr/p/free 驱动写分支就是把过期
    // 决定写进盘——resize_mdos_entry 不复核邻接，过期的 free 没有第二道防线
    let mbr = table::parse_mbr(&src)
        .unwrap_or_else(|e| {
            bail_fail(crate::outcome::mbr_parse_failure(&e, format!("parse failed: {e}")))
        })
        .unwrap_or_else(|| bail_fail(RunOutcome::refused("no MBR on target".to_string())));
    // 锁下快照
    let part = crate::gpt_policy::resolve_part_in(crate::gpt_policy::TableEntries::Mbr(&mbr), pref)
        .unwrap_or_else(|f| bail_fail(f));
    let p = match mbr.iter().find(|p| p.num == part) {
        Some(p) => p,
        None => bail_fail(RunOutcome::refused(format!("partition {part} not found (MBR resize covers primary partitions 1..4 only)"))),
    };
    if p.is_container {
        bail_fail(RunOutcome::refused("extended partition container cannot be resized (logical partitions are out of scope)".to_string()));
    }
    // FS 类型按锁下现状重取：识别与取锁之间分区可被重新格式化（mkfs 不守本工具的锁），
    // 拿旧类型选工具就是把 ext4 的工具链砸到 xfs 上；PV 判据随之重算
    let fstype = fsid::identify(&src, lba_bytes(p.start_lba as u64, ss), lba_bytes(p.size_lba as u64, ss))
        .unwrap_or_else(|e| bail_fail(RunOutcome::no_mutation(format!("identify failed: {e}"))));
    let is_pv = fstype == fsid::ContentKind::Lvm2Pv;
    // SIZE 锚点按锁下的新分区尺寸重算；grow 标记是请求的形状，锁前锁后同值（见 GPT 分支）
    let cur_bytes = lba_bytes(p.size_lba as u64, ss);
    let (target, _) = resolve_size_request(a.size, cur_bytes).unwrap_or_else(|f| bail_fail(f));
    let shrinking = target.is_some_and(|t| t < cur_bytes);
    // 锁下的方向可能因窗口内的外部改写而与锁前相反（缩容一律不走这里）
    if shrinking {
        bail_fail(super::refuse_shrink());
    }
    check_pv_intent(part, Some(fstype), is_pv, shrinking, a.grow_lv, a.no_fs).unwrap_or_else(|f| bail_fail(f));
    // PV 链的工具必须在写表之前齐备（与 GPT 分支同一处、同一判据）：pvresize/pvs 及
    // grow-lv 的 lvs/vgs/lvextend 只在表提交之后启动，缺工具那时才发现就只剩 PARTIAL
    #[cfg(target_os = "linux")]
    if is_pv {
        preflight_lvm_tools(a.grow_lv, !is_block).unwrap_or_else(|e| bail_fail(RunOutcome::from(e)));
        // 要扩哪个 LV 必须在此定下来（与 GPT 分支同一处判据）：--lv 写错 / 无 LV /
        // LV 不唯一都在写表之前发现，提交之后只剩"表已改、PV 链未做"的现场
        if a.grow_lv {
            let start_bytes = lba_bytes(p.start_lba as u64, ss);
            preflight_lv_selection(&src, part, is_block, start_bytes, a.lv.as_deref())
                .map(|_| ())
                .unwrap_or_else(|e| bail_fail(lvm_preflight_fail(e)));
        }
    }
    // 记录里带着 LVM 收尾基线（上次请求过 --grow-lv），本次却没给：记录不存 LV 路径，
    // 收尾只能按本次请求重选。静默退成只跑 pvresize 等于把承诺过的 LV 扩容丢掉
    // 判据与 GPT 分支同一处
    if !a.grow_lv && src.record_journal().is_some_and(|j| j.lvm_post().is_some()) {
        bail_fail(RunOutcome::refused(
            "an unfinished resize recorded an LV growth — re-run with --grow-lv (the record does not \
             store the LV path)"
                .to_string(),
        ));
    }
    // 占用复核在锁下（离线选择时的探测在锁前）：首次落盘前确认分区仍空闲，与 GPT 分支闸同源。
    // 起点不变、末端不变或只增的请求不挪动上层 DM 栈映射的扇区，按 Extend 放行；缩容在入口即拒绝
    let edit = if grow_to_end || target.is_some_and(|b| b >= cur_bytes) {
        crate::fsops::WriteKind::Extend
    } else {
        crate::fsops::WriteKind::Overwrite
    };
    crate::fsops::ensure_idle_before_write(&src, part, lba_bytes(p.start_lba as u64, ss), edit).unwrap_or_else(|f| bail_fail(f));
    let total_sectors = src.size / ss;
    // 后置条件含 FS 调整：请求在扩（`grow` 即便右侧无空闲，也仍要把 FS 扩满现分区，
    // 见 `free == 0` 那支）就必须在写表之前问出"里面是什么、那一步做得了吗"——判据与 GPT 路径
    // 同一处。缩容在入口即拒绝，no-op 两不动，故都不在此列
    let extends = grow_to_end || target.is_some_and(|b| b / ss > p.size_lba as u64);
    if extends && !a.no_fs {
        fsops::grow_target_at(&src, part, lba_bytes(p.start_lba as u64, ss), lba_bytes(p.size_lba as u64, ss))
            .and_then(|g| fsops::check_grow_step(g, is_block))
            .unwrap_or_else(|e| bail_fail(RunOutcome::from(e)));
    }
    // 记录型续跑：记录里有提交前区间、且盘上区间已不是它 ⇒ 分区层提交过，当前表项
    // 就是本次作业的目标。MBR 没有搬移，但表写之后的 FS/PV 收尾同样是本次作业的一部分；
    // 按现算的右侧空闲分流会把它拒成 "no free space to its right"
    let table_range = (u64::from(p.start_lba), u64::from(p.start_lba) + u64::from(p.size_lba) - 1);
    let record_committed = resumed
        && src.record_journal()
            .and_then(|j| j.old_range())
            .is_some_and(|old| old != table_range);
    // 提交过的事实只对"本次不要求再写表"的请求成立：请求更大的尺寸时按新尺寸再扩一次，
    // 记录里的旧区间随下一次写表保持有效
    let grow_again = !grow_to_end && target.is_some_and(|b| b / ss > u64::from(p.size_lba));
    if record_committed && !grow_again {
        return mbr_grow_finish(a, &mut src, p, is_pv, is_block);
    }
    if grow_to_end {
        let free = free_right_msdos(&mbr, p, total_sectors);
        // 请求的分区增长无法实现：与 GPT 的 `resize grow` 及在线路径同语义拒绝，而不是
        // 只把 FS 扩满现分区再以 0 退出——那会让调用方以为分区表也已更新
        if free == 0 {
            // PV 例外：分区层没有可扩的扇区，但 pvresize/lvextend 仍是本次操作的后置
            // 条件（上一次运行可能正停在提交之后），交给收尾链续做
            if is_pv {
                eprintln!("partition {part} cannot grow in the table — no free space to its right; verifying the PV chain");
                return mbr_grow_finish(a, &mut src, p, is_pv, is_block);
            }
            bail_fail(RunOutcome::refused(
                "grow would not extend the partition — no free space to its right (MBR cannot relocate blocking partitions); \
                 use `resizefs` to fill the filesystem inside the current partition".to_string(),
            ));
        }
        let new_size_lba = p.size_lba as u64 + free;
        if new_size_lba > u32::MAX as u64 {
            bail_fail(RunOutcome::refused(format!("new size {new_size_lba} sectors exceeds MBR 32-bit LBA limit")));
        }
        record_old_range(&mut src, p).unwrap_or_else(|f| bail_fail(f));
        resize_mbr_entry(&mut src, part, new_size_lba as u32)
            .unwrap_or_else(|f| bail_fail(f));
        crate::movepart::fault_mbr_after_table_write();
        mbr_grow_finish(a, &mut src, p, is_pv, is_block)
    } else {
        let Some(bytes) = target else {
            bail_fail(RunOutcome::refused("`resize` needs a size: diskedit resize <TARGET>[:N] <SIZE> (see diskedit help resize)".to_string()));
        };
        if bytes < ss {
            bail_fail(RunOutcome::refused(format!("size {bytes} < one sector ({ss})")));
        }
        let new_size_lba = bytes / ss;
        if new_size_lba > u32::MAX as u64 {
            bail_fail(RunOutcome::refused(format!("size {new_size_lba} sectors exceeds MBR 32-bit LBA limit")));
        }
        // 越界与右侧空闲均已判定，取整提示不会跟在一条拒绝后面
        if new_size_lba == p.size_lba as u64 {
            // 请求的终点就是现状：表与 FS 都不动（GPT 路径对未变尺寸同样判 FsAction::Leave）。
            // 不跑 FS 步，请求"什么都不改"就不会因外部工具缺失而拿到非零退出码。
            // PV 例外与续跑例外：分区层无变化不意味着本次操作没有后置条件——PV 的
            // 收尾链要跑，续跑的记录（可能带着 FS 待办）也要收尾。finish 链里 FS 扩到
            // 现状大小是幂等 no-op，续跑真正要的是链内的记录收尾（与 GPT 侧
            // !resuming 才短路的口径对齐）
            crate::support::note_sector_truncation("resizing to", bytes, ss);
            if is_pv || resumed {
                return mbr_grow_finish(a, &mut src, p, is_pv, is_block);
            }
            println!("partition {part} is already {cur_bytes} bytes — nothing to do");
            // 表与 FS 都没动：事务已完成且没有待办
            return crate::outcome::RunOutcome::applied_with(Vec::new());
        }
        if new_size_lba > p.size_lba as u64 {
            let want = new_size_lba - p.size_lba as u64;
            if want > free_right_msdos(&mbr, p, total_sectors) {
                bail_fail(RunOutcome::refused("not enough contiguous free space to the right (MBR resize cannot relocate blocking partitions)".to_string()));
            }
            crate::support::note_sector_truncation("resizing to", bytes, ss);
            // 写 LBA0 之前的所有读与校验都已完成（见 resize_mdos_entry）：这里的失败
            // 一律是"表确定未写"，不该套 after_irreversible 的"盘可能已改变"
            record_old_range(&mut src, p).unwrap_or_else(|f| bail_fail(f));
            resize_mbr_entry(&mut src, part, new_size_lba as u32)
                .unwrap_or_else(|f| bail_fail(f));
            // 分区层做完 → 与 grow-to-end 同一条收尾
            mbr_grow_finish(a, &mut src, p, is_pv, is_block)
        } else {
            // 相等已在上面短路；到这里只剩缩小，锁前与锁下两处 `refuse_shrink` 本应先拦住
            bail_fail(super::refuse_shrink())
        }
    }
}
