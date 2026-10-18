//! undo：回放 journal，撤销本工具的直接写入（分区表与搬移前的原字节）。

use crate::support::*;
use crate::args::Args;
use crate::dev::{classify_journal, FileSource, Journal, JournalCandidate, JournalRead, JournalRecord, RecoveryData};

pub(crate) const HELP: &str = r#"diskedit undo <TARGET> --yes

  Replay the journal to undo the partition-table bytes this tool wrote.
  Relocated data cannot be rolled back, and writes made by external FS
  tools are not undone."#;

/// 挑不出唯一可读 journal 的两种原因——**性质不同，出口码就该不同**，故在这里
/// 分型而不是把文案揉成一个 String 让调用方猜：
/// - `Unreadable`：文件在而读不出来（CRC/magic/损坏/I-O）⇒ 盘上事实，重试无用（30）；
///   只有 abandon 能释放，提示也指向那里
/// - `Ambiguous`：多份同时可读 ⇒ 不猜（猜错会把历史字节回放到不该回放的盘上）
#[derive(Debug)]
enum PickJournalError {
    Unreadable { detail: String },
    Ambiguous { listed: String },
}

impl std::fmt::Display for PickJournalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PickJournalError::Unreadable { detail } => write!(f, "the undo journal exists but is unreadable: {detail} — \
                 it cannot be replayed; release it with `diskedit abandon`"),
            PickJournalError::Ambiguous { listed } => write!(f, "multiple journals found for this target — refusing: {listed}"),
        }
    }
}

/// 候选落点的挑选结论。缺席与空壳都不是"可回放的记录"，但该给出的结论不同：
/// 前者是请求与现状不符，后者描述零次写入、清理即可
enum PickedJournal {
    Absent,
    /// 头完整、零记录的壳（可能有多个落点）
    Shells(Vec<std::path::PathBuf>),
    Records(std::path::PathBuf, JournalRead),
}

/// 在候选落点中挑出唯一可读的 journal；没有可读记录时区分"空壳"与"什么都没有"。
/// 候选列表按"本次命名在前、历史命名在后"给出，缺席是常态而非故障：把它记成错误会让
/// 真正的损伤原因（另一份文件存在但读不出来）被 `No such file or directory` 盖住。
/// 损坏候选**记录后继续**看后面的候选（与 read_checkpoint 同一口径）：一份损坏的历史
/// 命名不该遮住一份完好的本次命名；全部候选都不可读时才把死因如实带回。
///
/// 分类只走 [`classify_journal`]：写闸口、`abandon` 与这里对"什么算现场"因此不会分叉。
/// 有损坏候选时不把"只剩空壳"报成干净——那份损坏的可能正是丢了记录的真现场
fn pick_journal(candidates: &[std::path::PathBuf]) -> Result<PickedJournal, PickJournalError> {
    let mut usable: Vec<(std::path::PathBuf, JournalRead)> = Vec::new();
    let mut shells: Vec<std::path::PathBuf> = Vec::new();
    let mut damaged: Vec<String> = Vec::new();
    for path in candidates {
        match classify_journal(path) {
            JournalCandidate::Absent => {}
            JournalCandidate::Shell => shells.push(path.clone()),
            JournalCandidate::Damaged(detail) => damaged.push(format!("{} ({detail})", path.display())),
            JournalCandidate::Records(read) => usable.push((path.clone(), read)),
        }
    }
    match usable.len() {
        1 => {
            for d in &damaged {
                eprintln!("warning: unreadable journal {d}");
            }
            let (p, r) = usable.remove(0);
            Ok(PickedJournal::Records(p, r))
        }
        0 if !damaged.is_empty() => Err(PickJournalError::Unreadable { detail: damaged.join("; ") }),
        0 if !shells.is_empty() => Ok(PickedJournal::Shells(shells)),
        0 => Ok(PickedJournal::Absent),
        _ => Err(PickJournalError::Ambiguous {
            listed: usable.iter().map(|(p, _)| p.display().to_string()).collect::<Vec<_>>().join(", "),
        }),
    }
}

