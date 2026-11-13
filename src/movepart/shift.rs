//! `resize --allow-move` 的判定半程与执行装配：事前判定（只读）、绑定现场、扩容后的 FS 收尾。
//!
//! 写盘不在本模块：搬移、表提交与换区重建都由 [`crate::execution::execute_plan`]
//! 执行，本模块把判定结论装配成一份计划并交给它。事务入口与归责在
//! [`crate::cmd::resize::apply_move`]。持久状态是恢复记录里的
//! [`crate::recovery::Relocation`]：有序计划、双游标、块字节、作业语义与几何守卫
//! 同属一份记录，与 `resize` 的分区层/FS/LVM 收尾共用一个事务号。

use super::*;

/// 分区已扩、swap 未重建：创建于 32K 页的 swap 在本机（4K 页）激活不了，identify 按
/// swapon 口径认不出它，但元数据确实是 swap，头里的页数仍是扩前的值——这是一条真实的
/// 未完成后置条件，报成功就是静默成功。判据、措辞、补救命令同处一处；探测区间由调用点
/// 决定（扩前 / 扩后 / 提交后各不相同，那是调用点的知识）
pub(crate) fn swap_rebuild_pending(
    src: &FileSource,
    part: u32,
    base: u64,
    len_bytes: u64,
) -> io::Result<Option<Pending>> {
    // 读失败≠"确认存在待办"：Pending 文案断言"本机激活不了"只在探测成功且命中时成立，
    // 设备 I/O 故障属于另一码事，按故障上抛（调用点都在写盘之后，Partial 的
    // "盘可能已改变"警示才是对状态的如实表述）
    if !matches!(crate::fsid::probe_fs(src, base, len_bytes)?.swap, Some(h) if !h.activatable) {
        return Ok(None);
    }
    Ok(Some(Pending::new(
        part,
        PendingKind::Swap,
        "partition grown but the swap area was not rebuilt (its page format is not activatable on this host)",
        crate::fsops::rescue_hint(Some(ContentKind::Swap), &crate::dev::part_dev_hint(src, part, base), None),
    )))
}

/// 搬移换区条目的平台与工具前置：`mkswap` 重建 swap 区，镜像目标的重建对象是 loop
/// 分区设备，还要 `losetup`。工具缺失与平台不符都归 Unsupported(6)——分类走 outcome
/// 的唯一映射点，不在这里把变体压成字符串再折成"改参数重试有意义"。
///
/// 新跑必须在任何目标写入之前调用：否则会先把修复与表改完、再以 pending 收场。
/// 恢复分支不套用：已开始的作业缺工具只能在执行期以 pending 收场并保留现场
pub(crate) fn ensure_swap_rebuild_tools(is_block: bool) -> Result<(), RunOutcome> {
    crate::fsops::require_linux().map_err(RunOutcome::from)?;
    crate::fsops::check_support(
        "rebuild",
        ContentKind::Swap.as_str(),
        crate::fsops::swap_rebuild_support(is_block),
    )
    .map_err(RunOutcome::from)?;
    Ok(())
}

/// 事前判定的产物，也是 execute 的**唯一输入源**：凡是执行阶段要用的都在这里带过去。
/// `chunk_len` / `no_fs` 若同时留在 execute 的参数位上，同一个值就有两个来源、谁赢没有定义。
/// `g` 随行：执行半程对表项的全部落位都基于它（不重读盘上的表）
pub(crate) struct ApplyDecision {
    pub(crate) g: ValidatedGeometry,
    /// 本次要绑定（或确认已一致）的有序计划与双游标
    pub(crate) relocation: crate::recovery::Relocation,
    /// 扩容分区在表提交前的区间。写完表之后盘上只剩新区间，续跑靠它区分
    /// "分区层已提交、只差 FS/PV 收尾"与"索引已推进但表未提交"
    pub(crate) old_range: (u64, u64),
    pub(crate) chunk_len: u64,
    pub(crate) no_fs: bool,
    /// 调用方在 apply 之后还要跑 LVM 收尾链：撤记录的决定属于那条链的出口
    pub(crate) defer_ckpt: bool,
    /// LVM 基线。新跑由本次增量算出；续跑取记录里的值（LV 路径不在记录里，
    /// 由续跑请求重新选定）
    pub(crate) lvm: Option<LvmPostFacts>,
}

