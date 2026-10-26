#![allow(clippy::let_underscore_must_use)] // 测试的清理步骤有意忽略失败
use super::*;
use super::support::*;

/// `shift = None`（缩容请求）既不现算也不执行任何搬移计划：无 ckpt 必须拒绝；
/// 有 ckpt 同样拒绝——槽上的 plan 属于当初那条扩容请求，缩容请求执行它就是把
/// 请求静默放大成旧扩容（plan 的"执行方即请求方"归属判据），续跑只认原样重跑
#[test]
pub(crate) fn shift_plan_none_never_ghost_relocates() {
    let chunk: u64 = 1024 * 1024;
    let (mut src, path) = plan_fixture(&[(1, [0x11; 16], 2048, 6143), (2, [0x22; 16], 8192, 10239)]);
    let g = table::load_gpt(&src).unwrap().unwrap();
    let ckpt_path = src.identity.checkpoint_path().to_path_buf();

    let e = match plan_shift(&mut src, 1, None) {
        Err(e) => e,
        Ok(_) => panic!("no checkpoint must mean no relocation plan for a shrink"),
    };
    assert!(matches!(&e, Fail::Refused(m) if m.contains("shrink")), "{e:?}");

    let plan = make_plan(&mut src, 1).unwrap();
    let ck = Checkpoint {
        proof: crate::dev::LiveProof::Inode,
        disk_size: src.size, ss: g.ss, grow_part: 1, last_usable_lba: plan.last_usable_lba,
        moves: plan.moves.clone(), kind: PlanKind::Shift, cur_index: 0, chunks_done: 0, chunk_bytes: chunk,
        fp: Default::default(),
        map: None,
    };
    atomic_write_ckpt(&ckpt_path, &ck.serialize()).unwrap();
    // 有 ckpt 的缩容：拒绝并指向续跑/放弃，绝不把旧 grow plan 当缩容的执行体
    let e = match plan_shift(&mut src, 1, None) {
        Err(e) => e,
        Ok(_) => panic!("a shrink must not execute the pending grow relocation plan"),
    };
    assert!(matches!(&e, Fail::Refused(m) if m.contains("unfinished")), "{e:?}");
    assert!(pending_relocation(&src, 1).unwrap());
    assert!(!pending_relocation(&src, 2).unwrap(), "another partition has no pending job");

    drop(src);
    let _ = std::fs::remove_file(&ckpt_path);
    let _ = std::fs::remove_file(&path);
}

/// 缩容撞上"表已提交、只剩 FS 收尾"的扩容作业：那份作业的终点恰好等于当前末端，
/// 只比终点就会把缩容当成扩容续跑执行。同一现场下，尺寸未变的原样重跑才是合法续跑
#[test]
pub(crate) fn shrink_does_not_resume_a_committed_growth_job() {
    const SHIFT: u64 = 2048;
    let chunk: u64 = 1024 * 1024;
    // 作业已提交后的盘面：目标末端 = 原末端 + SHIFT，挡路者已右移到目标之后
    let (mut src, path) = plan_fixture(&[
        (1, [0x11; 16], 2048, 10239 + SHIFT),
        (2, [0x22; 16], 10240 + SHIFT, 12287 + SHIFT),
    ]);
    let g = table::load_gpt(&src).unwrap().unwrap();
    let ckpt_path = src.identity.checkpoint_path().to_path_buf();
    let ck = Checkpoint {
        proof: crate::dev::LiveProof::Inode,
        disk_size: src.size, ss: g.ss, grow_part: 1, last_usable_lba: g.header.last_usable_lba,
        moves: vec![PlanEntry { part_num: 2, first_lba: 10240, len_lba: 2048, delta_lba: SHIFT, is_swap: false }],
        kind: PlanKind::Shift, cur_index: 0, chunks_done: 0, chunk_bytes: chunk,
        fp: Default::default(),
        map: None,
    };
    atomic_write_ckpt(&ckpt_path, &ck.serialize()).unwrap();

    let e = plan_shift(&mut src, 1, None).err().expect("shrink must not resume the committed growth job");
    assert!(matches!(&e, Fail::Refused(m) if m.contains("unfinished")), "{e:?}");
    assert!(pending_relocation(&src, 1).unwrap());

    // 尺寸未变：计划终点与当前末端一致 ⇒ 这是同一作业的收尾，允许续跑
    let p = plan_shift_noop(&mut src, 1).expect("same-size re-run must resume the committed job");
    assert!(p.kind == PlanKind::Shift);
    assert_eq!(plan_requested_end(&p, 2048), Some(10239 + SHIFT));

    drop(src);
    let _ = std::fs::remove_file(&ckpt_path);
    let _ = std::fs::remove_file(&path);
}

