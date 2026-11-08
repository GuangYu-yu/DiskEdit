//! `resize --allow-move` 的判定半程与收尾：事前判定（只读）、绑定现场、扩容后的 FS 收尾。
//!
//! 写盘不在本模块：搬移、表提交与换区重建都由 [`crate::execution::execute_plan`]
//! 执行，本模块把判定结论装配成一份计划并交给它。持久状态是恢复记录里的
//! [`crate::recovery::Relocation`]：有序计划、双游标、块字节、作业语义与几何守卫
//! 同属一份记录，与 `resize` 的分区层/FS/LVM 收尾共用一个事务号。

use super::*;
use crate::planner::chunk::move_chunk_layout;

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
        crate::fsops::rescue_hint(Some(ContentKind::Swap), &crate::dev::part_dev_hint(src, part, base)),
    )))
}

/// 一次搬移事务 = 事前判定（只读）+ 执行（写盘）。
/// 判定的返回类型里只有 `Refused` / `NoMutation`（此刻确定未写盘）；执行半程交给
/// [`crate::execution::execute_plan`]，它的错误分类按"首写之前 / 之后"给出，本函数不再改写。
/// 几何由调用方解析（一条命令解析一次，往下传），本函数与执行半程都不读表
#[allow(clippy::too_many_arguments)] // src/几何/计划/chunk/开关/推迟收尾/LV 选择/日志各司其职：合成参数对象只是给 lint 换一层间接
pub fn apply(
    src: &mut WriteSource,
    g: &ValidatedGeometry,
    plan: &Plan,
    chunk_len: u64,
    no_fs: bool,
    defer_ckpt: bool,
    lvm: Option<LvmSelection>,
    log: &mut dyn FnMut(&str),
) -> RunOutcome {
    // 事务入口的注入点：此刻 abort 必须与"从未运行过"不可区分
    fault_before_any_write();
    // 记录里的搬移状态是续跑的唯一依据；读两份并持有期结束后再进入判定
    let (existing_relocation, existing_lvm) = match src.record_journal() {
        Some(j) => (j.relocation().cloned(), j.lvm_post()),
        None => (None, None),
    };
    let d = match prepare_apply(
        src,
        g,
        plan,
        chunk_len,
        no_fs,
        defer_ckpt,
        lvm,
        existing_relocation.as_ref(),
        existing_lvm,
        log,
    ) {
        Ok(d) => d,
        Err(f) => return f,
    };
    // 修复虽是写盘，但先于任何目标写入执行；放在 RunOutcome 域是为了保住分类：
    // 其读表阶段尚未写一个字节（NoMutation），一旦进入 io 域就只剩 Partial 一条路
    if let Some(what) = plan.repair.describe() {
        if let Err(f) = gpt_policy::apply_repair(src, &plan.repair) {
            return f;
        }
        log(&format!("[repair] {what}"));
    }
    fault_after_repair();
    // 越界之后只剩两条出口：写入完成，或"盘可能已改变"的执行失败
    let mut post: Vec<Pending> = Vec::new();
    match execute_apply(src, plan, d, log, &mut post) {
        Ok(()) => RunOutcome::applied_with(post),
        // 分类由执行器给：首写之前的拒绝仍是 3/4，首写之后才是 5。失败自带的待办与
        // 本进程已攒下的待办合并，一个都不丢
        Err(RunOutcome::Partial { cause, pending }) => {
            RunOutcome::partial_with(cause, pending.into_iter().chain(post).collect())
        }
        // 待办只由写盘之后的步骤产生：非空即已越过首写，"确定未写盘"的原变体不再成立
        Err(other) if !post.is_empty() => RunOutcome::partial_with(other.to_string(), post),
        Err(other) => other,
    }
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
            // chunk_bytes 单独先查：它是命令行可控项，混在"盘上事实不符"里报会让用户
            // 去查盘，而这一项的直接原因是本次换了 --chunk
            if c.chunk_bytes != chunk_len {
                return Err(crate::outcome::RunOutcome::refused(format!(
                    "the recovery record was written with --chunk {} MiB, this run uses {} — re-run with the original value",
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
                return Err(crate::outcome::RunOutcome::refused(
                    "the recovery record does not match the current disk geometry — refusing",
                ));
            }
            // 与下方 chunks_in_entry 的越界拒绝同一口径：entry_index 超出计划条目数
            // 是记录被改写或两处进度字段口径分叉的信号，夹紧只会把异常放行成
            // 「全部完成」——条目数内才允许停在任意位置
            if c.entry_index > plan.moves.len() as u32 {
                return Err(crate::outcome::RunOutcome::refused(format!(
                    "recovery record entry progress {} exceeds the plan's {} entries — the record changed between reads; refusing",
                    c.entry_index, plan.moves.len()
                )));
            }
            let done = c.entry_index;
            for (mi, m) in plan.moves.iter().enumerate() {
                let cur = g0
                    .entry_index(m.part_num)
                    .and_then(|i| g0.entries().get(i))
                    .ok_or_else(|| crate::outcome::RunOutcome::refused(format!(
                        "the recovery record names partition {} that is no longer on the disk — refusing",
                        m.part_num
                    )))?;
                let target = m.first_lba.checked_add(m.delta_lba)
                    .ok_or_else(|| crate::outcome::RunOutcome::no_mutation("relocation target LBA overflows"))?;
                // 端点也要比：只比起点时，成员末端在中断窗口被改小/改大仍算"停在应停处"，
                // 而 ckpt 里的 len_lba 会继续被当成搬移长度与扩容边界
                let src_end = m.first_lba.checked_add(m.len_lba).and_then(|v| v.checked_sub(1))
                    .ok_or_else(|| crate::outcome::RunOutcome::no_mutation("relocation source range overflows"))?;
                let dst_end = target.checked_add(m.len_lba).and_then(|v| v.checked_sub(1))
                    .ok_or_else(|| crate::outcome::RunOutcome::no_mutation("relocation target range overflows"))?;
                let at_src = cur.starting_lba == m.first_lba && cur.ending_lba == src_end;
                let at_dst = cur.starting_lba == target && cur.ending_lba == dst_end;
                let ok = match (mi as u32).cmp(&done) {
                    std::cmp::Ordering::Less => at_dst,
                    std::cmp::Ordering::Equal => at_src || at_dst,
                    std::cmp::Ordering::Greater => at_src,
                };
                if !ok {
                    return Err(crate::outcome::RunOutcome::refused(format!(
                        "partition {} is not where the recovery record says it should be (on disk LBA {}, expected {}) — the layout moved under the job; refusing",
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
                    .ok_or_else(|| crate::outcome::RunOutcome::no_mutation("relocation target LBA overflows"))?;
                let dst_end = dst.checked_add(m.len_lba).and_then(|v| v.checked_sub(1))
                    .ok_or_else(|| crate::outcome::RunOutcome::no_mutation("relocation target range overflows"))?;
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
                        return Err(crate::outcome::RunOutcome::refused(format!(
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
                        return Err(crate::outcome::RunOutcome::refused(format!(
                            "relocation targets of partitions {} and {} overlap (LBA {}..{}) — inconsistent recovery record; refusing",
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
                        .ok_or_else(|| crate::outcome::RunOutcome::no_mutation("relocation source range overflows"))?;
                    if dst <= src_end && m.first_lba <= dst_end {
                        return Err(crate::outcome::RunOutcome::refused(format!(
                            "relocation target of partition {} (LBA {}..{}) would overwrite the source of partition {} (LBA {}..{}) before it is copied — inconsistent recovery record; refusing",
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
            // 缩方向同样要拒：中断期间第三方可以把目标扩到 grow_end 之外，照 ckpt 收尾
            // 就会把表项缩回去，而 FS 已按更大的分区扩过——尾部数据变得不可达
            if future_grow_end < ge.ending_lba {
                return Err(crate::outcome::RunOutcome::refused(format!(
                    "partition {} already ends at LBA {}, past the recovery record's grow target {} — \
                     finishing would shrink it below a filesystem that may already fill it; refusing",
                    plan.grow_part, ge.ending_lba, future_grow_end
                )));
            }
            if future_grow_end > ge.ending_lba {
                for (ei, e) in g0.entries().iter().enumerate() {
                    let n = ei as u32 + 1;
                    if n == plan.grow_part || plan.moves.iter().any(|m| m.part_num == n) {
                        continue;
                    }
                    if e.starting_lba <= future_grow_end && e.ending_lba > ge.ending_lba {
                        return Err(crate::outcome::RunOutcome::refused(format!(
                            "the grown partition would extend into partition {} (LBA {}..{}) created since the record was written — the layout moved under the job; refusing",
                            n, e.starting_lba, e.ending_lba
                        )));
                    }
                }
            }
            // chunks_done 采信自本次记录读取，moves 重建自 resume_outcome 的第一次
            // 读——两次读之间文件可能被改写（CRC 自洽），deserialize 的进度上界按文件自身
            // 的 moves 推导，拼接口径下失效。此处按内存里的 plan.moves 重算当前条目的
            // chunk 上界（与搬移循环同一份确定性布局）：越界不是合法运行能产出的进度，
            // 静默 min 会把"跳过未拷贝的 chunk"放行成提交
            let chunk_bound = match plan.moves.get(done as usize) {
                Some(m) => move_chunk_layout(m.len_lba, plan.ss, m.delta_lba, m.len_lba, chunk_len)
                    .map(|(_, count)| count)
                    .ok_or_else(|| crate::outcome::RunOutcome::no_mutation("relocation chunk count overflow"))?,
                None => 0,
            };
            if c.chunks_in_entry > chunk_bound {
                return Err(crate::outcome::RunOutcome::refused(format!(
                    "recovery record chunk progress {} exceeds the bound {} recomputed from the plan — the record changed between reads; refusing",
                    c.chunks_in_entry, chunk_bound
                )));
            }
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
                // 工具缺失与平台不符都归 Unsupported(6)：分类走 outcome 的唯一映射点，
                // 不在这里把变体压成字符串再统一折成"改参数重试有意义"
                crate::fsops::require_linux().map_err(crate::outcome::RunOutcome::from)?;
                crate::fsops::check_support(
                    "rebuild",
                    crate::fsid::ContentKind::Swap.as_str(),
                    crate::fsops::swap_rebuild_support(src.is_block),
                )
                .map_err(crate::outcome::RunOutcome::from)?;
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
fn bind_relocation(
    src: &mut WriteSource,
    reloc: &crate::recovery::Relocation,
    lvm: Option<&LvmPostFacts>,
) -> io::Result<()> {
    record_journal(src)?.bind_relocation(reloc.clone(), lvm.map(LvmPostFacts::to_record))
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
                .checked_add(m.delta_lba)
                .map(|dst| (m.part_num, dst))
                .ok_or_else(|| RunOutcome::no_mutation("relocation target LBA overflows"))
        })
        .collect::<Result<_, _>>()?;
    let confirmed = crate::assembly::assemble_resize_move_plan(
        src,
        &mut g,
        &moves,
        (plan.grow_part, grow_end),
    )?;

    // 现场先绑：执行器从记录里的条目游标决定从哪继续，记录里没有搬移计划时它拒绝推进游标
    bind_relocation(src, &d.relocation, d.lvm.as_ref()).map_err(|e| {
        RunOutcome::no_mutation(format!("the relocation plan was not recorded ({e})"))
    })?;
    record_journal(src)?.bind_old_range(d.old_range).map_err(|e| {
        RunOutcome::no_mutation(format!("the pre-commit range was not recorded ({e})"))
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
/// 未完成的后置条件进写会话 `post`；其余 `Err` 归 io 域
/// （此刻已过 durable boundary，不得再伪装成拒绝）。
/// 判据与写盘前的 preflight 是同一处，只是区间取扩容后的那份——两步之间盘被改动时
/// 结论会变，那种情形归后置条件未完成（pending），不算拒绝
pub(crate) fn finalize_growth(
    src: &mut WriteSource,
    part: u32,
    r: GrowthRegions,
    ss: u64,
    log: &mut dyn FnMut(&str),
    post: &mut Vec<Pending>,
) -> Result<(), RunOutcome> {
    // 表已提交：本函数内的失败都只能按"盘可能已改变"上报
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
            // 表已经提交过：读不出来只能按"盘可能已改变"上报
            other => return Err(other.past_boundary()),
        },
    };
    match growable {
        // swap：内容可弃，表项已扩 → mkswap 重建使新空间生效（UUID/卷标保持）。
        // grow_support 对 swap 承诺的就是 mkswap——漏掉它即违背承诺。外部工具写盘，先落屏障
        Growable::Target(t) if t.fstype == ContentKind::Swap => {
            src.mark_non_reversible()
                .map_err(|e| RunOutcome::partial(format!("partition {part}: cannot persist the recovery barrier before rebuilding the swap area: {e}")))?;
            let ident = swap_identity_preferring(src, r.old.0, r.new.0, r.old.1, ss);
            match crate::fsops::recreate_swap(src, part, &ident) {
                Ok(note) => log(&format!("swap recreated ({note})")),
                Err(e) => post.push(Pending::new(
                    part,
                    PendingKind::Swap,
                    e.to_string(),
                    crate::fsops::rescue_hint(Some(ContentKind::Swap), &crate::dev::part_dev_hint(src, part, r.old.0 * ss)),
                )),
            }
        }
        // 有可扩的一段：装配成一条 `FsResize` 步骤交给执行器——判决与执行同源，
        // 屏障、工具调用与挂载点清理都在那一步里，命令层不再自己调工具
        Growable::Target(_) => {
            let plan = crate::assembly::assemble_fs_resize(src, part, None)
                .map_err(|e| RunOutcome::partial(e.to_string()))?;
            // 记录存在时，FS 步的"退出后替换"要落到记录里。换了 sink
            // 只影响这一件事：没有记录的载体本来就没有可写的地方，此时用空 sink
            let mut sink: Box<dyn crate::execution::StepSink> = if src.record_journal().is_some() {
                Box::new(crate::recovery::sink::RecordSink::quiet())
            } else {
                Box::new(crate::execution::NoProgress)
            };
            crate::execution::execute_plan_complete(src, &plan, None, sink.as_mut())
                // FS 步的预扫描可能给出首写前的拒绝：表已提交，这里同样只能报 5
                .map_err(RunOutcome::past_boundary)?;
            log("filesystem resized");
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