/// 事前判定（只读）：FS preflight → 槽位判定。
/// 返回 `Err` 一律发生在任何写盘之前；本函数签名上就没有 `&mut WriteSource`
#[allow(clippy::too_many_arguments)] // 与 apply 同一组入参
pub(crate) fn prepare_apply(
    src: &FileSource,
    g0: &ValidatedGeometry,
    plan: &Plan,
    chunk_len: u64,
    no_fs: bool,
    defer_ckpt: bool,
    lvm: Option<LvmSelection>,
    existing_relocation: Option<&crate::recovery::Relocation>,
    existing_lvm: Option<crate::recovery::LvmPost>,
    log: &mut dyn FnMut(&str),
) -> Result<ApplyDecision, RunOutcome> {
    // F5：记录已绑搬移计划、本次读到的备份头不可读、记录里的目的区末端越过主头
    // `last_usable_lba`。三条同时成立才拒绝——备份头坏而主头上界够用时续跑合法。
    // 本函数只读，且位于 bind 与 repair 之前，故此刻确实一个目标字节都没写
    if let Some(reloc) = existing_relocation {
        let mut request_end = 0u64;
        let mut complete = true;
        for e in &reloc.entries {
            match e
                .src_first
                .checked_add_signed(e.delta)
                .and_then(|d| d.checked_add(e.len))
                .and_then(|v| v.checked_sub(1))
            {
                Some(end) => request_end = request_end.max(end),
                None => complete = false,
            }
        }
        if complete && crate::gpt_policy::resume_boundary_unavailable(g0, true, request_end) {
            return Err(RunOutcome::refused(format!(
                "the recovery record's relocation ends at LBA {request_end}, past the primary header's last \
                 usable LBA {}, and the backup header cannot be read — the boundary facts needed to resume are \
                 unavailable; this run wrote no bytes and the disk is where the previous job stopped. Check the \
                 record with `diskedit inspect`, then release it with `diskedit abandon`; a fresh run records \
                 the job again",
                g0.decided_last_usable_lba()
            )));
        }
    }
    // 占用闸（与 prepare_resize 同一判据）：被搬移分区的数据区会被整体重写，grow 目标
    // 随后要接 FS 步——挂载中或活动 swap 的分区必须在任何写盘前拒绝。镜像无块设备
    // 占用语义，闸内自会放行
    for m in &plan.moves {
        // 续跑时按盘上**当前**起点探测：已搬完的条目内核视图停在新位置，
        // 探旧起点找不到节点，且新位置上的占用也看不见；新跑两者重合，行为不变
        let probe_lba = g0.entry_index(m.part_num).and_then(|i| g0.entries().get(i))
            .map(|e| e.starting_lba)
            .unwrap_or(m.first_lba);
        crate::fsops::ensure_idle_before_write(src, m.part_num, probe_lba.checked_mul(plan.ss)
            .ok_or_else(|| RunOutcome::no_mutation("move source start overflows byte offset"))?, crate::fsops::WriteKind::Overwrite)?;
    }
    // 几何由调用方解析并下传（构造点即拒绝条目重叠）；几何里的上界是"修复生效后"的值，
    // ckpt 的边界校验依赖它。盘在 plan 生成之后变了的话，由下面的恢复校验拒绝
    // 写盘前的 preflight：FS 扩展属本次操作的后置条件，工具缺失必须现在拒绝——
    // 一旦开始写盘才发现，就会留下"分区已改、FS 未扩"的中间态。
    // 不依赖命令层是否检查过：续传路径不经过 plan，本处才是唯一必经关口
    let grow_entry = g0.entry_index(plan.grow_part).and_then(|i| g0.entries().get(i));
    let Some(ge) = grow_entry.filter(|ge| ge.ending_lba != 0) else {
        // 新跑路径 live_entry 已保证条目非空；空槽只出现在续跑——盘上布局在中断窗口
        // 被外部改动。不拒绝的话，下游会把 starting_lba=0 的空条目扩成横跨表头的大条目
        return Err(crate::outcome::RunOutcome::refused(format!(
            "partition {} has an empty entry on disk — the layout moved under the job; refusing",
            plan.grow_part
        )));
    };
    // 目标分区只向右扩、起点不变：DM 栈已映射的扇区不受影响，按 Extend 放行其持有
    crate::fsops::ensure_idle_before_write(src, plan.grow_part, ge.starting_lba.checked_mul(plan.ss)
        .ok_or_else(|| RunOutcome::no_mutation("grow target start overflows byte offset"))?, crate::fsops::WriteKind::Extend)?;
    if !no_fs {
        // 要动的是哪一段、里面是什么 FS 由 grow_target_at 判定——与写盘后的收尾同一判据，
        // 两处只是区间不同。区间的 LBA 单位是**表自身**的 ss（plan.ss）。此处位于
        // apply_repair / 搬移之前，本次调用尚未写目标盘：它的环境故障按 NoMutation 报，
        // "不支持 / 工具缺失 / 里面是什么都不知道"交 FsError 分流（6 / 6 / 3），不在此另判
        let base = ge.starting_lba.checked_mul(plan.ss)
            .ok_or_else(|| RunOutcome::no_mutation("grow target start overflows byte offset"))?;
        let len = (ge.ending_lba - ge.starting_lba + 1).checked_mul(plan.ss)
            .ok_or_else(|| RunOutcome::no_mutation("grow target length overflows byte size"))?;
        crate::fsops::check_grow_step(crate::fsops::grow_target_at(src, plan.grow_part, base, len)?, src.is_block)?;
    }
    // 恢复三态：有效 → 续传 / 槽位被另一族作业占用 → 拒绝 / 空 → 新建
    // 落点取命中路径：续传写同一份现场；空槽返回本次命名落点
    // 新跑的 LVM 收尾事实由目标分区的实际新末端与写表前选定的 LV 导出；续跑命中的
    // 现场自带基线，不再按本次算出的增量重推（盘上可能已是新尺寸）
    let derived_lvm = {
        let future_grow_end = grow_end_for(plan, ge.starting_lba)?;
        let old_len = ge.ending_lba.checked_sub(ge.starting_lba)
            .and_then(|v| v.checked_add(1))
            .ok_or_else(|| crate::outcome::RunOutcome::no_mutation("grow target entry inverted"))?;
        let new_len = future_grow_end.checked_sub(ge.starting_lba)
            .and_then(|v| v.checked_add(1))
            .ok_or_else(|| crate::outcome::RunOutcome::no_mutation("grow target length overflows"))?;
        let old_bytes = old_len.checked_mul(plan.ss)
            .ok_or_else(|| crate::outcome::RunOutcome::no_mutation("grow target length overflows byte size"))?;
        let new_bytes = new_len.checked_mul(plan.ss)
            .ok_or_else(|| crate::outcome::RunOutcome::no_mutation("grow target length overflows byte size"))?;
        lvm_post_for(defer_ckpt, lvm.as_ref(), old_bytes, new_bytes)?
    };
    // 记录里的 LVM 基线只存三个数，LV 路径由本次请求重新选定；续跑沿用这份基线，
    // 不按本次算出的增量重推（盘上可能已是新尺寸）
    let resumed_lvm = existing_lvm.map(|p| LvmPostFacts {
        lv_path: None,
        lv_size_before: p.lv_size_before,
        lv_target: p.lv_target,
        extent_bytes: p.extent_bytes,
    });
    let relocation = match existing_relocation {
        Some(c) => {
            // 记录与盘面的全部一致性判据只在 `verify_relocation_resume` 一处：
            // `move` 走同一条
            crate::movepart::verify_relocation_resume(c, g0, src.size, chunk_len)?;
            let done = c.entry_index;
            // Y = durable 恢复点（记录里的值），不是本进程内存进度
            log(&format!("resuming at entry {} chunk {} (durable record)", c.entry_index, c.chunks_in_entry));
            crate::recovery::Relocation {
                entries: plan.moves.iter().map(crate::recovery::RelocEntry::from).collect(),
                entry_index: done,
                chunks_in_entry: c.chunks_in_entry,
                chunk_bytes: chunk_len,
                kind: plan.kind.into(),
                grow_part: plan.grow_part,
                disk_size: src.size,
                ss: plan.ss,
            }
        }
        None => {
            // 走到这里有两种来源：新跑（plan 由当前盘上几何生成，成员自然停在 first_lba），
            // 或 plan 生成时还读到过搬移现场、而它在本次读取之前消失/损坏（例如并发的
            // `abandon`）。后者本进程一个 chunk 都没搬过，成员必须
            // 仍停在 first_lba：已推进到目标位的成员，其源区可能已被右邻的拷贝覆盖，按旧
            // plan 从旧起点重读会把垃圾写回目标位，而后续表项提交照常成功
            for m in &plan.moves {
                let cur = g0
                    .entry_index(m.part_num)
                    .and_then(|i| g0.entries().get(i))
                    .ok_or_else(|| crate::outcome::RunOutcome::refused(format!(
                        "the plan names partition {} that is no longer on the disk — refusing",
                        m.part_num
                    )))?;
                let src_end = m.first_lba.checked_add(m.len_lba).and_then(|v| v.checked_sub(1))
                    .ok_or_else(|| crate::outcome::RunOutcome::no_mutation("relocation source range overflows"))?;
                if cur.starting_lba != m.first_lba || cur.ending_lba != src_end {
                    return Err(crate::outcome::RunOutcome::refused(format!(
                        "the recovery record vanished between reads and partition {} is not at its source position \
                         (on disk LBA {}..{}, expected {}..{}) — the layout moved under the job; refusing",
                        m.part_num, cur.starting_lba, cur.ending_lba, m.first_lba, src_end
                    )));
                }
            }
            // 被搬移的 swap 条目在表项提交后由 recreate_swap 重建（mkswap；镜像目标的重建
            // 对象是 loop 分区设备，还要 losetup），且不受 --no-fs 约束。新跑必须现在确认
            // 平台与工具可用：否则会先把表改完，再以 pending 收场。恢复分支不套用这条——
            // 已有 relocation ckpt 是已经开始的作业，缺工具只能在执行期以 pending 收场并
            // 保留现场，不能反过来阻断恢复。预检只覆盖平台与工具存在性：PATH 里有可执行
            // 文件不等于它一定能成功操作目标（权限、设备策略），那些失败仍落在执行期
            if plan.moves.iter().any(|m| m.is_swap) {
                ensure_swap_rebuild_tools(src.is_block)?;
            }
            crate::recovery::Relocation {
                entries: plan.moves.iter().map(crate::recovery::RelocEntry::from).collect(),
                entry_index: 0,
                chunks_in_entry: 0,
                chunk_bytes: chunk_len,
                kind: plan.kind.into(),
                grow_part: plan.grow_part,
                disk_size: src.size,
                ss: plan.ss,
            }
        }
    };
    Ok(ApplyDecision {
        g: g0.clone(),
        relocation,
        old_range: (ge.starting_lba, ge.ending_lba),
        chunk_len,
        no_fs,
        defer_ckpt,
        lvm: resumed_lvm.or(derived_lvm),
    })
}

