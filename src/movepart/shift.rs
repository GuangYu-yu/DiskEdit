//! 搬移/扩容的执行半程：chunk 拷贝、表项提交与扩容收尾。

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
    // 设备 I/O 故障属于另一码事，按故障上抛（调用点都在写盘之后，Failed 的
    // "盘可能已改变"警示才是对状态的如实表述）
    if !crate::fsid::unactivatable_swap(src, base, len_bytes)? {
        return Ok(None);
    }
    Ok(Some(Pending::new(
        part,
        PendingKind::Swap,
        "partition grown but the swap area was not rebuilt (its page format is not activatable on this host)",
        crate::fsops::rescue_hint(Some(FsKind::Swap), &crate::dev::part_dev_hint(src, part, base)),
    )))
}

/// 一次搬移事务 = 事前判定（只读）+ 执行（写盘）。
/// 两半的返回类型不同，这就是 durable boundary 的类型表达：`prepare_apply` 只可能给出
/// `Refused` / `Infra`（此刻确定未写盘），`execute_apply` 的返回类型里没有 `Fail`，
/// 因此"写盘之后仍返回 Refused"在那半边**写不出来**。
/// 几何由调用方解析（一条命令解析一次，往下传），本函数与执行半程都不读表
pub fn apply(
    src: &mut FileSource,
    g: &ValidatedGeometry,
    plan: &Plan,
    chunk_len: u64,
    no_fs: bool,
    log: &mut dyn FnMut(&str),
) -> Outcome {
    // 事务入口的注入点：此刻 abort 必须与"从未运行过"不可区分
    fault_before_any_write();
    let d = match prepare_apply(src, g, plan, chunk_len, no_fs, log) {
        Ok(d) => d,
        Err(f) => return f.into_outcome(),
    };
    // 修复虽是写盘，但先于任何目标写入执行；放在 Fail 域是为了保住分类：
    // 其读表阶段尚未写一个字节（Infra），一旦进入 io 域就只剩 Failed 一条路
    if let Some(what) = plan.repair.describe() {
        if let Err(f) = gpt_policy::apply_repair(src, &plan.repair) {
            return f.into_outcome();
        }
        log(&format!("[repair] {what}"));
    }
    fault_after_repair();
    // 越界之后只剩两条出口：写入完成，或"盘可能已改变"的执行失败
    let mut post = PostWrite::new();
    match execute_apply(src, plan, d, log, &mut post) {
        Ok(()) => post.applied(),
        Err(e) => post.failed(e.to_string()),
    }
}

/// 事前判定的产物，也是 execute 的**唯一输入源**：凡是执行阶段要用的都在这里带过去。
/// `chunk_len` / `no_fs` 若同时留在 execute 的参数位上，同一个值就有两个来源、谁赢没有定义。
/// `g` 随行：执行半程对表项的全部落位都基于它（不重读盘上的表）
pub(crate) struct ApplyDecision {
    pub(crate) g: ValidatedGeometry,
    pub(crate) ckpt: Checkpoint,
    pub(crate) ckpt_path: PathBuf,
    pub(crate) chunk_len: u64,
    pub(crate) no_fs: bool,
}