/// checkpoint 是否还在。判据与写闸口同源：非普通文件不是现场；只有 `NotFound` 算不在，
/// 属性读不出来按存在处理——否则磁盘不可读时会把"还有未收尾作业"报成"目标干净"
fn checkpoint_present(p: &std::path::Path) -> bool {
    match std::fs::metadata(p) {
        Ok(m) => m.is_file(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
        Err(_) => true,
    }
}

/// 空壳（头完整、零记录）描述的是零次写入：没有任何东西依赖它，但它一直占着候选落点，
/// 而写闸口与 `abandon` 都按设计跳过它——不在这里收掉就没有命令能收。
///
/// 只删**确认属于本目标**的壳：头里的身份与本目标不符即拒绝，绝不按"读出来是空的"就删。
/// 全部核验完再动手，避免删掉一个之后才在第二个上拒绝而留下半清理状态
fn remove_empty_shells(src: &FileSource, shells: Vec<std::path::PathBuf>) -> ! {
    // 空壳与"目标上留着未收尾的 checkpoint"是两件事：后者描述的是另一族作业的进度
    // （例如搬移搬到一半），undo 回放不了它，也不能假装目标干净——它会让后续 resize
    // 被判成 Divergent 而拒绝，用户看到的是"什么也没做却被拒绝"
    let leftover = src.identity.checkpoint_candidates(legacy_disk_guid(src));
    let stale: Vec<String> = leftover.iter().filter(|p| checkpoint_present(p)).map(|p| p.display().to_string()).collect();
    if !stale.is_empty() {
        bail_fail(Fail::refused(format!(
            "the undo journal is empty, but this target still has an unfinished checkpoint ({}); undo cannot release it — \
             re-run the command that started that job to resume it to completion",
            stale.join(", ")
        )));
    }
    for p in &shells {
        let identity = Journal::read_identity(p)
            .unwrap_or_else(|e| bail_fail(Fail::infra(format!("journal identity header is unreadable: {e}"))));
        if let Some(why) = crate::dev::scene_mismatch(&identity.fp, &identity.map, &src.fingerprint, &src.loop_mapping) {
            bail_fail(Fail::refused(format!(
                "{why} — this empty journal is not removed here; {ROLLBACK_STUCK_REMEDY}"
            )));
        }
    }
    let mut removed = 0;
    for p in &shells {
        // NotFound 的后置条件（不残留）已经成立；成功删除同时落盘目录项
        if crate::dev::remove_recovery_file(p) {
            removed += 1;
        }
    }
    bail_fail(Fail::refused(format!(
        "nothing to undo ({removed} empty journal shell(s) removed; they held no records)"
    )))
}

/// 单条记录的分叉核对结论。三态而非 bool：两个失败方向的**出口码不同**——盘上字节
/// 对不上是"请求与现状不符"（这次回滚不该做），读不出来是"盘读不动"（做不了）；
/// 压成一个 bool 会让调用方用同一句话报出两种性质不同的故障
enum EntryCheck {
    /// 盘上当前字节等于写后内容（该写入未被回滚），或等于原文（上一次 undo 中断留下的
    /// 进度——重放同一原文幂等无害）
    Ok,
    /// 两者都不是：该写入之后有外部改动动过同一段字节（journal 不挡第三方），
    /// 回放会把它覆盖掉
    Mismatch(String),
    /// 这段区间读不出来，无从核对——不得回放
    Unreadable(String),
}

fn check_replay_entry(src: &FileSource, rec: &JournalRecord) -> EntryCheck {
    let RecoveryData::PreImage { off, bytes, after } = &rec.recovery else {
        // Barrier 型已被整体拒绝（见 cmd_undo 的 find_map），此臂只作纵深防御
        return EntryCheck::Ok;
    };
    let mut now = vec![0u8; bytes.len()];
    match src.read_at(*off, &mut now) {
        Ok(()) if now == *after || now == *bytes => EntryCheck::Ok,
        Ok(_) => EntryCheck::Mismatch(format!(
            "target no longer matches the journaled write at offset {off} — the disk changed outside this tool \
             (or an interrupted write by this tool itself) after that write; \
             replaying would overwrite the foreign edit"
        )),
        Err(e) => EntryCheck::Unreadable(format!(
            "cannot verify the journaled write at offset {off} against the target: {e} — refusing to replay over an unverifiable region"
        )),
    }
}

/// 写盘前的干跑：按真实回放的顺序（逆序）在**内存里的虚拟盘面**上逐条核对并改写。
/// 全部通过 ⇒ 回放不会在回滚到一半时才发现盘面不对；任一条不过 ⇒ 拒绝发生在第一个
/// 字节写回之前。
///
/// 必须走虚拟盘面，不能逐条去对真盘：同一偏移被写多次时，先写那条的写后内容已被
/// 后写那条覆盖，只有把后者的回滚结果留在虚拟盘面上，前者才核得上——这与真实回放的
/// 顺序完全一致（先回滚后写的，再核先写的）
///
/// 虚拟盘面按起始偏移索引、只在长度相等时复用。不同长度走真盘回读的分支今天是**死路**：
/// PreImage 的唯一生产者是 `FileSource::write_at`，pre-image 长度恒等于当次写入长度，
/// 而生产调用点只两类——整扇区写（MBR/GPT，偏移按扇区对齐、长度恒为扇区大小或同一份
/// 表数组）与 NTFS HiddenSectors 的 4 字节写（偏移 ≡ 0x1C mod 扇区）。起始偏移决定了
/// 调用点类别，故同一偏移的两条记录长度必相同。失效条件：将来新增一类 journaled 局部
/// 写，其起始偏移与既有类别重合而长度不同——届时这里会退化为"多读一次真盘"的误拒
/// （fail-closed，一字未写），并应把索引改成 (offset, length) 双键
fn verify_replay_plan(src: &FileSource, entries: &[JournalRecord]) -> Result<(), Fail> {
    let mut staged: std::collections::HashMap<u64, Vec<u8>> = std::collections::HashMap::new();
    for rec in entries.iter().rev() {
        let RecoveryData::PreImage { off, bytes, after } = &rec.recovery else { continue };
        // 该偏移若已被本轮更晚的记录改写，取虚拟盘面上的值；否则从真盘读一次
        let now = match staged.get(off) {
            Some(v) if v.len() == bytes.len() => v.clone(),
            _ => {
                let mut buf = vec![0u8; bytes.len()];
                src.read_at(*off, &mut buf).map_err(|e| {
                    Fail::infra(format!(
                        "cannot verify the journaled write at offset {off} against the target: {e} — \
                         refusing to replay over an unverifiable region; {ROLLBACK_STUCK_REMEDY}"
                    ))
                })?;
                buf
            }
        };
        if now != *after && now != *bytes {
            return Err(Fail::refused(format!(
                "target no longer matches the journaled write at offset {off} — the disk changed outside \
                 this tool (or an interrupted write by this tool itself) after that write; \
                 replaying would overwrite the foreign edit; {ROLLBACK_STUCK_REMEDY}"
            )));
        }
        staged.insert(*off, bytes.clone());
    }
    Ok(())
}

/// 逐条核对失败后共用的出路：两种成因（盘上字节被外部改动、目标被缩短到记录之外）下
/// "重跑 undo"都要等目标回到那条写入时的状态之后才可能成功。确实不再需要回滚时，
/// `abandon` 释放现场——它不写目标一个字节，放弃的是"还能回滚"这个能力
const ROLLBACK_STUCK_REMEDY: &str =
    "restore the target to the state it was in, then re-run `undo`; if the rollback is no longer \
     wanted, release it with `diskedit abandon`";

/// checkpoint 的历史命名带 GPT Disk GUID，而 GUID 只在表可读时存在。undo 属恢复路径：
/// 表读不出来时要照常工作（该候选缺席即可），故一切失败都降级为 None——判据与
/// `abandon` 完全同源，共用 `legacy_disk_guid`（含降级告警），两处不各写一遍
pub(crate) fn cmd_undo(a: &Args) -> u8 {
    // undo 回放的是整盘 journal：`:N` 指定了也会被静默忽略（与 info 同判据）
    if let Some(n) = a.part {
        bail_fail(Fail::refused(format!("`undo` replays the whole target's journal — drop :{n}")));
    }
    // 回放类命令的载体必须是整盘：journal 记的是整盘偏移，落到分区节点上会把
    // 别处的字节写进该分区
    refuse_partition_node_target("undo", &a.target);
    if !a.yes {
        bail_fail(Fail::refused("`undo` overwrites current bytes from journal; pass --yes to confirm"));
    } else {
        let mut src = open_target_owned(a).unwrap_or_else(|f| bail_fail(f));
        // 回放写回的是整盘表字节：目标仍在被使用时回放，会让内核分区视图与盘面
        // 从"分叉"变成"按历史布局分叉"。闸门在任何写回与 journal 处理之前
        if let Err(f) = crate::fsops::ensure_table_write_idle(&src) {
            bail_fail(f);
        }
        // 三种挑不出各自的出口码：缺席 = 请求与现状不符（10）；在而读不出 = 盘上事实，
        // 重试无用且只有 abandon 能释放（30）；歧义同样按拒绝处理（不猜）
        let picked = pick_journal(src.identity.journal_candidates()).unwrap_or_else(|e| bail_fail(match e {
            PickJournalError::Unreadable { .. } => Fail::infra(e.to_string()),
            _ => Fail::refused(e.to_string()),
        }));
        let (p, entries, tail_incomplete) = match picked {
            PickedJournal::Absent => bail_fail(Fail::refused("no undo journal on this target".to_string())),
            PickedJournal::Shells(shells) => remove_empty_shells(&src, shells),
            PickedJournal::Records(p, read) => match read {
                JournalRead::Complete(v) => (p, v, false),
                // 尾部未完成的记录：append-only 下那次追加没走完，它对应的写入也就没发生，
                // 丢弃安全；前面的完整前缀照常回放（严格契约仍守：每条都过了 CRC）
                JournalRead::TruncatedTail(v) => (p, v, true),
            },
        };
        // 这份 journal 是写给**哪个目标**的：v4 起头里记着创建时的指纹与 loop 映射。
        // 不符即拒绝，且发生在任何写回之前——把别的目标记下的原文回放到本目标上，是这条
        // 路径最坏的后果。它与下面按记录字节的整体核算是两道不同判据：那一道证明"这些字节
        // 与盘上现状相符"，这一道证明"这份历史属于这个目标"，两者都要过
        let identity = Journal::read_identity(&p)
            .unwrap_or_else(|e| bail_fail(Fail::infra(format!("journal identity header is unreadable: {e}"))));
        if let Some(why) =
            crate::dev::scene_mismatch(&identity.fp, &identity.map, &src.fingerprint, &src.loop_mapping)
        {
            // why 里已经带着"记的是什么、现在是什么"的逐字段差异；这里补的是出路——
            // 与下方逐条核对失败同一句话：把目标恢复成记录时的样子再重跑，或不再需要回滚时 abandon
            bail_fail(Fail::refused(format!("{why} — this journal is not replayed here; {ROLLBACK_STUCK_REMEDY}")));
        }
        // 身份保障的边界要说给用户听：这个载体一个可比字段都给不出时（真块设备节点、以及
        // 任何指纹恒零的载体），上面那道核对无从下手——字段没有证据就不参与比较，"没比"
        // 不能表述成"已验证"。回放实际依靠的是"落点由身份键派生"加下面逐条记录的字节核对。
        // 有尺寸证据（非 Unix 平台的镜像、以及任何元数据长度可读的载体）时不在此列：那道
        // 核对确实跑了，只是覆盖的是尺寸这一项
        if !src.fingerprint.has_any_evidence() {
            eprintln!(
                "note: identity boundary — this target exposes none of the fingerprint fields (device id, inode, \
                 size), so none of them was compared; no evidence is not a match, and the replay rests on the \
                 identity-derived journal location plus the per-record byte check"
            );
        }
        let n = entries.len();
        if tail_incomplete {
            eprintln!(
                "warning: the journal ends with an incomplete record (an interrupted append, or that record was damaged) — \
                 replaying the {n} complete record(s) before it; anything recorded after that point cannot be undone"
            );
        }
        // 含**不可回滚写入**的 journal 不可回滚：数据搬移的字节、外部 FS 工具的写入
        // （resize2fs/mkswap/mkfs…）与内核侧表写入都按设计不入 journal，只回滚表项会留下
        // 表与盘上内容自相矛盾的布局（例如 FS 自述尺寸 > 分区尺寸），故显式拒绝而非假回滚。
        // 屏障记着是哪一类越过了这条线，理由因此可以直接说给用户听
        if let Some(m) = entries.iter().find_map(|r| match r.recovery {
            RecoveryData::Barrier => Some(r.mutation),
            RecoveryData::PreImage { .. } => None,
        }) {
            bail_fail(Fail::refused(format!(
                // 只陈述 undo 做不到什么、以及谁做得到。"重跑原命令续跑"不能写在这里：
                // 并非所有不可回滚的场景都有 ckpt 可续（`copy` 就没有），那句判断
                // 属于 transaction 的出路分流（见 `TransactionManager::busy`）
                "this journal records a non-reversible mutation ({}), so rolling back only the table would leave the layout \
                 contradicting the on-disk content; restore from backup, or release the transaction with `diskedit abandon`",
                m.describe()
            )));
        }
        // 整体核对先于任何写回：这份 journal 若不是写给当前盘面的，回滚必须在
        // 第一个字节落地之前被拒（判据见 verify_replay_plan）
        verify_replay_plan(&src, &entries).unwrap_or_else(|f| bail_fail(f));
        // 已写回的条目数：核对失败时的退出码按它分流——refused/infra 的契约是"确定
        // 未写盘"，部分回滚已落盘后按它报会让脚本误判盘面未动（写失败臂同据归 Failed）
        let mut replayed: usize = 0;
        for rec in entries.iter().rev() {
            // Barrier 已在前置整体拒绝（上方 find_map），此臂只作纵深防御——真有漏网
            // 的 Barrier，跳过比回放更接近"不假装回滚了"的安全缺省
            let RecoveryData::PreImage { off, bytes, .. } = &rec.recovery else { continue };
            // 纵深：整体核对与本条写入之间，盘面仍可能被外部改动（journal 不挡第三方）
            match check_replay_entry(&src, rec) {
                EntryCheck::Ok => {}
                EntryCheck::Mismatch(detail) => {
                    if replayed == 0 {
                        bail_fail(Fail::refused(format!(
                            "{detail} (release the transaction with `diskedit abandon` if the rollback is no longer wanted)"
                        )));
                    }
                    bail_fail(Fail::failed(format!(
                        "{detail}; {replayed} journal entr{} already rolled back, journal kept — {} \
                         (verify with: diskedit info {})",
                        if replayed == 1 { "y" } else { "ies" },
                        ROLLBACK_STUCK_REMEDY,
                        a.target
                    )));
                }
                EntryCheck::Unreadable(detail) => {
                    if replayed == 0 {
                        bail_fail(Fail::infra(detail));
                    }
                    bail_fail(Fail::failed(format!(
                        "{detail}; {replayed} journal entr{} already rolled back, journal kept — {} \
                         (verify with: diskedit info {})",
                        if replayed == 1 { "y" } else { "ies" },
                        ROLLBACK_STUCK_REMEDY,
                        a.target
                    )));
                }
            }
            if let Err(e) = src.write_at(*off, bytes) {
                // journal 保留在原地：可重试 undo。回放进行到一半才失败——前面的条目
                // 已经写入，"确定未写盘"的断言不成立，归 Failed（可能已改变）
                bail_fail(Fail::failed(format!("undo write failed at offset {off}: {e} (journal kept, retry)")));
            }
            // 逐条落盘再进下一条：掉电重试时，已回滚条目在盘上稳定停在原文（通过核对），
            // 进度精确到条；整场回放攒到最后一次 sync 的话，一次掉电可撕裂任意多条，
            // 撕裂字节与外部改动无法区分，重试只能被拒
            if let Err(e) = src.sync_data() {
                bail_fail(Fail::failed(format!("undo sync failed at offset {off}: {e} (journal kept, retry)")));
            }
            replayed += 1;
        }
        // undo 的契约是"盘确定回到写入前状态"：sync 失败意味着回滚可能未落盘，
        // 不能报成功——那会让用户以为已经回滚。回放本身已完成，但持久性无法断言，
        // 同样只承诺"可能已改变，需验证"
        src.sync_all().unwrap_or_else(|e| {
            bail_fail(Fail::failed(format!("undo wrote the journal back but sync failed: {e} — rollback may not be durable, verify before retrying")))
        });
        // 回放成功的收尾不止删被回放的那份：历史命名候选若还留在盘上（读不出的那份，
        // pick_journal 已逐条告警过），下一次 undo 仍会把它报成 Unreadable、abandon
        // 仍把它列为现场——回放既然已经成功，这个"有事没做完"的表象必须一并消失。
        // Barrier 型 journal 走不到这里（上方已整体拒绝），Ambiguous 也在挑 journal
        // 时前置拒绝，这里能多删的只有历史候选
        clear_journal_candidates(src.identity.journal_candidates(), &p);
        // 事务的恢复状态随回滚一并释放：这份 journal 记下的表写入已经全部回退，而 checkpoint
        // 描述的是同一个事务的进度——留下来只会描述一个已被回滚掉的世界，让后续 resize 被判成
        // Divergent 而永久拒绝（"什么也没做却被拒绝"）。
        //
        // 这里能无条件删掉**全部** ckpt 候选，依据两条前提；改动它们必须重新审这一处：
        // ① 写 ckpt 只能经 `TransactionManager::begin_or_resume` 的三个 claim 进入，而目标上
        //    已有"无 ckpt 的 journal"时三者都拒绝（FreshOnly 见任何 active 即拒、AnyCheckpoint
        //    要求 ckpt 已存在、OwnRelocation 的归属判据同样要求 ckpt 已存在），因此与某份
        //    journal 并存的 ckpt 必属同一个作业；
        // ② 此处删的候选集与 `active_recovery_records` 枚举现场用的是同一组（journal_candidates
        //    + checkpoint_candidates(legacy_disk_guid)），既不会漏删、也不会删到别处的落点。
        // 新增绕过 begin_or_resume 的 ckpt 写入点，或新增接受非空 active 的入口，即推翻本结论
        for ckpt in src.identity.checkpoint_candidates(legacy_disk_guid(&src)) {
            crate::dev::remove_recovery_file(&ckpt);
        }
        // undo 不开撤销窗口（WriteNoJournal）：结果只用于自身退出码
        table_write_done(&src, &format!("undone {n} journal entries (verify with: diskedit info {})", a.target)).exit_code()
    }
}

/// 回放成功后清掉**全部** journal 候选，不止被回放的那份 `picked`。被回放的那份
/// 无条件删除（使命已结束）；其余候选只有以本工具 magic 开头才删——那说明它是
/// 我们自己写下的残卷（pick_journal 口径的"读不出"，损伤已逐条告警过），留着会让
/// 下一次 undo 报 Unreadable、abandon 仍列为现场。陌生内容不删：候选路径按命名
/// 派生只说明"可能是我们的落点"，证明不了"是我们的文件"——`Journal::open` 拒绝
/// 采纳陌生文件，这里同判据保留并告警，交 abandon 人工处置
fn clear_journal_candidates(candidates: &[std::path::PathBuf], picked: &std::path::Path) {
    for cand in candidates {
        if cand == picked {
            crate::dev::remove_recovery_file(cand);
            continue;
        }
        // 读得出但首部不是本工具 magic ⇒ 陌生文件，保留交人工处置；
        // 读不出来是另一回事（打不开 / 读不满 magic 长）——那不是"内容不像我们的"，
        // 报成后者会把用户引去查一个并不存在的陌生文件
        let probe = std::fs::File::open(cand).and_then(|mut f| {
            use std::io::Read;
            let mut hdr = [0u8; Journal::MAGIC.len()];
            f.read_exact(&mut hdr)?;
            Ok(hdr == *Journal::MAGIC)
        });
        match probe {
            Ok(true) => {
                eprintln!("removing stale journal candidate {}", cand.display());
                crate::dev::remove_recovery_file(cand);
            }
            Ok(false) => {
                eprintln!(
                    "warning: {} does not start with a diskedit journal header — left in place; \
                     release it with `diskedit abandon` if it is ours",
                    cand.display()
                );
            }
            Err(e) => {
                eprintln!(
                    "warning: {} could not be read to check for a diskedit journal header ({e}) — left in place",
                    cand.display()
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::let_underscore_must_use)] // 清理临时文件有意忽略失败
    use super::{pick_journal, verify_replay_plan, PickJournalError};
    use crate::dev::{FileSource, Journal, JournalRead};
    use std::path::PathBuf;

    /// 与身份派生同源：镜像的 journal 就是它的兄弟文件（后缀与 dev.rs 的词表一致）
    fn journal_path(img: &std::path::Path) -> PathBuf {
        let mut p = img.to_path_buf().into_os_string();
        p.push(".diskedit.journal");
        PathBuf::from(p)
    }

    /// 测试用 journal 身份：`open_src` 造出的目标指纹是全零（"取不到稳定身份"那一类），
    /// 这里也记全零——两侧都无证据时身份核对不参与，测试因此聚焦在回放本身。
    /// 身份不符的拒绝路径在 CLI 层测：`cmd_undo` 的失败出口是 process::exit，
    /// 单元测试里跑不出"拒绝之后还能继续断言"的形态
    fn jid() -> crate::dev::JournalIdentity {
        crate::dev::JournalIdentity { fp: Default::default(), map: None }
    }

    fn open_src(img: &std::path::Path) -> FileSource {
        let file = std::fs::OpenOptions::new().read(true).write(true).open(img).unwrap();
        let size = std::fs::metadata(img).unwrap().len();
        FileSource {
            identity: crate::dev::TargetIdentity::resolve_image(img),
            file,
            path: img.to_path_buf(),
            sector_size: 512,
            size,
            is_block: false,
            journal: None,
            ownership: None,
            fingerprint: Default::default(),
            loop_mapping: None,
        }
    }

    fn tmp_path(tag: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("diskedit_undo_{tag}_{}.journal", std::process::id()));
        let _ = std::fs::remove_file(&p);
        p
    }

    /// 四态挑选：空壳只在没有任何可读记录时成为结论；损坏候选不否决唯一可用的记录，
    /// 也不会让"只剩空壳"被报成干净
    #[test]
    fn pick_journal_separates_shells_damaged_and_records() {
        let absent = tmp_path("pj_absent");
        assert!(matches!(pick_journal(&[absent]).unwrap(), super::PickedJournal::Absent));

        let shell = tmp_path("pj_shell");
        std::fs::write(&shell, b"").unwrap();
        drop(Journal::open(&shell, jid()).unwrap());
        assert!(matches!(
            pick_journal(std::slice::from_ref(&shell)).unwrap(),
            super::PickedJournal::Shells(v) if v == vec![shell.clone()]
        ));

        let good = tmp_path("pj_good");
        {
            let mut j = Journal::open(&good, jid()).unwrap();
            j.record(0, &[0xAA; 4], &[0xBB; 4]).unwrap();
        }
        assert!(matches!(
            pick_journal(std::slice::from_ref(&good)).unwrap(),
            super::PickedJournal::Records(p, _) if p == good
        ));

        // 一份可用 + 一份读不出来：按可用那份回放（损坏候选只告警）
        let bad = tmp_path("pj_bad");
        std::fs::write(&bad, b"DEJ").unwrap();
        assert!(matches!(
            pick_journal(&[good.clone(), bad.clone()]).unwrap(),
            super::PickedJournal::Records(p, _) if p == good
        ));

        // 一份可用 + 一份空壳：只按可用那份回放，空壳留在原地（回放成功后由
        // clear_journal_candidates 清掉候选，不在这里顺手删）
        let shell2 = tmp_path("pj_shell2");
        std::fs::write(&shell2, b"").unwrap();
        drop(Journal::open(&shell2, jid()).unwrap());
        assert!(matches!(
            pick_journal(&[good.clone(), shell2.clone()]).unwrap(),
            super::PickedJournal::Records(p, _) if p == good
        ));
        assert!(good.exists(), "a usable journal must be left for the replay");
        assert!(shell2.exists(), "picking the records must not touch the shell");

        // 两份可用：不猜
        let good2 = tmp_path("pj_good2");
        {
            let mut j = Journal::open(&good2, jid()).unwrap();
            j.record(0, &[0xCC; 4], &[0xDD; 4]).unwrap();
        }
        assert!(matches!(
            pick_journal(&[good.clone(), good2.clone()]),
            Err(PickJournalError::Ambiguous { .. })
        ));

        // 只剩空壳 + 一份读不出来的：先报读不出来，不能把目标报成干净
        assert!(matches!(
            pick_journal(&[shell.clone(), bad.clone()]),
            Err(PickJournalError::Unreadable { .. })
        ));

        for p in [shell, shell2, good, good2, bad] {
            let _ = std::fs::remove_file(p);
        }
    }

    /// 上次 undo 中断的现场（部分条目已回滚、其余未动）必须能续跑：核对接受两种
    /// 历史——写后内容（未动）与原文（自己上次的回滚进度）。只认写后内容会把
    /// "journal kept, retry" 的承诺落空：中断的回滚永远续不上，只能 abandon 成
    /// 半回滚混合态。注意 cmd_undo 的失败出口是 process::exit，本测试只覆盖成功路径
    #[test]
    fn interrupted_undo_replay_resumes_over_its_own_rollback() {
        let img = tmp_path("resume_img");
        let mut jpath = img.clone().into_os_string();
        jpath.push(".diskedit.journal");
        let journal = std::path::PathBuf::from(jpath);
        let _ = std::fs::remove_file(&journal);
        {
            let mut j = Journal::open(&journal, jid()).unwrap();
            j.record(0, &[0xAA; 4], &[0xBB; 4]).unwrap();
            j.record(100, &[0xCC; 4], &[0xDD; 4]).unwrap();
        }
        // 逆序回放中断后的盘面：offset 100 已回滚（进度条），offset 0 未动
        let mut disk = vec![0u8; 1024];
        disk[0..4].copy_from_slice(&[0xBB; 4]);
        disk[100..104].copy_from_slice(&[0xCC; 4]);
        std::fs::write(&img, &disk).unwrap();

        let a = crate::args::Args {
            target: img.to_string_lossy().into_owned(),
            part: None, grow: None, start: None, end: None, size: None,
            fs: None, name: None, clone: false, type_guid: None, table: None,
            yes: true, online: false, random: false, sector_size: None,
            align: String::new(), chunk_mib: 0, grow_to_end: false,
            allow_move: false, no_fs: false, grow_lv: false, start_end: false, lv: None,
            pos: Vec::new(), seen: Vec::new(),
        };
        let code = super::cmd_undo(&a);
        assert_eq!(code, 0, "undo must resume over its own interrupted progress");
        let now = std::fs::read(&img).unwrap();
        assert_eq!(&now[0..4], &[0xAA; 4], "the untouched entry must be rolled back");
        assert_eq!(&now[100..104], &[0xCC; 4], "the rolled-back entry stays at the original bytes");
        assert!(!journal.exists(), "a completed undo must drop the journal");

        let mut lpath = img.clone().into_os_string();
        lpath.push(".diskedit.lock");
        let _ = std::fs::remove_file(&img);
        let _ = std::fs::remove_file(std::path::PathBuf::from(lpath));
    }

    /// 同一偏移会被写两次：修复一步提交一次、紧随的常规提交再提交一次，journal 里因此
    /// 留下区间相同、`before`/`after` 各自相继的两条记录。逆序回放必须逐级退到最初的
    /// 原文——这是 undo 对"同一偏移多记录"的行为契约，也是它不被误判成"外部改动"的边界
    #[test]
    fn same_offset_records_replay_back_through_each_step() {
        let img = tmp_path("same_off_img");
        let mut jpath = img.clone().into_os_string();
        jpath.push(".diskedit.journal");
        let journal = std::path::PathBuf::from(jpath);
        {
            let mut j = Journal::open(&journal, jid()).unwrap();
            j.record(0, &[0xAA; 4], &[0xBB; 4]).unwrap();
            j.record(0, &[0xBB; 4], &[0xCC; 4]).unwrap();
        }
        let mut disk = vec![0u8; 1024];
        disk[0..4].copy_from_slice(&[0xCC; 4]);
        std::fs::write(&img, &disk).unwrap();

        assert_eq!(super::cmd_undo(&args_for(&img.to_string_lossy())), 0);
        let now = std::fs::read(&img).unwrap();
        assert_eq!(&now[0..4], &[0xAA; 4], "the oldest pre-image must win");
        assert!(!journal.exists(), "a completed undo must drop the journal");

        let _ = std::fs::remove_file(&img);
    }

    /// 上一条的中断态：后写那条已回滚（盘上停在中间值），先写那条未动。续跑必须接着退到
    /// 最初原文——核对一旦只认"写后内容"，这里就会被判成外部改动，把用户逼向 abandon
    #[test]
    fn same_offset_records_resume_after_a_partial_rollback() {
        let img = tmp_path("same_off_resume_img");
        let mut jpath = img.clone().into_os_string();
        jpath.push(".diskedit.journal");
        let journal = std::path::PathBuf::from(jpath);
        {
            let mut j = Journal::open(&journal, jid()).unwrap();
            j.record(0, &[0xAA; 4], &[0xBB; 4]).unwrap();
            j.record(0, &[0xBB; 4], &[0xCC; 4]).unwrap();
        }
        // 逆序回放的进度：第二条已写回中间值，第一条未动
        let mut disk = vec![0u8; 1024];
        disk[0..4].copy_from_slice(&[0xBB; 4]);
        std::fs::write(&img, &disk).unwrap();

        assert_eq!(super::cmd_undo(&args_for(&img.to_string_lossy())), 0);
        let now = std::fs::read(&img).unwrap();
        assert_eq!(&now[0..4], &[0xAA; 4], "an interrupted rollback must resume to the oldest pre-image");

        let _ = std::fs::remove_file(&img);
    }

    /// 整批核对发生在**任何写回之前**：每条记录都必须在盘面上找到它的一种历史
    /// （写后内容，或上次回滚留下的原文）；只要有一条对不上就整体拒绝。
    /// 判据与已回滚条数无关——这正是它比"边回滚边核对"强的地方：后者会把盘留在
    /// "回滚了一半"的混合态，而那种态既重跑不了也谈不上未动盘
    #[test]
    fn replay_plan_checks_every_entry_before_any_write_back() {
        let img = tmp_path("plan_img");
        let journal = journal_path(&img);
        {
            let mut j = Journal::open(&journal, jid()).unwrap();
            j.record(0, &[0xAA; 4], &[0xBB; 4]).unwrap();
            j.record(100, &[0xCC; 4], &[0xDD; 4]).unwrap();
        }
        let entries = match Journal::read_entries(&journal).unwrap() {
            JournalRead::Complete(v) => v,
            JournalRead::TruncatedTail(_) => panic!("a completed append must read back complete"),
        };

        // 正常现场：两条都是写后内容
        let mut disk = vec![0u8; 1024];
        disk[0..4].copy_from_slice(&[0xBB; 4]);
        disk[100..104].copy_from_slice(&[0xDD; 4]);
        std::fs::write(&img, &disk).unwrap();
        assert!(verify_replay_plan(&open_src(&img), &entries).is_ok());

        // 后写那条仍能对上、先写那条被外部改过：整批拒绝，而不是先回滚掉后一条
        disk[0..4].copy_from_slice(&[0x77; 4]);
        std::fs::write(&img, &disk).unwrap();
        let e = verify_replay_plan(&open_src(&img), &entries)
            .expect_err("one foreign edit must stop the whole plan, not just that entry");
        assert!(matches!(e, crate::outcome::Fail::Refused(_)), "{e:?}");

        // 上次 undo 中断的进度（两处都是原文）同样接受：重放同一原文幂等无害
        let mut disk = vec![0u8; 1024];
        disk[0..4].copy_from_slice(&[0xAA; 4]);
        disk[100..104].copy_from_slice(&[0xCC; 4]);
        std::fs::write(&img, &disk).unwrap();
        assert!(verify_replay_plan(&open_src(&img), &entries).is_ok());

        let _ = std::fs::remove_file(&img);
        let _ = std::fs::remove_file(&journal);
    }

    /// 候选缺席不是故障：它不得盖住另一份候选的真实损伤原因。
    /// 两份候选里"第一份不存在、第二份是陌生文件"是最常见的现场（历史命名那份通常不存在），
    /// 报出 No such file or directory 等于让用户去查一个根本不是原因的路径
    #[test]
    fn absent_candidate_does_not_mask_a_damaged_one() {
        let missing = tmp_path("missing");
        let foreign = tmp_path("foreign");
        std::fs::write(&foreign, b"not-a-journal").unwrap();
        let e = pick_journal(&[missing.clone(), foreign.clone()]).err().unwrap();
        // 在而读不出 ⇒ Unreadable（下游归 Infra + abandon 提示），且死因里不含缺席路径
        assert!(matches!(e, PickJournalError::Unreadable { .. }), "{e}");
        assert!(e.to_string().contains(&foreign.display().to_string()), "{e}");
        assert!(!e.to_string().contains(&missing.display().to_string()), "{e}");

        // 全部候选都不存在 → Absent（请求与现状不符，10）
        assert!(matches!(
            pick_journal(std::slice::from_ref(&missing)).unwrap(),
            super::PickedJournal::Absent
        ));

        // 唯一存在且可读的候选被选中（缺席的那份不影响）
        let ok = tmp_path("ok");
        {
            let mut j = Journal::open(&ok, jid()).unwrap();
            j.record(0, &[0xAA; 4], &[0xBB; 4]).unwrap();
        }
        let super::PickedJournal::Records(p, _) = pick_journal(&[missing, ok.clone()]).unwrap() else {
            panic!("a single readable candidate must be picked");
        };
        assert_eq!(p, ok);

        let _ = std::fs::remove_file(&ok);
        let _ = std::fs::remove_file(&foreign);
    }

    /// 两份候选同时可读 → Ambiguous：不猜（回放错一份就是把历史字节写到不该写的盘上），
    /// 错误文案列出两份路径，供 abandon 逐一定位
    #[test]
    fn two_readable_candidates_are_ambiguous_not_silently_picked() {
        let a = tmp_path("amb_a");
        let b = tmp_path("amb_b");
        for p in [&a, &b] {
            let mut j = Journal::open(p, jid()).unwrap();
            j.record(0, &[0xAA; 4], &[0xBB; 4]).unwrap();
        }
        let e = pick_journal(&[a.clone(), b.clone()]).err().unwrap();
        assert!(matches!(e, PickJournalError::Ambiguous { .. }), "{e}");
        assert!(e.to_string().contains(&a.display().to_string()), "{e}");
        assert!(e.to_string().contains(&b.display().to_string()), "{e}");

        let _ = std::fs::remove_file(&a);
        let _ = std::fs::remove_file(&b);
    }

    /// D1（A1）：既有 journal 的撕裂尾在采纳时被物理截断，新记录得以回放。模拟崩溃发生在
    /// 第一条记录只写了一半——盘上只有 magic + 一条不完整的记录（头声称巨大长度、无载荷）；
    /// open 截回 magic 长，之后追加的新记录才能被 undo 成功回放，而非永远卡在 TruncatedTail
    #[test]
    fn torn_journal_tail_is_truncated_and_replayable() {
        let img = tmp_path("torn_img");
        let mut jpath = img.clone().into_os_string();
        jpath.push(".diskedit.journal");
        let journal = std::path::PathBuf::from(jpath);
        let _ = std::fs::remove_file(&journal);

        // 盘面：offset 0 处原字节 0xAA，写入后变 0xBB
        let mut disk = vec![0u8; 1024];
        disk[0..4].copy_from_slice(&[0xAA; 4]);
        std::fs::write(&img, &disk).unwrap();

        // 先经 record() 真实建出头（open 惰性，不建文件），再截回头长、
        // 追加一段不完整的"首记录"——模拟崩溃发生在第一条记录只写了一半
        {
            let mut j = Journal::open(&journal, jid()).unwrap();
            j.record(usize::MAX as u64, &[0u8; 0], &[0u8; 0]).unwrap();
        }
        let with_magic = std::fs::read(&journal).unwrap();
        std::fs::write(&journal, &with_magic[..Journal::HEADER_LEN]).unwrap();
        {
            use std::io::Write;
            // 头声称巨大长度、且无对应载荷 ⇒ 扫描落到 TruncatedTail（非空 CRC 误判）
            let mut f = std::fs::OpenOptions::new().append(true).open(&journal).unwrap();
            f.write_all(b"\xff\xff\xff\xff\x99\x99\x99\x99").unwrap();
        }
        // 采纳：垃圾尾被截回头长（零完整记录）
        {
            let _j = Journal::open(&journal, jid()).unwrap();
        }
        assert_eq!(
            std::fs::read(&journal).unwrap().len(),
            Journal::HEADER_LEN,
            "torn first record must be truncated back to the header length"
        );
        // 重跑追加：记一条新记录，文件长度 = 头 + 一条完整记录
        {
            let mut j = Journal::open(&journal, jid()).unwrap();
            j.record(0, &[0xAA; 4], &[0xBB; 4]).unwrap();
        }
        // 记录帧 = [len(4)][crc(4)][payload]；PreImage payload = tag(1) + meta(0) +
        // rtag(1) + off(8) + pre_len(4) + before(4) + after(4) = 22（JournalRecord::encode）
        let framed = Journal::HEADER_LEN + 8 + 22;
        assert_eq!(
            std::fs::read(&journal).unwrap().len(),
            framed,
            "re-run append must yield header + exactly one complete record (torn tail already gone)"
        );
        // undo 成功回放新记录（盘上已 == 0xBB，回滚到 0xAA）并删除 journal
        let code = super::cmd_undo(&args_for(&img.to_string_lossy()));
        assert_eq!(code, 0, "undo of the replayed journal must succeed");
        let now = std::fs::read(&img).unwrap();
        assert_eq!(&now[0..4], &[0xAA; 4], "the entry must be rolled back to its pre-image");
        assert!(!journal.exists(), "a completed undo must drop the journal");

        let _ = std::fs::remove_file(&img);
    }

    /// D6（A6）：回放成功后清掉全部候选，不止被回放的那份——本工具的读不出残卷
    /// （magic 在、载荷损坏）随之消失；无 magic 的陌生文件保留并告警
    #[test]
    fn replay_success_clears_stale_unreadable_candidate() {
        let picked = tmp_path("d6_pick");
        let stale = tmp_path("d6_stale");
        let foreign = tmp_path("d6_foreign");
        for p in [&picked, &stale, &foreign] {
            let _ = std::fs::remove_file(p);
        }
        {
            let mut j = Journal::open(&picked, jid()).unwrap();
            j.record(0, &[0xAA; 4], &[0xBB; 4]).unwrap();
        }
        // 历史命名候选：本工具写下的残卷——magic 在、载荷被翻坏一个字节（pick_journal
        // 口径的"读不出"：CRC 不符）
        {
            let mut j = Journal::open(&stale, jid()).unwrap();
            j.record(0, &[0x11; 4], &[0x22; 4]).unwrap();
        }
        let mut raw = std::fs::read(&stale).unwrap();
        let last = raw.len() - 1;
        raw[last] ^= 0xFF;
        std::fs::write(&stale, &raw).unwrap();
        // 陌生文件：落点命名匹配，但内容不是本工具的 journal——不删
        std::fs::write(&foreign, b"not-a-journal").unwrap();

        super::clear_journal_candidates(&[picked.clone(), stale.clone(), foreign.clone()], &picked);
        assert!(!picked.exists(), "the replayed journal must be removed");
        assert!(!stale.exists(), "our unreadable residue must be removed after replay");
        assert!(foreign.exists(), "a file without our magic must be left in place");

        let _ = std::fs::remove_file(&foreign);
    }

    /// 精简 Args 构造：仅 target + yes，其余取默认空值（专供 cmd_undo 成功路径测试）
    fn args_for(target: &str) -> crate::args::Args {
        crate::args::Args {
            target: target.to_string(),
            part: None, grow: None, start: None, end: None, size: None,
            fs: None, name: None, clone: false, type_guid: None, table: None,
            yes: true, online: false, random: false, sector_size: None,
            align: String::new(), chunk_mib: 0, grow_to_end: false,
            allow_move: false, no_fs: false, grow_lv: false, start_end: false, lv: None,
            pos: Vec::new(), seen: Vec::new(),
        }
    }
}