/// 续跑的身份比对必须含**请求数值**：同 kind、不同 SIZE 的重跑不得静默执行旧 plan。
/// 不比对数值时的后果正是断言反面的那一幕——搬移按旧 X 落位、表项照抄旧终点，
/// 本次请求的 Y 被静默丢弃，而写入不可逆
#[test]
pub(crate) fn shift_resume_requires_the_same_requested_size() {
    let chunk: u64 = 1024 * 1024;
    const P1_START: u64 = 2048;
    const P1_END: u64 = 6143;
    const SHIFT: u64 = 4096;
    let (mut src, path) = plan_fixture(&[(1, [0x11; 16], P1_START, P1_END), (2, [0x22; 16], 8192, 10239)]);
    let ckpt_path = src.identity.checkpoint_path().to_path_buf();
    let g = table::load_gpt(&src).unwrap().unwrap();

    // 槽位为空时现算一份 Shift plan，把它当作"当初那次请求"的进度记录落盘
    let plan = plan_shift(&mut src, 1, Some(SHIFT)).unwrap();
    assert!(plan.kind == PlanKind::Shift);
    // 身份里的数值一维就是它：请求的扩容终点 = 目标原末端 + shift
    assert_eq!(grow_end_for(&plan, P1_START).unwrap(), P1_END + SHIFT);
    let ck = Checkpoint {
        proof: crate::dev::LiveProof::Inode,
        disk_size: src.size, ss: g.ss, grow_part: 1, last_usable_lba: plan.last_usable_lba,
        moves: plan.moves.clone(), kind: PlanKind::Shift, cur_index: 0, chunks_done: 0, chunk_bytes: chunk,
        fp: Default::default(),
        map: None,
    };
    atomic_write_ckpt(&ckpt_path, &ck.serialize()).unwrap();

    // 原样重跑：两维都对上 ⇒ 续跑，终点仍是当初请求的那个
    let resumed = plan_shift(&mut src, 1, Some(SHIFT)).expect("re-running the same size must resume");
    assert!(resumed.kind == PlanKind::Shift);
    assert_eq!(grow_end_for(&resumed, P1_START).unwrap(), P1_END + SHIFT);

    // 同 kind、不同 SIZE
    let e = plan_shift(&mut src, 1, Some(SHIFT + 4096))
        .err()
        .expect("a different size must not silently execute the pending plan");
    assert!(matches!(&e, Fail::Refused(m) if m.contains("different size")), "{e:?}");

    // 槽上是 Shift 作业，grow 入口不得把它当自己的续跑
    let e = plan_resuming(&mut src, 1).err().expect("a Shift job must refuse the grow entry");
    assert!(matches!(&e, Fail::Refused(m) if m.contains("resize SIZE")), "{e:?}");
    // 归属仍是本分区的作业：断言被挡住的是身份比对，不是归属判定
    assert!(pending_relocation(&src, 1).unwrap());

    drop(src);
    let _ = std::fs::remove_file(&ckpt_path);
    let _ = std::fs::remove_file(&path);
}