/// 事前判定（只读）：FS preflight → 槽位判定。
/// 返回 `Err` 一律发生在任何写盘之前；本函数签名上就没有 `&mut FileSource`
pub(crate) fn prepare_apply(
    src: &FileSource,
    g0: &ValidatedGeometry,
    plan: &Plan,
    chunk_len: u64,
    no_fs: bool,
    log: &mut dyn FnMut(&str),
) -> Result<ApplyDecision, Fail> {
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
            .ok_or_else(|| Fail::infra("move source start overflows byte offset"))?)?;
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
        return Err(crate::outcome::Fail::refused(format!(
            "partition {} has an empty entry on disk — the layout moved under the job; refusing",
            plan.grow_part
        )));
    };
    crate::fsops::ensure_idle_before_write(src, plan.grow_part, ge.starting_lba.checked_mul(plan.ss)
        .ok_or_else(|| Fail::infra("grow target start overflows byte offset"))?)?;
    if !no_fs {
        // 要动的是哪一段、里面是什么 FS 由 grow_target_at 判定——与写盘后的收尾同一判据，
        // 两处只是区间不同。区间的 LBA 单位是**表自身**的 ss（plan.ss）。此处位于
        // apply_repair / 搬移之前，本次调用尚未写目标盘：它的环境故障按 Infra 报，
        // "不支持 / 工具缺失 / 里面是什么都不知道"交 FsError 分流（10 / 30），不在此另判
        let base = ge.starting_lba.checked_mul(plan.ss)
            .ok_or_else(|| Fail::infra("grow target start overflows byte offset"))?;
        let len = (ge.ending_lba - ge.starting_lba + 1).checked_mul(plan.ss)
            .ok_or_else(|| Fail::infra("grow target length overflows byte size"))?;
        crate::fsops::check_grow_step(crate::fsops::grow_target_at(src, plan.grow_part, base, len)?, src.is_block)?;
    }
    // 恢复三态：有效 → 续传 / 槽位被另一族作业占用 → 拒绝 / 空 → 新建
    // 落点取命中路径：续传写同一份现场；空槽返回本次命名落点
    let (slot, ckpt_path) = read_checkpoint(src, g0)?;
    let ckpt = match slot {
        CheckpointSlot::Ambiguous(paths) => return Err(ambiguous_checkpoint(&paths)),
        // 与 make_plan_resuming 共用同一句拒绝理由：同一事实的措辞只写一次
        CheckpointSlot::Resize(_) => return Err(foreign_resize_refusal()),
        CheckpointSlot::Relocation(c) => {
            let c = *c;
            // chunk_bytes 单独先查：它是命令行可控项，混在"盘上事实不符"里报会让用户
            // 去查盘，而真实原因是这次换了 --chunk-size
            if c.chunk_bytes != chunk_len {
                return Err(crate::outcome::Fail::refused(format!(
                    "the checkpoint was written with --chunk-size {} MiB, this run uses {} — re-run with the original value",
                    c.chunk_bytes / (1024 * 1024),
                    chunk_len / (1024 * 1024),
                )));
            }
            // 恢复的外部校验（对照 classify_restore 的位置一致性模式）。ckpt 是持久化输入，
            // 而这里的 plan 由 resume_outcome 直接从 ckpt 重建——拿 plan 派生值互相比较是
            // 自循环，盘上几何这一维度根本进不来。真正独立的证据只在盘上：按 cur_index 逐条
            // 核对每个被搬移分区的**当前起点**。已完成的停在 first_lba + delta_lba（表项绝对
            // 赋值，重放幂等），未开始的仍在 first_lba；正在搬的那一条可能已提交表项而 ckpt
            // 尚未推进（commit 与推进之间崩溃），故两个位置都接受。全部完成即终态，逐条落在
            // 目标位。此处不重算 plan——那会破坏"盘上几何已被部分改写"下的续跑语义
            if c.disk_size != src.size || c.ss != g0.sector_size() {
                return Err(crate::outcome::Fail::refused(
                    "existing checkpoint does not match the current disk geometry — refusing",
                ));
            }
            let done = c.cur_index.min(plan.moves.len() as u32);
            for (mi, m) in plan.moves.iter().enumerate() {
                let cur = g0
                    .entry_index(m.part_num)
                    .and_then(|i| g0.entries().get(i))
                    .ok_or_else(|| crate::outcome::Fail::refused(format!(
                        "checkpoint names partition {} that is no longer on the disk — refusing",
                        m.part_num
                    )))?;
                let target = m.first_lba.checked_add(m.delta_lba)
                    .ok_or_else(|| crate::outcome::Fail::infra("relocation target LBA overflows"))?;
                // 端点也要比：只比起点时，成员末端在中断窗口被改小/改大仍算"停在应停处"，
                // 而 ckpt 里的 len_lba 会继续被当成搬移长度与扩容边界
                let src_end = m.first_lba.checked_add(m.len_lba).and_then(|v| v.checked_sub(1))
                    .ok_or_else(|| crate::outcome::Fail::infra("relocation source range overflows"))?;
                let dst_end = target.checked_add(m.len_lba).and_then(|v| v.checked_sub(1))
                    .ok_or_else(|| crate::outcome::Fail::infra("relocation target range overflows"))?;
                let at_src = cur.starting_lba == m.first_lba && cur.ending_lba == src_end;
                let at_dst = cur.starting_lba == target && cur.ending_lba == dst_end;
                let ok = match (mi as u32).cmp(&done) {
                    std::cmp::Ordering::Less => at_dst,
                    std::cmp::Ordering::Equal => at_src || at_dst,
                    std::cmp::Ordering::Greater => at_src,
                };
                if !ok {
                    return Err(crate::outcome::Fail::refused(format!(
                        "partition {} is not where the checkpoint says it should be (on disk LBA {}, expected {}) — the layout moved under the job; refusing",
                        m.part_num, cur.starting_lba, m.first_lba
                    )));
                }
            }
            // 目的区校验：上面的位置比对只覆盖源位置这半个维度。目的区同样是盘上可控
            // 值——中断期间第三方改表（尾部打包的目的区恰在 last_usable 端空闲带，最易
            // 被新建分区占用），或 ckpt 被改写出 CRC 自洽的目的区。与**非本作业**条目
            // 重叠的续跑会把 chunk 拷贝写到别人的分区并提交重叠表项。本作业成员的条目
            // 全部豁免：移位/尾打包 plan 的目的区合法落在其他成员的旧位（右侧先搬让位，
            // 先处理成员表项提交前崩溃的重跑即命中此态），成员现位是否停在应停处已由
            // 上面的位置比对逐条裁决
            let mut dst_ranges = Vec::with_capacity(plan.moves.len());
            for m in &plan.moves {
                let dst = m.first_lba.checked_add(m.delta_lba)
                    .ok_or_else(|| crate::outcome::Fail::infra("relocation target LBA overflows"))?;
                let dst_end = dst.checked_add(m.len_lba).and_then(|v| v.checked_sub(1))
                    .ok_or_else(|| crate::outcome::Fail::infra("relocation target range overflows"))?;
                dst_ranges.push((m.part_num, dst, dst_end));
            }
            for &(pn, dst, dst_end) in &dst_ranges {
                for (ei, e) in g0.entries().iter().enumerate() {
                    // 空条目（0,0）不可能命中：dst 下界 ≥ first_usable ≥ 2
                    let n = ei as u32 + 1;
                    if n == pn || plan.moves.iter().any(|m| m.part_num == n) {
                        continue;
                    }
                    if e.starting_lba <= dst_end && dst <= e.ending_lba {
                        return Err(crate::outcome::Fail::refused(format!(
                            "relocation target of partition {} (LBA {}..{}) overlaps partition {} on disk — the layout moved under the job; refusing",
                            pn, dst, dst_end,
                            ei + 1
                        )));
                    }
                }
            }
            // 目的区两两不相交：重叠的目的区无论执行顺序如何都互相覆写
            for (i, &(apn, ad, ae)) in dst_ranges.iter().enumerate() {
                for &(bpn, bd, be) in dst_ranges.iter().skip(i + 1) {
                    if ad <= be && bd <= ae {
                        return Err(crate::outcome::Fail::refused(format!(
                            "relocation targets of partitions {} and {} overlap (LBA {}..{}) — inconsistent checkpoint; refusing",
                            apn, bpn, ad, ae
                        )));
                    }
                }
            }
            // 顺序不变量：plan.moves 按降序处理（最右侧先搬），故一个目的区可以复写
            // **已处理**成员的旧位，但不得覆盖尚未处理成员（数组更靠后）的源区——
            // 那次拷贝会读到已被本目的区覆写的数据。ckpt 的 CRC 只证明字节自洽，
            // 不证明目的区没被改写，故按盘上重建的 plan 复验
            for (i, &(pn, dst, dst_end)) in dst_ranges.iter().enumerate() {
                for m in plan.moves.iter().skip(i + 1) {
                    let src_end = m.first_lba.checked_add(m.len_lba).and_then(|v| v.checked_sub(1))
                        .ok_or_else(|| crate::outcome::Fail::infra("relocation source range overflows"))?;
                    if dst <= src_end && m.first_lba <= dst_end {
                        return Err(crate::outcome::Fail::refused(format!(
                            "relocation target of partition {} (LBA {}..{}) would overwrite the source of partition {} (LBA {}..{}) before it is copied — inconsistent checkpoint; refusing",
                            pn, dst, dst_end, m.part_num, m.first_lba, src_end
                        )));
                    }
                }
            }
            // 扩容区间同样会写进表项并扩进 FS：中断期间第三方可能在"目标当前末端+1 ..
            // 最左目的区−1"的空闲段新建分区（它与任何目的区都不重叠），照旧扩到
            // grow_end_for 就会写出重叠表并把 FS 扩进该分区。成员豁免：grow_end 恒小于
            // 每个成员的新起点，而新起点不小于其源起点
            let future_grow_end = grow_end_for(plan, ge.starting_lba)?;
            if future_grow_end > ge.ending_lba {
                for (ei, e) in g0.entries().iter().enumerate() {
                    let n = ei as u32 + 1;
                    if n == plan.grow_part || plan.moves.iter().any(|m| m.part_num == n) {
                        continue;
                    }
                    if e.starting_lba <= future_grow_end && e.ending_lba > ge.ending_lba {
                        return Err(crate::outcome::Fail::refused(format!(
                            "the grown partition would extend into partition {} (LBA {}..{}) created since the checkpoint — the layout moved under the job; refusing",
                            n, e.starting_lba, e.ending_lba
                        )));
                    }
                }
            }
            // chunks_done 采信自本次 read_checkpoint，moves 重建自 resume_outcome 的第一次
            // 读——两次读之间文件可能被改写（CRC 自洽），deserialize 的进度上界按文件自身
            // 的 moves 推导，拼接口径下失效。此处按内存里的 plan.moves 重算当前条目的
            // chunk 上界（与搬移循环同一份确定性布局）：越界不是合法运行能产出的进度，
            // 静默 min 会把"跳过未拷贝的 chunk"放行成提交
            let chunk_bound = match plan.moves.get(done as usize) {
                Some(m) => move_chunk_layout(m.len_lba, plan.ss, m.delta_lba, m.len_lba, chunk_len)
                    .map(|(_, count)| count)
                    .ok_or_else(|| crate::outcome::Fail::infra("relocation chunk count overflow"))?,
                None => 0,
            };
            if c.chunks_done > chunk_bound {
                return Err(crate::outcome::Fail::refused(format!(
                    "checkpoint chunk progress {} exceeds the bound {} recomputed from the plan — the checkpoint changed between reads; refusing",
                    c.chunks_done, chunk_bound
                )));
            }
            // Y = durable 恢复点（ckpt 文件里的值），不是本进程内存进度
            log(&format!("resuming at entry {} chunk {} (durable checkpoint)", c.cur_index, c.chunks_done));
            Checkpoint {
                disk_size: src.size,
                ss: plan.ss,
                grow_part: plan.grow_part,
                last_usable_lba: plan.last_usable_lba,
                moves: plan.moves.clone(),
                kind: plan.kind,
                cur_index: done,
                chunks_done: c.chunks_done,
                chunk_bytes: chunk_len,
                fp: src.fingerprint,
                map: src.loop_mapping,
            }
        }
        CheckpointSlot::Empty => {
            // 被搬移的 swap 条目在表项提交后由 recreate_swap 重建（mkswap；镜像目标的重建
            // 对象是 loop 分区设备，还要 losetup），且不受 --no-fs 约束。新跑必须现在确认
            // 平台与工具可用：否则会先把表改完，再以 pending 收场。恢复分支不套用这条——
            // 已有 relocation ckpt 是已经开始的作业，缺工具只能在执行期以 pending 收场并
            // 保留现场，不能反过来阻断恢复。预检只覆盖平台与工具存在性：PATH 里有可执行
            // 文件不等于它一定能成功操作目标（权限、设备策略），那些失败仍落在执行期
            if plan.moves.iter().any(|m| m.is_swap) {
                crate::fsops::require_linux()
                    .map_err(|e| crate::outcome::Fail::refused(format!("cannot rebuild swap: {e}")))?;
                crate::fsops::check_support(
                    "rebuild",
                    crate::fsid::FsKind::Swap.as_str(),
                    crate::fsops::swap_rebuild_support(src.is_block),
                )
                .map_err(|e| crate::outcome::Fail::refused(e.to_string()))?;
            }
            Checkpoint {
                disk_size: src.size,
                ss: plan.ss,
                grow_part: plan.grow_part,
                last_usable_lba: plan.last_usable_lba,
                moves: plan.moves.clone(),
                kind: plan.kind,
                cur_index: 0,
                chunks_done: 0,
                chunk_bytes: chunk_len,
                fp: src.fingerprint,
                map: src.loop_mapping,
            }
        }
    };
    Ok(ApplyDecision { g: g0.clone(), ckpt, ckpt_path, chunk_len, no_fs })
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