/// 数据 chunk 的 I/O 失败必须带着精确 LBA 区间上抛：errno 说不出"哪一段"，
/// 用户需要它区分瞬态错误（重跑续传）与永久坏块（先做救援镜像再对镜像操作）。
/// 本层只报告区间与出路，不判断块对文件系统是否已分配、可否弃——那是 fsck 的事
pub(crate) fn chunk_io_error(
    e: io::Error,
    action: &str,
    base_lba: u64,
    len_lba: u64,
    chunk: usize,
    total: usize,
    hint: &str,
) -> io::Error {
    // 诊断区间同样不回绕：chunk 搬移的算术全部 checked，报错的 LBA 没有理由例外
    let end = base_lba.saturating_add(len_lba);
    io::Error::other(format!(
        "{action} failed at LBA {base_lba}..{end} (chunk {chunk}/{total}): {e} — {hint}"
    ))
}

/// `copy` 的块 I/O 失败提示：副本没有续跑，目标字节可能已被覆盖
pub(crate) const COPY_HINT: &str = "copy has no resume — the target bytes may already be overwritten; release the scene with `diskedit abandon`, then re-run; a permanently unreadable block must be imaged with ddrescue first";

/// 搬移进度的唯一持久载体是恢复记录；写会话里没有它就是事务装配错误
fn record_journal(src: &mut WriteSource) -> io::Result<&mut crate::recovery::RecordJournal> {
    src.record_journal_mut().ok_or_else(|| {
        io::Error::other("the write session carries no recovery record — refusing to relocate")
    })
}