#[test]
pub(crate) fn make_plan_pack_and_boundaries() {
    const SWAP: [u8; 16] = [0x6D, 0xFD, 0x57, 0x06, 0xAB, 0xA4, 0xC4, 0x43, 0x84, 0xE5, 0x09, 0x33, 0xC8, 0x4B, 0x4F, 0x4F];
    const LUKS: [u8; 16] = table::LUKS_TYPE_GUID;
    // root 2048..6143 | home 6144..10239 | swap 10240..12287
    let (mut src, _p1) = plan_fixture(&[
        (1, [0x11; 16], 2048, 6143),
        (2, [0x22; 16], 6144, 10239),
        (3, SWAP, 10240, 12287),
    ]);
    let plan = make_plan(&mut src, 1).unwrap();
    assert_eq!(plan.moves.len(), 2);
    // 末→首执行序：swap 先（重建），home 后（搬移）；末端对齐 last_usable=32734
    assert!(plan.moves[0].is_swap && plan.moves[0].part_num == 3);
    assert_eq!(plan.moves[0].first_lba + plan.moves[0].delta_lba, 30687, "swap new first_lba");
    assert!(!plan.moves[1].is_swap && plan.moves[1].part_num == 2);
    assert_eq!(plan.moves[1].first_lba + plan.moves[1].delta_lba, 26591, "home new first_lba");
    drop(src);

    // LUKS 在目标之后 → 拒绝
    let (mut src2, _p2) = plan_fixture(&[
        (1, [0x11; 16], 2048, 6143),
        (2, LUKS, 6144, 10239),
    ]);
    assert!(make_plan(&mut src2, 1).is_err());
    drop(src2);

    // 尾部容量不足：条目越过 last_usable（损坏表）→ 不得进入规划。
    // gptman 会拒绝写入越界条目，故先写合法表再手工修补原始字节 + 重算**该副本**的双 CRC
    let (src3, p3) = plan_fixture(&[(1, [0x11; 16], 2048, 6143), (2, [0x22; 16], 30000, 32000)]);
    drop(src3);
    let last_lba = std::fs::metadata(&p3).unwrap().len() / 512 - 1;
    let backup_arr_lba = last_lba - 32; // 备份数组跨度 = 128×128/512 扇区，紧邻备份头之前
    // 条目 2（索引 1）的 ending_lba 改成 34000（越过 last_usable）
    let patch_entry = |raw: &mut Vec<u8>, arr_lba: u64, hdr_lba: u64| {
        let (a, h) = ((arr_lba * 512) as usize, (hdr_lba * 512) as usize);
        raw[a + 128 + 40..a + 128 + 48].copy_from_slice(&34000u64.to_le_bytes());
        let arr_crc = table::crc32(&raw[a..a + 128 * 128]);
        raw[h + 88..h + 92].copy_from_slice(&arr_crc.to_le_bytes());
        raw[h + 16..h + 20].fill(0);
        let hdr_crc = table::crc32(&raw[h..h + 92]);
        raw[h + 16..h + 20].copy_from_slice(&hdr_crc.to_le_bytes());
    };
    // 越界的只有主副本：那是这一份的事，盘尾备份给出合法表，规划照旧成立
    {
        let mut raw = std::fs::read(&p3).unwrap();
        patch_entry(&mut raw, 2, 1);
        std::fs::write(&p3, &raw).unwrap();
    }
    let mut src3 = plan_open(&p3);
    assert!(make_plan(&mut src3, 1).is_ok(), "the intact backup copy must still yield a plan");
    drop(src3);
    // 两份都越界 ⇒ 拒绝：越界条目不得被当成有效布局
    {
        let mut raw = std::fs::read(&p3).unwrap();
        patch_entry(&mut raw, 2, 1);
        patch_entry(&mut raw, backup_arr_lba, last_lba);
        std::fs::write(&p3, &raw).unwrap();
    }
    let mut src3 = plan_open(&p3);
    assert!(make_plan(&mut src3, 1).is_err());
    drop(src3);
    let _ = std::fs::remove_file(&p3);
}