pub(crate) const RESUME_HINT: &str = "the checkpoint is kept, re-run the command to retry this block (transient errors resume from here); for a permanently unreadable block, image the source with ddrescue and run the operation on the image";

pub(crate) const COPY_HINT: &str = "copy has no resume — release the scene with `diskedit undo` (if rollbackable) or `diskedit abandon`, then re-run; a permanently unreadable block must be imaged with ddrescue first";

// ---- durable boundary：以上判定全部结束，以下开始写盘 ----

/// 执行（写盘）。入参 `&mut FileSource` + 返回 `io::Result` 共同构成边界：
/// 函数体内**构造不出** `Fail::Refused`（返回类型里放不下它），
/// 于是"写盘之后还能返回 Refused"在这里连编译都过不去
pub(crate) fn execute_apply(
    src: &mut FileSource,
    plan: &Plan,
    d: ApplyDecision,
    log: &mut dyn FnMut(&str),
    post: &mut PostWrite,
) -> io::Result<()> {
    // 决策按值收下：ckpt 就地推进；表项落位全部基于随行几何（不重读盘上的表——
    // 搬移循环内每条重解析一次正是"解析次数随 N 增长"的根源）。条目绝对赋值 +
    // 同一份几何连续提交，与逐条重读盘上表再提交逐字节等价（commit 幂等）
    let mut ckpt = d.ckpt;
    let (chunk_len, no_fs) = (d.chunk_len, d.no_fs);
    let ckpt_path = &d.ckpt_path;
    let mut g = d.g;
    // 修复已由 apply 的 Fail 域在调用本函数之前完成（备份头搬移 / 保护 MBR 重写），
    // 使后续所有写入都基于修复后的几何
    atomic_write_ckpt(ckpt_path, &ckpt.serialize())?;
    // 本操作含**不可回滚的写入**：搬移的数据块按设计不入 journal（前向恢复、无回滚），
    // 故留一条标记使 undo 拒绝回滚——否则表被回滚而数据未回滚，布局与盘上内容自相矛盾。
    // delta_lba 在 plan 生成处只产生非负值：目标起点 = first_lba + delta，直接 checked 相加
    let effective = plan.moves.iter().find(|m| m.delta_lba != 0);
    if let Some(m) = effective {
        let to_lba = m.first_lba.checked_add(m.delta_lba).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "relocation target LBA overflows — refusing")
        })?;
        src.set_mutation(crate::dev::Mutation::RelocatePartition {
            partition: m.part_num,
            from_lba: m.first_lba,
            to_lba,
        });
        // 位移非零 ⇒ 存在不可回滚的写入。位移全为 0（右邻已尾打包时重跑 resize
        // grow --allow-move 的典型产物）没有这样的写入，不落屏障：undo 的可回滚性
        // 不受这次空转影响
        src.mark_non_reversible()?;
    }

    // 恢复从最近一次 durable checkpoint 继续；batching（CKPT_BATCH_CHUNKS）只决定
    // checkpoint 的提交频率，即崩溃后最多重做多少个已完成但未持久化的 chunk。
    // chunk 级进度只对"中断时正在搬的那一条"有效（后续条目从头计数）：自重叠搬移
    // （delta < 分区长度）重跑已完成 chunk 会读到被自己覆写过的源数据，chunk 级续传正是为此设计
    let resume_index = ckpt.cur_index;
    let resume_chunks = ckpt.chunks_done;
    // 搬移循环：单分区 = 单事务；尾→头推进
    for mi in (ckpt.cur_index as usize)..plan.moves.len() {
        let m = &plan.moves[mi];
        // δ=0：条目已在目标位（右邻尾打包后重跑的产物）。自拷贝、重写表、推进 ckpt
        // 之外的一切都是空转，跳过——表项绝对赋值下它本来就"已提交"
        if m.delta_lba == 0 {
            ckpt.chunks_done = 0;
            ckpt.cur_index = mi as u32 + 1;
            atomic_write_ckpt(ckpt_path, &ckpt.serialize())?;
            continue;
        }
        // 中断时正在搬的条目号随本次 ckpt 序列化落盘，恢复侧据此定位 chunk 级进度
        ckpt.cur_index = mi as u32;
        // 目标区端点一次算好、全链 checked：要原样写进表项并进入字节地址换算
        let new_first = m.first_lba.checked_add(m.delta_lba).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "relocation target LBA overflows — refusing")
        })?;
        let new_end = m.len_lba.checked_sub(1).and_then(|l| new_first.checked_add(l)).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "relocation target LBA overflows — refusing")
        })?;
        // durable 起点：只有"中断时正在搬的那一条"带 chunk 级进度，后续条目从 0 开始
        let entry_resume = if mi as u32 == resume_index { resume_chunks } else { 0 };
        ckpt.chunks_done = entry_resume;
        // swap：内容可弃 → 不搬数据，表项落位后 mkswap 重建（UUID/卷标保持）
        if m.is_swap {
            // 类型复核：`is_swap` 来自 ckpt（当初的类型判定），锁下这份盘上表才是现状。
            // 两次运行之间该分区被改成别的类型时，按旧判定 mkswap 会把一个已不属于
            // swap 的分区覆写掉。类型读不出来同样按"不再成立"拒绝，绝不用无知当许可
            let still_swap = match crate::gpt_policy::live_entry(&g, m.part_num) {
                Ok(e) => is_swap_guid(&e.partition_type_guid),
                Err(_) => false,
            };
            if !still_swap {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "partition {} is not a swap partition on disk any more (its type changed since the checkpoint) — \
                         refusing to recreate it there; release the scene with `diskedit abandon`",
                        m.part_num
                    ),
                ));
            }
            // PARTUUID/分区号由条目原样保留（unique guid 不动），只改位置。
            // 绝对赋值而非 += delta：commit 后、ckpt 更新前崩溃的重放幂等
            g.update_entry((m.part_num - 1) as usize, |e| {
                e.starting_lba = new_first;
                e.ending_lba = new_end;
            })?;
            g.commit(src)?;
            fault_after_swap_commit(mi);
            // 新起点此刻只在盘上的表里：内核视图若还停在旧位置，按新起点找不到分区节点，
            // mkswap 会以"表已提交"的状态失败成待办
            if !crate::support::kernel_resync(src) {
                log("note: the kernel partition view did not follow the table write — the swap rebuild may need a re-run after the kernel re-reads the table");
            }
            // 旧 swap 签名区在源位置（数据区未搬移），从那里读 UUID/卷标
            let ident = swap_identity_preferring(src, m.first_lba, new_first, m.len_lba, plan.ss);
            let had_uuid = ident.0.is_some();
            // mkswap 是外部工具的写盘，先落屏障（对齐 execute_resize 的 swap 分支）
            src.set_mutation(crate::dev::Mutation::ExternalFsTool);
            src.mark_non_reversible()?;
            match crate::fsops::recreate_swap(src, m.part_num, ident) {
                Ok(()) if had_uuid => log(&format!("swap {} recreated at new location (UUID preserved)", m.part_num)),
                Ok(()) => log(&format!("swap {} recreated at new location (old signature had no UUID — mkswap wrote a fresh one)", m.part_num)),
                // 表项已落位、内容可弃：swap 未重建属"后续步骤未完成"，不是本次操作失败。
                // 不推进 cur_index、就地停止条目循环，续跑从本条目重试 mkswap（表项提交与
                // identity 读取幂等，旧签名仍在源位）。推进会让重跑的位置比对要求"后续
                // 条目仍在源位"而自相矛盾；跳过待办则让重跑把未重建的 swap 洗成成功
                Err(e) => {
                    post.pending(Pending::new(
                        m.part_num,
                        PendingKind::Swap,
                        e.to_string(),
                        // hint 指向表项已提交的新位置（只有读旧签名区取 identity 才用旧起点）
                        crate::fsops::rescue_hint(Some(FsKind::Swap), &crate::dev::part_dev_hint(src, m.part_num, new_first * plan.ss)),
                    ));
                    ckpt.chunks_done = 0;
                    atomic_write_ckpt(ckpt_path, &ckpt.serialize())?;
                    fault_after_swap_entry(mi);
                    break;
                }
            }
            ckpt.chunks_done = 0;
            ckpt.cur_index = mi as u32 + 1;
            atomic_write_ckpt(ckpt_path, &ckpt.serialize())?;
            fault_after_swap_entry(mi);
            continue;
        }
        let overflow = || io::Error::new(io::ErrorKind::InvalidData, "relocation byte offset overflows — refusing");
        let src_off = m.first_lba.checked_mul(plan.ss).ok_or_else(overflow)?;
        let dst_off = new_first.checked_mul(plan.ss).ok_or_else(overflow)?;
        let total = m.len_lba.checked_mul(plan.ss).ok_or_else(overflow)?;
        // 自重叠搬移（位移小于分区长度）下，重做 chunk 的源区间会被先落盘的更靠尾
        // chunk 的写覆盖，除非每个 chunk 不大于位移（写区间起点 ≥ 任何重做源区间终点）。
        // 因此 chunk 收敛到位移字节以内，且停用批量提交——批量攒下的未持久化 chunk
        // 同样会探入重做区间。非自重叠搬移写区间与源区间不相交，两种优化照常可用。
        // 块布局取自唯一实现，与 checkpoint 的进度上界同源
        let (entry_chunk, _) = move_chunk_layout(m.len_lba, plan.ss, m.delta_lba, m.len_lba, chunk_len)
            .ok_or_else(overflow)?;
        // 分派判据是**几何**自重叠（位移小于分区长度），不是"块大小是否真被收敛"：
        // 请求的 chunk 本来就不大于位移时布局已经安全，但 durable 边界仍逐 chunk 推进
        // 并由下方的批量开关决定是否攒批——两种优化都只在非自重叠时可用
        let overlap = m.delta_lba < m.len_lba;
        if overlap && entry_chunk < chunk_len {
            log(&format!(
                "partition {}: self-overlapping move, chunk size reduced to {} bytes to keep redo reads intact",
                m.part_num, entry_chunk
            ));
        }
        let chunk_from_tail: Vec<(u64, u64)> = {
            // (offset_within, len) 从尾部向头部
            let mut v = Vec::new();
            let mut pos = total;
            while pos > 0 {
                let len = entry_chunk.min(pos);
                v.push((pos - len, len));
                pos -= len;
            }
            v
        };
        let total_chunks = chunk_from_tail.len() as u64;
        // volatile 进度：已完成但尚未 checkpoint 的 chunk 数；durable 边界始终只在 ckpt 文件里
        let mut pending_chunks = 0u64;
        for (i, (within, len)) in chunk_from_tail.iter().enumerate() {
            if (i as u64) < entry_resume {
                continue; // durable 边界之前视为完成：恢复只认 ckpt 值，绝不用内存进度跳过
            }
            let mut buf = vec![0u8; *len as usize];
            src.read_at(src_off + within, &mut buf).map_err(|e| {
                chunk_io_error(e, "reading source", (src_off + within) / plan.ss, len / plan.ss, i + 1, total_chunks as usize, RESUME_HINT)
            })?;
            src.write_data_at(dst_off + within, &buf).map_err(|e| {
                chunk_io_error(e, "writing target", (dst_off + within) / plan.ss, len / plan.ss, i + 1, total_chunks as usize, RESUME_HINT)
            })?;
            src.sync_data()?; // 数据 chunk 用 sync_data；表结构提交用 sync_all
            ckpt.chunks_done = i as u64 + 1; // 内存进度：递增发生在 sync_data 之后，永不超前于 durable 数据
            pending_chunks += 1;
            // 自重叠搬移：durable 边界逐 chunk 推进（写区间探入后续重做源区间的窗口
            // 只能靠收紧 checkpoint 频率关死）；非自重叠照常攒批
            if overlap || pending_chunks >= CKPT_BATCH_CHUNKS || i as u64 + 1 == total_chunks {
                atomic_write_ckpt(ckpt_path, &ckpt.serialize())?;
                pending_chunks = 0;
            }
            fault_chunk(i as u64 + 1);
        }
        fault_before_entry_commit();
        // 本分区完成 → 按崩溃安全四结构序列提交表项（整表重写）。
        // 绝对赋值（非 += delta）：commit 后 ckpt 更新前崩溃的重放幂等
        g.update_entry((m.part_num - 1) as usize, |e| {
            e.starting_lba = new_first;
            e.ending_lba = new_end;
        })?;
        g.commit(src)?;
        fault_before_hidden_fix();
        // 起始位置变化的 NTFS 分区需修 HiddenSectors（数据是字节拷贝，boot sector 带着旧值）
        if crate::fsid::identify(src, dst_off, total)? == Some(FsKind::Ntfs) {
            fix_ntfs_hidden_sectors(src, new_first, plan.ss, log)?;
        }
        log(&format!("partition {} relocated (delta {} sectors)", m.part_num, m.delta_lba));
        ckpt.chunks_done = 0;
        ckpt.cur_index = mi as u32 + 1;
        atomic_write_ckpt(ckpt_path, &ckpt.serialize())?;
        fault_after_entry_commit(mi);
    }
    // 条目循环未走完（swap 重建失败就地 break）：扩容收尾不能做——grow_end_for 按
    // 全体 moves 取 min(new_first)−1，会把尚未搬移成员的旧位也划进扩容区，提交即写出
    // 与未搬移分区重叠的表项。扩容留给续跑：重跑在同一 ckpt 上从本条目重试 mkswap，
    // 条目走完后才到达这里
    if (ckpt.cur_index as usize) < plan.moves.len() {
        log("relocation incomplete (swap rebuild pending) — growth not applied, re-run the command to continue");
        return Ok(());
    }
    fault_before_grow();

    // 扩容收尾：目标分区扩到 movable 新区域之前（防止与刚打包的分区重叠）；
    // 无 movable 时到 last_usable。commit → resize FS（同一份随行几何，不重读盘上的表）
    let grow_idx = (plan.grow_part - 1) as usize;
    let grow_start = g.entries()[grow_idx].starting_lba;
    let grow_end = grow_end_for(plan, grow_start)?;
    let old_len = g.entries()[grow_idx].ending_lba.checked_sub(grow_start)
        .map(|v| v + 1)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "grow target entry inverted — refusing"))?;
    g.update_entry(grow_idx, |te| te.ending_lba = grow_end)?;
    let grow_len = grow_end - grow_start + 1;
    g.commit(src)?;
    table::ensure_protective_mbr(src)?;
    log(&format!("partition {} extended (FS area ends before relocated partitions)", plan.grow_part));
    // 扩到设备末端的 FS 工具（resize2fs 一类）以内核报告的尺寸为准：内核视图停在旧尺寸时
    // 它们会认为无事可做、以 0 退出，把"分区已扩、FS 未扩"洗成成功。重读失败时不跑 FS 步，
    // 把扩容记成待办
    let kernel_followed = crate::support::kernel_resync(src);

    // FS resize 是契约的一部分：失败即后置条件未满足（由调用方换算为 PARTIAL）。
    // 这里不能只打日志当成功——脚本会据此认为空间已可用
    if no_fs {
        log("partition extended (--no-fs: filesystem left untouched)");
    } else if !kernel_followed {
        post.pending(Pending::new(
            plan.grow_part,
            PendingKind::Fs,
            "the kernel partition view still reports the old size, so the filesystem was not resized — refresh the view (partprobe / partx) and re-run to finish",
            String::new(),
        ));
    } else {
        finalize_growth(src, plan.grow_part, GrowthRegions {
            old: (grow_start, old_len),
            new: (grow_start, grow_len),
        }, g.sector_size(), log, post)?;
    }
    // ckpt 在 FS 步定论后才撤：FS 步失败而 ckpt 已删时，重跑会按全新作业从旧区重拷，
    // 自重叠搬移下旧区字节已被写覆盖，重拷即把覆盖后的数据盖回新区。任何待办（FS 步
    // 或 swap 重建）都保留 ckpt——重跑按续传口径只重试未完成的部分
    if post.is_empty() {
        crate::dev::warn_if_remove_failed(ckpt_path);
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
/// 未完成的后置条件进写会话 `post`；其余 `Err` 归 io 域
/// （此刻已过 durable boundary，不得再伪装成拒绝）。
/// 判据与写盘前的 preflight 是同一处，只是区间取扩容后的那份——两步之间盘被改动时
/// 结论会变，那种情形归后置条件未完成（pending），不算拒绝
pub(crate) fn finalize_growth(
    src: &mut FileSource,
    part: u32,
    r: GrowthRegions,
    ss: u64,
    log: &mut dyn FnMut(&str),
    post: &mut PostWrite,
) -> io::Result<()> {
    let (base, len) = (r.new.0 * ss, r.new.1 * ss);
    match crate::fsops::grow_target_at(src, part, base, len) {
        // 走到这里的是写盘前已放行、没有可扩 FS 的那几种：首启 overlay、PV，以及页格式
        // 在本机激活不了的 swap——`unknown` 已由 check_grow_step 拦下，本支至多因盘在两步
        // 之间被改动而见到它。swap 那一类还留着一件**真实的待办**（重建才能用上新空间），
        // 故先探测一遍。
        // 此支不落 ExternalFsTool 屏障：PV 的外部写入（pvresize/lvextend）在收尾
        // resize_done 里发生，屏障由它携带 journal 写句柄在 spawn 前落——"表被回滚、
        // PV 已扩"的自相矛盾同样被排除，且 undo 不再被"屏障已落、pvresize 未跑"的
        // 死亡窗口无谓锁死。本支只探测、不写盘
        Ok(g @ (Growable::OverlayPending | Growable::SwapUnactivatable | Growable::NoFilesystem(_))) => {
            match swap_rebuild_pending(src, part, base, len)? {
                Some(missed) => post.pending(missed),
                // 这句是 FS 步的实情，不是对区域内容的断言：`unknown` 同时涵盖空区域与
                // 认不出的类型，PV 的空间则在收尾的 pvresize 链里生效
                None => log(match g {
                    // 只说这句实情：内层没认出文件系统（未格式化 / 认不出的格式 / 字节不足），
                    // 不替它断言是哪一种
                    Growable::OverlayPending => "partition extended (no filesystem was identified in the overlay RW layer)",
                    Growable::NoFilesystem(Some(FsKind::Lvm2Pv)) => "partition extended (the PV space takes effect through the pvresize chain)",
                    _ => "partition extended (filesystem not resized — its type was not identified)",
                }),
            }
        }
        // swap：内容可弃，表项已扩 → mkswap 重建使新空间生效（UUID/卷标保持）。
        // grow_support 对 swap 承诺的就是 mkswap——漏掉它即违背承诺。外部工具写盘，先落屏障
        Ok(Growable::Target(t)) if t.fstype == FsKind::Swap => {
            src.set_mutation(crate::dev::Mutation::ExternalFsTool);
            src.mark_non_reversible()?;
            let ident = swap_identity_preferring(src, r.old.0, r.new.0, r.old.1, ss);
            let had_uuid = ident.0.is_some();
            match crate::fsops::recreate_swap(src, part, ident) {
                Ok(()) if had_uuid => log("swap recreated (UUID preserved)"),
                Ok(()) => log("swap recreated (old signature had no UUID — mkswap wrote a fresh one)"),
                Err(e) => post.pending(Pending::new(
                    part,
                    PendingKind::Swap,
                    e.to_string(),
                    crate::fsops::rescue_hint(Some(FsKind::Swap), &crate::dev::part_dev_hint(src, part, r.old.0 * ss)),
                )),
            }
        }
        // 外部 FS 工具（resize2fs/xfs_growfs/…）的写入不可回滚：扩完后 FS 自述尺寸大于
        // 旧分区尺寸，此后回滚表项即"表与内容自相矛盾"。必须先于该写入落屏障
        Ok(Growable::Target(t)) => {
            src.set_mutation(crate::dev::Mutation::ExternalFsTool);
            src.mark_non_reversible()?;
            match t.resize_fs(src) {
                Ok(leak) => {
                    log("filesystem resized");
                    // 扩容已成功，卸载的成败是另一条后置条件：它不改写成功这一事实，
                    // 只作为一条待办随结果浮出
                    if let Some(leak) = leak {
                        post.pending(leak.into_pending(part));
                    }
                }
                Err(e) => post.pending(Pending::new(
                    part,
                    PendingKind::Fs,
                    e.to_string(),
                    crate::fsops::rescue_hint(Some(t.fstype), &crate::dev::part_dev_hint(src, part, r.old.0 * ss)),
                )),
            }
        }
        // 判定本身失败：写盘前那类"请求与现状不符"已不成立，此刻只剩两条出路——
        // 后置条件做不了（pending，脚本据此知道空间还没可用）与读不出来（io 域，报失败）
        Err(e) => match crate::outcome::Fail::from(e) {
            crate::outcome::Fail::Refused(why) => post.pending(Pending::new(part, PendingKind::Fs, why, String::new())),
            crate::outcome::Fail::Infra(cause) | crate::outcome::Fail::Failed(cause) => {
                return Err(io::Error::other(cause))
            }
        },
    }
    Ok(())
}

/// 尽力读取附加信息（如卷标）：失败只意味着该项缺失，不影响正确性
#[allow(clippy::let_underscore_must_use)]
pub(crate) fn best_effort_read(src: &FileSource, off: u64, buf: &mut [u8]) {
    let _ = src.read_at(off, buf);
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
) -> (Option<[u8; 16]>, Option<Vec<u8>>) {
    let ident = read_swap_identity(src, old_start, len_lba, ss);
    if ident.0.is_some() || old_start == new_start {
        return ident;
    }
    let at_new = read_swap_identity(src, new_start, len_lba, ss);
    if at_new.0.is_some() { at_new } else { ident }
}

/// 从 swap 首部读 UUID/卷标（内核 include/linux/swap.h union swap_header：info @1024，
/// sws_uuid@1036、sws_volume@1052）。非 swap 签名或读取失败 → None（mkswap 生成随机 UUID）
///
/// 卷标以**字节**返回：sws_volume 是 16 字节定长字段，mkswap 存入时不校验编码，
/// 任何 String 化（Latin-1 或替换非法序列）都会在写回时改变原值
pub(crate) fn read_swap_identity(src: &FileSource, first_lba: u64, len_lba: u64, ss: u64) -> (Option<[u8; 16]>, Option<Vec<u8>>) {
    let base = first_lba * ss;
    // 签名位置由唯一探测器判定；候选集取 libblkid 口径（含 32K）——本函数只读元数据，
    // 宽于"swapon 可激活"的范围是有意的：读得到就能保住 UUID/PARTUUID。
    // 探测读失败与未命中同出口：收尾时的尽力读取，保不住就按随机 UUID 走
    if !matches!(
        crate::fsid::probe_swap_header(src, base, len_lba * ss, &crate::fsid::blkid_known_pages()),
        Ok(Some(_))
    ) {
        return (None, None); // 未格式化、非 swap、或读不出来 → 随机 UUID
    }
    // UUID/卷标偏移与页大小无关：libblkid struct swap_header_v1_2 中 uuid @1036、volume @1052
    // （与内核 union swap_header 的 info 区同布局，均在第一页内、bootbits 之后）
    let mut uuid = [0u8; 16];
    let mut vol = [0u8; 16];
    if src.read_at(base + 1036, &mut uuid).is_err() {
        return (None, None);
    }
    // 卷标是附加信息：读失败只意味着重建后没有卷标（swap 通常按 UUID 挂载），
    // 不影响可用性；UUID 读失败已在上方显式返回 None，由 mkswap 生成新值
    best_effort_read(src, base + 1052, &mut vol);
    let vol_bytes: Vec<u8> = vol.iter().take_while(|&&b| b != 0).copied().collect();
    let uuid_opt = if uuid == [0u8; 16] { None } else { Some(uuid) };
    let vol_opt = if vol_bytes.is_empty() { None } else { Some(vol_bytes) };
    (uuid_opt, vol_opt)
}