/// 绑定计划与 LVM 基线（同一次写入）。已有同值计划时是 no-op，不重复落盘
pub(crate) fn bind_relocation(
    src: &mut WriteSource,
    reloc: &crate::recovery::Relocation,
    lvm: Option<&LvmPostFacts>,
) -> Result<(), crate::recovery::PersistFailure> {
    record_journal(src)
        .map_err(crate::recovery::PersistFailure::NotWritten)?
        .bind_relocation(reloc.clone(), lvm.map(LvmPostFacts::to_record))
}

/// 执行（写盘）：装配 → 绑定现场 → 执行器 → 扩容收尾。
///
/// 搬移、表提交与换区重建都由 [`crate::execution::execute_plan`] 执行：它先做完整预扫描
/// （执行器缺席、未装配的表步骤、块布局与地址区间），首写之后只可能给出"盘可能已改变"，
/// 由执行器统一承担。
/// 本函数只剩三件事：把记录绑定到这次作业、把搬移与扩容装配成一份计划、在计划完整走完
/// 之后收尾文件系统。
pub(crate) fn execute_apply(
    src: &mut WriteSource,
    plan: &Plan,
    d: ApplyDecision,
    log: &mut dyn FnMut(&str),
    post: &mut Vec<Pending>,
) -> Result<(), RunOutcome> {
    let mut g = d.g.clone();
    let grow_idx = (plan.grow_part - 1) as usize;
    let grow_start = g.entries().get(grow_idx).map(|e| e.starting_lba).ok_or_else(|| {
        RunOutcome::no_mutation(format!("partition {} is outside the entry array", plan.grow_part))
    })?;
    // 扩容终点由计划里的目的区决定：moves 是尾→首的紧凑序，最左的新起点即本次腾出
    // 空间的左边界。它与搬移同属一份作业，因此和搬移写进同一份计划、同一条表戳链
    let grow_end = grow_end_for(plan, grow_start)
        .map_err(|e| RunOutcome::no_mutation(e.to_string()))?;
    let old_len = d.old_range.1.checked_sub(d.old_range.0).and_then(|v| v.checked_add(1))
        .ok_or_else(|| RunOutcome::no_mutation("grow target entry inverted"))?;
    let grow_len = grow_end.checked_sub(grow_start).and_then(|v| v.checked_add(1))
        .ok_or_else(|| RunOutcome::no_mutation("grow target length overflows"))?;

    // 换区条目的盘上类型必须仍然是 swap：两次运行之间被改成别的类型时，按旧判定 mkswap
    // 会把一个已不属于 swap 的分区覆写掉。类型读不出来同样按"不再成立"拒绝——
    // 绝不用无知当许可
    for m in &plan.moves {
        if !m.is_swap {
            continue;
        }
        let still_swap = crate::gpt_policy::live_entry(&g, m.part_num)
            .is_ok_and(|e| is_swap_guid(&e.partition_type_guid));
        if !still_swap {
            return Err(RunOutcome::refused(format!(
                "partition {} is not a swap partition on disk any more (its type changed since the \
                 checkpoint) — refusing to recreate it there; release the scene with `diskedit abort`",
                m.part_num
            )));
        }
    }

    // 判定与写盘之间的外部改动：条目必须还是锁下读到的那一份。装配会把随行几何写成
    // 表内容，少了这一步，等待用户确认期间别人改的表会被旧几何静默覆盖
    g.ensure_entries_unchanged_fail(src)?;
    // 搬移与扩容装配成一份计划：每一步的写前快照是前一步的写后表戳，盘在装配之后被
    // 外部改动时由执行器的首写前比对拒绝
    let moves: Vec<(u32, u64)> = plan
        .moves
        .iter()
        .map(|m| {
            m.first_lba
                .checked_add_signed(m.delta_lba)
                .map(|dst| (m.part_num, dst))
                .ok_or_else(|| RunOutcome::no_mutation("relocation target LBA overflows"))
        })
        .collect::<Result<_, _>>()?;
    let confirmed = crate::assembly::assemble_resize_move_plan(
        src,
        &mut g,
        &moves,
        (plan.grow_part, grow_end),
        Some(&d.relocation),
        log,
    )?;

    // 现场先绑：执行器从记录里的条目游标决定从哪继续，记录里没有搬移计划时它拒绝推进游标
    bind_relocation(src, &d.relocation, d.lvm.as_ref())
        .map_err(|e| RunOutcome::no_mutation(e.bind_failure()))?;
    record_journal(src)?.bind_old_range(d.old_range).map_err(|e| {
        RunOutcome::no_mutation(e.record_failure("the pre-commit range"))
    })?;

    let mut sink = crate::recovery::sink::RecordSink::with_log(log);
    match crate::execution::execute_plan(src, &confirmed, Some(d.chunk_len), &mut sink, post)? {
        // 换区重建失败：条目循环停在那一处，扩容收尾不能做——grow_end 按全体 moves 取
        // min(new_first)−1，把尚未搬移成员的旧位也划进扩容区，提交即写出与未搬移分区
        // 重叠的表项。记录原样保留，重跑在同一记录上从本条目重试 mkswap
        crate::execution::StepFlow::Stopped => {
            log("relocation incomplete (swap rebuild pending) — growth not applied, re-run the command to continue");
            return Ok(());
        }
        crate::execution::StepFlow::Complete => {}
    }

    log(&format!("partition {} extended (FS area ends before relocated partitions)", plan.grow_part));
    // 扩到设备末端的 FS 工具（resize2fs 一类）以内核报告的尺寸为准：内核视图停在旧尺寸时
    // 它们会认为无事可做、以 0 退出，把"分区已扩、FS 未扩"洗成成功。重读失败时不跑 FS 步，
    // 把扩容记成待办
    let kernel_followed = crate::support::kernel_resync(src);
    // FS resize 是契约的一部分：失败即后置条件未满足（由调用方换算为 PARTIAL）。
    // 这里不能只打日志当成功——脚本会据此认为空间已可用
    if d.no_fs {
        log("partition extended (--no-fs: filesystem left untouched)");
    } else if !kernel_followed {
        post.push(Pending::new(
            plan.grow_part,
            PendingKind::Fs,
            "the kernel partition view still reports the old size, so the filesystem was not resized — refresh the view (partprobe / partx) and re-run to finish",
            String::new(),
        ));
    } else {
        finalize_growth(
            src,
            plan.grow_part,
            GrowthRegions { old: (grow_start, old_len), new: (grow_start, grow_len) },
            g.sector_size(),
            // 声明侧：表条目当前的类型 GUID。搬移只改起止，类型不随几何变化；
            // 条目在写表前已由 prepare_apply 验过，查不到就是内部矛盾，不能降级成
            // "这一侧没有答案"
            Some(is_swap_guid(&crate::gpt_policy::live_entry(&g, plan.grow_part)?.partition_type_guid)),
            log,
            post,
        )?;
    }
    // 记录的撤除由调用方的收尾出口决定（`finish_resize`）：FS 步定论且没有任何待办时
    // 才提交并删除。FS 步失败而记录已删时，重跑会按全新作业从旧区重拷，自重叠搬移下
    // 旧区字节已被写覆盖，重拷即把覆盖后的数据盖回新区。任何待办（FS 步或 swap 重建）
    // 都保留记录——重跑按续传口径只重试未完成的部分。
    // 现场记着 LVM 收尾事实而调用方不跑链：保留现场并把未做的那一步作为待办浮出
    if let Some(facts) = &d.lvm
        && !d.defer_ckpt
    {
        let what = match &facts.lv_path {
            Some(lv) => format!("partitions moved and partition {} extended, but pvresize / lvextend for {lv} was not run", plan.grow_part),
            None => format!("partitions moved and partition {} extended, but pvresize was not run", plan.grow_part),
        };
        post.push(Pending::new(
            plan.grow_part,
            PendingKind::Other("lvm chain (pvresize/lvextend)"),
            what,
            "re-run `diskedit resize` on the same target to finish the PV/LV chain (the recovery record holds the recorded extent count)".to_string(),
        ));
    }
    Ok(())
}