/// grow_end 公式的纯算术边界（不依赖任何平台）
#[test]
pub(crate) fn grow_end_formula_and_overflow() {
    let plan_of = |moves: Vec<PlanEntry>, last_usable: u64| Plan { ss: 512, last_usable_lba: last_usable, grow_part: 1, moves, kind: PlanKind::TailPacked, repair: RepairAction::None };
    let mv = |first_lba: u64, delta_lba: u64| PlanEntry { part_num: 1, first_lba, len_lba: 10, delta_lba, is_swap: false };

    // 无 movable → last_usable_lba
    assert_eq!(grow_end_for(&plan_of(vec![], 32734), 0).unwrap(), 32734);
    // last_usable_lba 取满值也不得溢出
    assert_eq!(grow_end_for(&plan_of(vec![], u64::MAX), 0).unwrap(), u64::MAX);
    // delta = 0（已贴合的 movable 同样进 moves）→ 其新起点 − 1
    assert_eq!(grow_end_for(&plan_of(vec![mv(1000, 0)], 32734), 0).unwrap(), 999);
    // 多个 movable → 取最小新起点 − 1（100→150、200→250）
    assert_eq!(grow_end_for(&plan_of(vec![mv(100, 50), mv(200, 50)], 32734), 0).unwrap(), 149);
    // first_lba + delta_lba 溢出 → 报错，不回绕
    assert!(grow_end_for(&plan_of(vec![mv(u64::MAX - 5, 10)], u64::MAX), 0).is_err());
    // 新起点越过 last_usable_lba → 拒绝（越界表不得写进几何）
    assert!(grow_end_for(&plan_of(vec![mv(40000, 0)], 32734), 0).is_err());
    // 终点低于目标分区自身起点 → 拒绝（端点倒挂的表项不得提交）
    assert!(grow_end_for(&plan_of(vec![mv(100, 0)], 32734), 200).is_err());
}

/// 右邻已尾打包时重跑 grow：规划只产出 δ=0 的空转 move，执行它不得写盘。
/// 落屏障的条件是"moves 里存在非 0 δ"（apply 里的 `effective`），本测试同时锁住
/// 该判定的输入（全 0 δ）与后果（邻居表项一字未动）
#[test]
pub(crate) fn tail_packed_rerun_only_yields_zero_delta_moves() {
    let data = vec![0u8; 4 * 1024 * 1024];
    let mut src = crate::support::src_from("tp0", &data);
    table::create_gpt(&mut src, 512, None).unwrap();
    // 4MiB = 8192 扇区，尾部留给备份头与备份数组 ⇒ last_usable = 8158：
    // p3 紧贴尾部，p1 与 p3 之间留 2000 扇区空隙
    gpt_policy::add_entry_at(&mut src, 2048, 3000, "p1", table::LINUX_FS_TYPE_GUID, [1; 16]).unwrap();
    gpt_policy::add_entry_at(&mut src, 5001, 8158, "p3", table::LINUX_FS_TYPE_GUID, [3; 16]).unwrap();
    let plan = make_plan(&mut src, 1).unwrap();
    assert!(!plan.moves.is_empty(), "the tail neighbour is a blocker and must appear as a move");
    assert!(plan.moves.iter().all(|m| m.delta_lba == 0), "an already tail-packed neighbour needs no relocation");
    let before = table::load_gpt(&src).unwrap().unwrap();
    let o = ap(&mut src, &plan, 1 << 20, true, &mut |_| {});
    assert!(o.is_complete(), "a zero-delta plan must not fail: {}", o.exit_code());
    let after = table::load_gpt(&src).unwrap().unwrap();
    assert_eq!(
        (before.entries[2].starting_lba, before.entries[2].ending_lba),
        (after.entries[2].starting_lba, after.entries[2].ending_lba),
        "a zero-delta move must leave the neighbour untouched"
    );
}

/// 真实路径：truncate 预扩后 backup GPT 与保护 MBR 同时过期 —— 解析标记 NeedsRepair、
/// plan 只记录修复动作（不写盘）、写入路径 repair 到新末端
#[test]
pub(crate) fn stale_after_enlarge_detected_then_repaired() {
    use std::io::Write as _;
    let (src, p) = plan_fixture(&[(1, [0x11; 16], 2048, 6143)]);
    let old_file_last = src.size / 512 - 1;
    drop(src);
    {
        let mut f = std::fs::OpenOptions::new().append(true).open(&p).unwrap();
        f.write_all(&vec![0u8; 1024 * 1024]).unwrap();
    }
    let mut src = plan_open(&p);
    let new_file_last = src.size / 512 - 1;
    let g = table::load_gpt(&src).unwrap().expect("扩容后仍应可识别");
    assert_eq!(
        g.state,
        table::GptState::NeedsRepair {
            cause: table::HeaderIssue::BackupLbaStale { expected: new_file_last, actual: old_file_last }
        }
    );
    assert_eq!(
        g.pmbr,
        table::PmbrSize::NeedsRepair { cause: table::PmbrIssue::Stale }
    );
    assert_eq!(g.header.backup_lba, old_file_last, "解析不得偷偷改写盘上表");
    // plan 只记录修复动作，不写盘；计划几何按修复后的值算
    let plan = make_plan(&mut src, 1).unwrap();
    assert!(matches!(
        plan.repair,
        RepairAction::RelocateAndRepair { .. }
    ), "扩容后备份头与保护 MBR 都过期，动作须同时覆盖两者");
    assert_eq!(plan.last_usable_lba, new_file_last - 32 - 1);
    assert_eq!(
        table::load_gpt(&src).unwrap().unwrap().state,
        table::GptState::NeedsRepair {
            cause: table::HeaderIssue::BackupLbaStale { expected: new_file_last, actual: old_file_last }
        },
        "plan 不得写盘"
    );
    // 写入路径：修复到新末端 + 保护 MBR 与容器一致
    let (_, action) = gpt_policy::resolve_geometry(&src).unwrap().unwrap();
    let ss = src.sector_size;
    gpt_policy::apply_repair(&mut src, &action, ss).unwrap();
    let g2 = table::load_gpt(&src).unwrap().unwrap();
    assert_eq!(g2.state, table::GptState::Valid);
    assert_eq!(g2.pmbr, table::PmbrSize::Normal);
    assert_eq!(g2.header.backup_lba, new_file_last);
    assert_eq!(g2.header.last_usable_lba, new_file_last - 32 - 1); // span = 128×128/512
    assert_eq!(gpt_policy::classify_repair(&g2, new_file_last).unwrap(), RepairAction::None, "修复须幂等");
    drop(src);
    let _ = std::fs::remove_file(&p);
}

/// 「Refused 承诺本次未写盘」的落地检验：需要修复的 stale 表遇到事前拒绝时，
/// 盘上必须**仍是** stale。修复写盘若发生在事前判定之前，这条断言就会红
#[test]
pub(crate) fn refused_paths_do_not_write_the_disk() {
    for via_apply in [false, true] {
        let (src, p) = plan_fixture(&[(1, [0x11; 16], 2048, 6143), (2, [0x22; 16], 8192, 12287)]);
        drop(src);
        make_stale(&p);
        let mut src = plan_open(&p);
        let ckpt_path = src.identity.checkpoint_path().to_path_buf();
        assert!(
            matches!(table::load_gpt(&src).unwrap().unwrap().state, table::GptState::NeedsRepair { .. }),
            "夹具须是需要修复的表"
        );

        let chunk: u64 = 1024 * 1024;
        let o = if via_apply {
            // 槽位被另一族（单分区 resize）作业占着 ⇒ apply 须事前拒绝
            let rs = RsCheckpoint {
                proof: crate::dev::LiveProof::Inode,
                disk_size: src.size, ss: 512, part: 1, old_start: 2048, old_end: 6143,
                new_start: 2048, new_end: 4095, fs_shrunk: FsShrink::NotShrunk, chunks_done: 0, chunk_bytes: chunk,
                fp: Default::default(),
                map: None,
                fstype: None,
            };
            atomic_write_ckpt(&ckpt_path, &rs.serialize()).unwrap();
            let plan = make_plan(&mut src, 1).unwrap();
            ap(&mut src, &plan, chunk, true, &mut |_| {})
        } else {
            // 新范围与分区 2 重叠 ⇒ resize-part 须事前拒绝
            rsize(&mut src, 1, 2048, 9000, chunk, true, &mut |_| {})
        };
        assert_eq!(o.exit_code(), 10, "via_apply={via_apply}");

        let g = table::load_gpt(&src).unwrap().unwrap();
        assert!(
            matches!(g.state, table::GptState::NeedsRepair { .. }),
            "via_apply={via_apply}: 拒绝路径不得写盘（修复不能被提前执行）"
        );
        assert_eq!(g.header.backup_lba, 20000, "via_apply={via_apply}");
        drop(src);
        let _ = std::fs::remove_file(&ckpt_path);
        let _ = std::fs::remove_file(&p);
    }
}