/// 扩容收尾的区间对（LBA，表自身 ss）：swap 重建读旧头的区间与探测新空间生效的区间
/// 是两个不同的事实，调用点各给一次
pub(crate) struct GrowthRegions {
    pub(crate) old: (u64, u64), // 搬移前 (start, len)：swap 旧头仍在此处（两族都拒绝搬 swap，start 不变）
    pub(crate) new: (u64, u64), // 提交后 (start, len)：探测按它进行
}

/// 扩容的统一 FS 收尾（apply 与 resize 的唯一实现）：表项已按新几何提交后，按
/// [`grow_target_at`](crate::fsops::grow_target_at) 给出的那段区间与其中的 FS 处置。
/// 未完成的后置条件进写会话 `post`；其余 `Err` 归 io 域。
/// 提交可能来自本次运行，也可能来自上一次续跑：失败保留原变体，越过首写之后的改判
/// 交给退出点的归责——本次没写过时 3/4 仍然成立，它们只陈述本次运行写没写
/// 判据与写盘前的 preflight 是同一处，只是区间取扩容后的那份——两步之间盘被改动时
/// 结论会变，那种情形归后置条件未完成（pending），不算拒绝
/// 两侧对账（[`crate::movepart::reconcile_swap`]）只约束会改写的那一支
pub(crate) fn finalize_growth(
    src: &mut WriteSource,
    part: u32,
    r: GrowthRegions,
    ss: u64,
    declared: Option<bool>,
    log: &mut dyn FnMut(&str),
    post: &mut Vec<Pending>,
) -> Result<(), RunOutcome> {
    // 表已提交：本函数内的失败保留原变体，由退出点按本次写事实归责
    let overflow = || RunOutcome::partial("growth byte offset overflows — refusing");
    let base = r.new.0.checked_mul(ss).ok_or_else(overflow)?;
    let len = r.new.1.checked_mul(ss).ok_or_else(overflow)?;
    let growable = match crate::fsops::grow_target_at(src, part, base, len) {
        Ok(g) => g,
        // 判定本身失败：写盘前那类"请求与现状不符"已不成立，此刻只剩后置条件做不了
        // 与读不出来两种结局
        Err(e) => match RunOutcome::from(e) {
            RunOutcome::Refused(why) => {
                post.push(Pending::new(part, PendingKind::Fs, why, String::new()));
                return Ok(());
            }
            // 读不出来时本次可能没写过：保留原变体，由退出点的归责按写事实改判
            other => return Err(other),
        },
    };
    // 对账只约束会改写的分支（`Target`）：证据侧由 `grow_target_at` 的结论给出，声明侧
    // 由调用方从表条目取。只有一方给出答案时用那一方并把依据写进日志；两方冲突则拒绝——
    // 判据本身与理由见 [`crate::movepart::reconcile_swap`]。`SwapUnactivatable` 不写盘，
    // 按内容判据记待办，不与声明侧对账
    match growable {
        // swap：内容可弃，表项已扩 → mkswap 重建使新空间生效（UUID/卷标保持）。
        // grow_support 对 swap 承诺的就是 mkswap——漏掉它即违背承诺。外部工具写盘，先落屏障
        Growable::Target(t) => {
            let swap = crate::movepart::reconcile_swap(part, declared, Some(t.fstype == ContentKind::Swap))?;
            if let Some(note) = swap.note(part) {
                log(&note);
            }
            if swap.is_swap {
                src.mark_non_reversible()
                    .map_err(|e| RunOutcome::partial(format!("partition {part}: {} — the table entry is committed", e.record_failure("the recovery barrier"))))?;
                let ident = swap_identity_preferring(src, r.old.0, r.new.0, r.old.1, ss);
                match crate::fsops::recreate_swap(src, part, &ident) {
                    Ok(note) => log(&format!("swap recreated ({note})")),
                    Err(e) => post.push(Pending::new(
                        part,
                        PendingKind::Swap,
                        e.to_string(),
                        crate::fsops::rescue_hint(
                            Some(ContentKind::Swap),
                            &crate::dev::part_dev_hint(src, part, r.old.0 * ss),
                            None,
                        ),
                    )),
                }
            } else {
                // 有可扩的一段：装配成一条 `FsResize` 步骤交给执行器——判决与执行同源，
                // 屏障、工具调用与挂载点清理都在那一步里，命令层不再自己调工具
                let plan = crate::assembly::assemble_fs_resize(src, part, None)
                    .map_err(|e| RunOutcome::partial(e.to_string()))?;
                // 记录存在时，FS 步的"退出后替换"要落到记录里。换了 sink
                // 只影响这一件事：没有记录的载体本来就没有可写的地方，此时用空 sink
                let mut sink: Box<dyn crate::execution::StepSink> = if src.record_journal().is_some() {
                    Box::new(crate::recovery::sink::RecordSink::quiet())
                } else {
                    Box::new(crate::execution::NoProgress)
                };
                // FS 步的预扫描可能给出首写前的拒绝：保留原变体，由退出点的归责改判
                crate::execution::execute_plan_complete(src, &plan, None, sink.as_mut())?;
                log("filesystem resized");
            }
        }
        // 走到这里的是写盘前已放行、没有可扩 FS 的那几种：首启 overlay、PV，以及页格式
        // 在本机激活不了的 swap——`unknown` 已由 check_grow_step 拦下，本支至多因盘在两步
        // 之间被改动而见到它。swap 那一类还留着一件**真实的待办**（重建才能用上新空间），
        // 故先探测一遍。
        // 此支不落 ExternalFsTool 屏障：PV 的外部写入（pvresize/lvextend）在收尾
        // resize_done 里发生，屏障由它携带 journal 写句柄在 spawn 前落——"表被回滚、
        // PV 已扩"的自相矛盾同样被排除，且 undo 不再被"屏障已落、pvresize 未跑"的
        // 死亡窗口无谓锁死。本支只探测、不写盘
        g @ (Growable::OverlayPending | Growable::SwapUnactivatable | Growable::NoFilesystem(_)) => {
            match swap_rebuild_pending(src, part, base, len).map_err(RunOutcome::from)? {
                Some(missed) => post.push(missed),
                None => {
                    let p = grow_post(part, &g);
                    log(&p.log);
                    if let Some(item) = p.pending {
                        post.push(item);
                    }
                }
            }
        }
    }
    Ok(())
}