/// 表本来就合法（无需修复）时，事前拒绝同样是 10 且一字未写：边界是否推进只取决于
/// "本次调用有没有碰过目标盘"，与"有没有修复可做"无关。与上一条互补——那条用需要修复
/// 的表，拦的是"修复写盘被提前执行"；这条稳住"无修复可做"时不得把拒绝误升成 30
#[test]
pub(crate) fn refusal_on_an_already_valid_table_is_still_ten() {
    let (src, p) = plan_fixture(&[(1, [0x11; 16], 2048, 6143), (2, [0x22; 16], 8192, 12287)]);
    drop(src);
    let mut src = plan_open(&p);
    let ckpt_path = src.identity.checkpoint_path().to_path_buf();
    assert!(
        matches!(table::load_gpt(&src).unwrap().unwrap().state, table::GptState::Valid),
        "夹具须是无需修复的表"
    );
    let before = std::fs::read(&p).unwrap();

    // 新范围与分区 2 重叠 ⇒ 事前拒绝
    let o = rsize(&mut src,1, 2048, 9000, 1024 * 1024, true, &mut |_| {});
    assert_eq!(o.exit_code(), 10, "无需修复的表上，事前拒绝仍是 Refused");
    drop(src);

    assert_eq!(std::fs::read(&p).unwrap(), before, "拒绝路径不得写盘");
    assert!(!ckpt_path.exists(), "拒绝早于 checkpoint 落盘");
    let _ = std::fs::remove_file(&p);
}

/// 边界另一侧：搬移中途失败只能报 Failed（30），不得报 Refused（10）——此刻数据区
/// 已经动过，声称"本次未写盘"就是假的。注入一次读失败让它发生在前几个 chunk 之后
#[test]
pub(crate) fn failure_after_the_boundary_is_failed_not_refused() {
    let (mut src, path) = plan_fixture(&[(1, [0x11; 16], 2048, 14335)]); // 源 6 MiB
    let ckpt_path = src.identity.checkpoint_path().to_path_buf();
    let chunk: u64 = 1024 * 1024;
    // 注入点落在搬移的第三轮读（右移时尾→头推进，此刻已有 3 个 chunk 写进目标区），
    // 且不在 prepare 阶段的任何探测区间里
    let _fault = crate::dev::ReadFaultGuard::at(2048 * 512 + 3 * chunk);
    let o = rsize(&mut src,1, 6144, 18431, chunk, true, &mut |_| {});
    assert_eq!(o.exit_code(), 30, "越界之后的失败须按 Failed 报（不得声称未写盘）");
    drop(src);
    let _ = std::fs::remove_file(&ckpt_path);
    let _ = std::fs::remove_file(&path);
}