/// 优先取旧起点上的 swap 身份；旧起点取不到 UUID 时回退到新起点。
///
/// 位移小于一个 swap 头跨度时，上一次 `mkswap` 写下的新头可能覆盖旧头区域；
/// 重跑在此读取身份时旧位置已无有效签名，而新头携带同一 UUID。回退只在旧位置
/// 确实没有 UUID 时发生，旧位置有效时仍以它为准
pub(crate) fn swap_identity_preferring(
    src: &FileSource,
    old_start: u64,
    new_start: u64,
    len_lba: u64,
    ss: u64,
) -> crate::fsops::SwapIdentity {
    let ident = read_swap_identity(src, old_start, len_lba, ss);
    if ident.has_uuid() || old_start == new_start {
        return ident;
    }
    let at_new = read_swap_identity(src, new_start, len_lba, ss);
    if at_new.has_uuid() { at_new } else { ident }
}

/// 从 swap 首部读 UUID/卷标（内核 include/linux/swap.h union swap_header：info @1024，
/// sws_uuid@1036、sws_volume@1052）。
///
/// 卷标以**字节**返回：sws_volume 是 16 字节定长字段，mkswap 存入时不校验编码，
/// 任何 String 化（Latin-1 或替换非法序列）都会在写回时改变原值。
/// "没有签名"与"读不出来"必须分开：前者可以照常重建（随机 UUID），后者重建就会
/// 悄悄丢掉 fstab/resume 依赖的 UUID 或卷标
pub(crate) fn read_swap_identity(
    src: &FileSource,
    first_lba: u64,
    len_lba: u64,
    ss: u64,
) -> crate::fsops::SwapIdentity {
    use crate::fsops::{SwapIdentity, SwapLabel};
    let (Some(base), Some(len_bytes)) = (first_lba.checked_mul(ss), len_lba.checked_mul(ss)) else {
        return SwapIdentity::Unreadable("swap header range overflows".into());
    };
    // 签名位置由唯一探测器判定；候选集取 libblkid 口径（含 32K）——本函数只读元数据，
    // 宽于"swapon 可激活"的范围是有意的：读得到就能保住 UUID/PARTUUID。
    match crate::fsid::probe_swap_header(src, base, len_bytes, &crate::fsid::blkid_known_pages()) {
        Ok(Some(_)) => {}
        Ok(None) => return SwapIdentity::NotSwap,
        Err(e) => return SwapIdentity::Unreadable(format!("cannot probe the swap signature: {e}")),
    }
    // UUID/卷标偏移与页大小无关：libblkid struct swap_header_v1_2 中 uuid @1036、volume @1052
    // （与内核 union swap_header 的 info 区同布局，均在第一页内、bootbits 之后）
    let mut uuid = [0u8; 16];
    if let Err(e) = src.read_at(base + 1036, &mut uuid) {
        return SwapIdentity::Unreadable(format!("cannot read the swap UUID: {e}"));
    }
    let mut vol = [0u8; 16];
    let label = match src.read_at(base + 1052, &mut vol) {
        Ok(()) => {
            let vol_bytes: Vec<u8> = vol.iter().take_while(|&&b| b != 0).copied().collect();
            if vol_bytes.is_empty() { SwapLabel::Absent } else { SwapLabel::Present(vol_bytes) }
        }
        Err(e) => SwapLabel::Unreadable(format!("cannot read the swap volume label: {e}")),
    };
    let uuid = (uuid != [0u8; 16]).then_some(uuid);
    SwapIdentity::Known { uuid, label }
}