/// 只有 backup 头过期（保护 MBR 与容器一致）时：PMBR 无需重写，只搬移备份头
#[test]
pub(crate) fn stale_backup_detected_then_relocated() {
    let (src, p) = plan_fixture(&[(1, [0x11; 16], 2048, 6143)]);
    let file_last = src.size / 512 - 1;
    drop(src);
    {
        // 把主头的 AlternateLBA 改到 20000（早于末端），重算头 CRC——构造 stale 状态
        let mut raw = std::fs::read(&p).unwrap();
        raw[512 + 32..512 + 40].copy_from_slice(&20000u64.to_le_bytes());
        raw[512 + 16..512 + 20].fill(0);
        let hdr_crc = table::crc32(&raw[512..512 + 92]);
        raw[512 + 16..512 + 20].copy_from_slice(&hdr_crc.to_le_bytes());
        std::fs::write(&p, &raw).unwrap();
    }
    let mut src = plan_open(&p);
    let g = table::load_gpt(&src).unwrap().expect("stale GPT must still be identifiable");
    assert_eq!(
        g.state,
        table::GptState::NeedsRepair {
            cause: table::HeaderIssue::BackupLbaStale { expected: file_last, actual: 20000 }
        }
    );
    assert_eq!(g.pmbr, table::PmbrSize::Normal, "夹具的保护 MBR 与容器一致");
    assert_eq!(g.header.backup_lba, 20000, "解析不得偷偷改写盘上表");
    // 写入路径：就地修复并把备份头搬到设备末端
    let (_, action) = gpt_policy::resolve_geometry(&src).unwrap().unwrap();
    let ss = src.sector_size;
    gpt_policy::apply_repair(&mut src, &action, ss).unwrap();
    let g2 = table::load_gpt(&src).unwrap().unwrap();
    assert_eq!(g2.state, table::GptState::Valid);
    assert_eq!(g2.pmbr, table::PmbrSize::Normal);
    assert_eq!(g2.header.backup_lba, file_last);
    assert_eq!(g2.header.last_usable_lba, file_last - 32 - 1); // span = 128×128/512
    assert_eq!(table::load_gpt(&src).unwrap().unwrap().state, table::GptState::Valid, "修复须幂等");
    drop(src);
    let _ = std::fs::remove_file(&p);
}

/// 几何自洽性在解析层强制：last_usable_lba 越过盘末端即拒绝（不必等下游算术兜底）。
/// 该判定读的是**本副本**的字段，故单份越界只说明这一份不可用；两份都越界才拒绝
#[test]
pub(crate) fn geometry_rejects_last_usable_beyond_disk() {
    let (src, p) = plan_fixture(&[(1, [0x11; 16], 2048, 6143)]);
    drop(src);
    // 对照组：未打补丁时表可解析——证明后面的 Err 来自几何判定而非 CRC 写错
    let clean = plan_open(&p);
    assert!(table::load_gpt(&clean).unwrap().is_some());
    drop(clean);
    let last_lba = std::fs::metadata(&p).unwrap().len() / 512 - 1;
    // 手改 last_usable_lba 后重算该副本的头 CRC，才能穿过签名/CRC 抵达几何判定
    let patch = |raw: &mut Vec<u8>, hdr_lba: u64| {
        let h = (hdr_lba * 512) as usize;
        raw[h + 48..h + 56].copy_from_slice(&u64::MAX.to_le_bytes());
        raw[h + 16..h + 20].fill(0);
        let hdr_crc = table::crc32(&raw[h..h + 92]);
        raw[h + 16..h + 20].copy_from_slice(&hdr_crc.to_le_bytes());
    };

    // 只坏主头：盘尾备份完好 ⇒ 回退。单份越界不该否掉整块盘
    {
        let mut raw = std::fs::read(&p).unwrap();
        patch(&mut raw, 1);
        std::fs::write(&p, &raw).unwrap();
    }
    let src = plan_open(&p);
    let g = table::load_gpt(&src).expect("a single out-of-range copy must fall back").expect("the backup copy is intact");
    assert!(
        matches!(g.state, table::GptState::NeedsRepair { cause: table::HeaderIssue::PrimaryUnreadable }),
        "recovered-from-backup must be marked for repair"
    );
    drop(src);

    // 两份都越界 ⇒ 拒绝，且拒绝发生在解析层（所有消费者共享），resolve_geometry 只是透传
    {
        let mut raw = std::fs::read(&p).unwrap();
        patch(&mut raw, last_lba);
        std::fs::write(&p, &raw).unwrap();
    }
    let src = plan_open(&p);
    let err = match table::load_gpt(&src) {
        Err(e) => e,
        Ok(_) => panic!("beyond-container last_usable_lba must be refused at parse time"),
    };
    assert!(
        matches!(err, table::GptError::BeyondContainer { field: "last_usable_lba", .. }),
        "{err}"
    );
    assert!(gpt_policy::resolve_geometry(&src).is_err());
    drop(src);
    let _ = std::fs::remove_file(&p);
}