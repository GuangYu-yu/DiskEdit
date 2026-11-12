#![allow(clippy::let_underscore_must_use)] // 测试的清理步骤有意忽略失败
use super::*;
use super::support::*;

/// 条目循环未走完（swap 重建失败就地停止）时扩容收尾必须跳过：grow_end_for 按全体
/// moves 取 min(new_first)−1，会把尚未搬移成员的旧位划进扩容区，提交即写出与未搬移
/// 分区重叠的表项。swap 条目只给 64 扇区（32KiB，低于 mkswap 的最小可建容量）：
/// Linux 上工具齐备时重建按容量拒绝；非 Linux 或工具缺失（mkswap 或镜像所需的
/// losetup）时以环境不满足（6）在写盘前停下（不建记录）——分类走 outcome 的
/// 唯一映射点，与其它 require_linux / 缺工具路径同源。恢复段另起一份未动过的现场：
/// 已有 relocation 记录时不做能力拒绝，缺工具/重建失败只以 pending 收场并保留记录
#[test]
pub(crate) fn swap_rebuild_failure_skips_growth_finalization() {
    const SWAP: [u8; 16] = [0x6D, 0xFD, 0x57, 0x06, 0xAB, 0xA4, 0xC4, 0x43, 0x84, 0xE5, 0x09, 0x33, 0xC8, 0x4B, 0x4F, 0x4F];
    let chunk: u64 = 1024 * 1024;
    // root 2048..6143 | home 6144..10239 | swap 10240..10303（64 扇区）
    let fixture = || plan_fixture(&[
        (1, [0x11; 16], 2048, 6143),
        (2, [0x22; 16], 6144, 10239),
        (3, SWAP, 10240, 10303),
    ]);

    // 新跑（无记录）：工具不齐时必须在写盘前拒绝，不建立 pending 事务
    let (mut src, _path) = fixture();
    let record_path = src.identity.record_path().expect("image targets have a record path").to_path_buf();
    let mut plan = make_plan(&mut src, 1).unwrap();
    assert!(plan.moves.iter().any(|m| m.is_swap), "fixture must carry a movable swap");
    // 平台闸先于工具探测：非 Linux 上即使两个工具名都在 PATH，也按环境不满足断言
    let tools_ready = cfg!(target_os = "linux")
        && crate::fsops::find_tool("mkswap").is_ok()
        && crate::fsops::find_tool("losetup").is_ok();
    if !tools_ready {
        let fresh = ap(&mut src, &mut plan, chunk, true, &mut |_| {});
        assert_eq!(
            fresh.exit_code(),
            6,
            "a new swap relocation without the rebuild tools must stop before writing: {fresh:?}"
        );
        assert!(!record_path.exists(), "a refusal must not create a recovery record");
        let g = table::load_gpt(&src).unwrap().unwrap();
        assert_eq!((g.entries[0].starting_lba, g.entries[0].ending_lba), (2048, 6143));
        assert_eq!((g.entries[2].starting_lba, g.entries[2].ending_lba), (10240, 10303));
        drop(src);
    } else {
        #[cfg(target_os = "linux")]
        {
            // 生产闸已放行：挂上记录，让作业在 swap 重建处停下并留下可续跑的进度
            attach_record(
                &mut src,
                crate::recovery::OpKind::Resize,
                (10240, 10303),
                (plan.grow_part, 2048, grow_end_for(&plan, 2048).unwrap()),
            );
            let fresh = ap(&mut src, &mut plan, chunk, true, &mut |_| {});
            assert_eq!(fresh.exit_code(), 5, "the fresh run ends as pending: {fresh:?}");
            // 续跑输入取自生产刚写出的现场：进度字段错（晚写、写错）在这条断言上就会红
            let c = relocation_of(&src).expect("a pending swap relocation must leave a relocation record");
            assert_eq!(c.entry_index, 0, "swap 未重建时 entry_index 必须停在 swap 条目");
            assert_eq!(c.chunks_in_entry, 0, "swap 条目不搬数据，chunk 进度必须为 0");
            assert_eq!(c.chunk_bytes, chunk);
            assert_eq!(c.entries.len(), plan.moves.len());
            for (a, b) in c.entries.iter().zip(&plan.moves) {
                assert_eq!(
                    (a.partition, a.src_first, a.len, a.delta, a.is_swap),
                    (b.part_num, b.first_lba, b.len_lba, b.delta_lba, b.is_swap)
                );
            }
            assert_eq!(c.grow_part, plan.grow_part);
            assert_eq!((c.disk_size, c.ss), (src.size, plan.ss));

            let o = ap(&mut src, &mut plan, chunk, true, &mut |_| {});
            match &o {
                crate::outcome::RunOutcome::Applied { pending, .. } => {
                    assert_eq!(pending.len(), 1, "须报一条待办：{pending:?}");
                    assert_eq!(pending[0].kind, crate::outcome::PendingKind::Swap);
                }
                other => panic!("expected Applied, got {other:?}"),
            }
            assert!(!o.is_complete());
            assert_eq!(o.exit_code(), 5, "swap 未重建，不能报完全成功");
            let g = table::load_gpt(&src).unwrap().unwrap();
            assert_eq!((g.entries[0].starting_lba, g.entries[0].ending_lba), (2048, 6143));
            assert_eq!((g.entries[1].starting_lba, g.entries[1].ending_lba), (6144, 10239));
            let sw = plan.moves.iter().find(|m| m.is_swap).expect("the plan must carry the swap move");
            let target = sw.first_lba.checked_add_signed(sw.delta_lba).unwrap();
            assert_eq!(
                (g.entries[2].starting_lba, g.entries[2].ending_lba),
                (target, target + sw.len_lba - 1),
                "swap 表项必须落在搬移后的目标位"
            );
            assert!(record_path.exists(), "有待办时记录必须保留");
            drop(src);
        }
    }
}

/// apply 路径的 durable 边界：自重叠搬移中途读失败后，记录只能记录已同步落盘的
/// chunk；续跑从该边界之后继续，已完成块的源区即便被改写也不得被重新搬回目标区。
/// 进度写入若被挪到提交之后、或写错，这两组断言都会红
#[cfg(target_os = "linux")]
#[test]
pub(crate) fn apply_crash_leaves_durable_progress_in_the_record() {
    let (mut src, _path) = plan_fixture(&[
        (1, [0x11; 16], 2048, 6143),
        (2, [0x22; 16], 8192, 24575),
    ]);
    let mut plan = make_plan(&mut src, 1).unwrap();
    let record_path = attach_record(
        &mut src,
        crate::recovery::OpKind::Resize,
        (2048, 6143),
        (plan.grow_part, 2048, grow_end_for(&plan, 2048).unwrap()),
    );
    let m = plan.moves.iter().find(|m| m.part_num == 2).expect("p2 must be the blocker").clone();
    assert!(
        m.delta_lba > 0 && m.delta_lba.unsigned_abs() < m.len_lba,
        "the fixture must produce a self-overlapping move"
    );
    let chunk: u64 = 1024 * 1024;
    let total = m.len_lba * 512;
    let blocks = (total / chunk) as u8;
    let src_off = m.first_lba * 512;
    let dst_off = m.first_lba.checked_add_signed(m.delta_lba).unwrap() * 512;
    for i in 0..blocks {
        src.write_at(src_off + i as u64 * chunk, &vec![0x10 + i; 512]).unwrap();
    }
    // 执行序尾→头：第 3 轮读（within = total − 3·chunk）失败时，前两块已同步并落 ckpt
    let fault = crate::dev::ReadFaultGuard::at(src_off + total - 3 * chunk);
    let o = ap(&mut src, &mut plan, chunk, true, &mut |_| {});
    assert_eq!(o.exit_code(), 5, "mid-move read failure must stay Partial: {o:?}");
    drop(fault);

    let c = relocation_of(&src).expect("a failed move must leave a relocation record");
    assert_eq!(c.entry_index, 0, "the only movable is still in flight");
    assert_eq!(c.chunks_in_entry, 2, "only synced-and-saved chunks may be durable");
    assert_eq!(c.chunk_bytes, chunk);
    assert_eq!(c.entries.len(), plan.moves.len());
    for (a, b) in c.entries.iter().zip(&plan.moves) {
        assert_eq!(
            (a.partition, a.src_first, a.len, a.delta, a.is_swap),
            (b.part_num, b.first_lba, b.len_lba, b.delta_lba, b.is_swap)
        );
    }
    assert_eq!(c.grow_part, plan.grow_part);
    assert_eq!((c.disk_size, c.ss), (src.size, plan.ss));

    // 已完成块（within = total − chunk）的源区改写成垃圾：续跑若从 0 重来会先读它
    src.write_at(src_off + total - chunk, &[0xEE; 512]).unwrap();
    let resumed = ap(&mut src, &mut plan, chunk, true, &mut |_| {});
    assert!(resumed.is_complete(), "the resume must finish: {resumed:?}");
    for i in 0..blocks {
        let mut b = [0u8; 512];
        src.read_at(dst_off + i as u64 * chunk, &mut b).unwrap();
        assert_eq!(b, [0x10 + i; 512], "chunk {i} came from the wrong source block");
    }
    let g = table::load_gpt(&src).unwrap().unwrap();
    let grow_end = grow_end_for(&plan, 2048).unwrap();
    assert_eq!(
        (g.entries[0].starting_lba, g.entries[0].ending_lba),
        (2048, grow_end),
        "grow must commit its computed ending_lba"
    );
    let dst = m.first_lba.checked_add_signed(m.delta_lba).unwrap();
    assert_eq!(
        (g.entries[1].starting_lba, g.entries[1].ending_lba),
        (dst, dst + m.len_lba - 1)
    );
    assert_eq!(g.entries[0].ending_lba + 1, g.entries[1].starting_lba, "the two entries must stay adjacent");
    // 收口由命令层做：完整结果才提交并删除记录
    let done = crate::support::table_record_done_if_complete(&mut src, resumed);
    assert!(done.is_complete(), "the record cleanup must not degrade the result: {done:?}");
    assert!(!record_path.exists(), "a completed resume must release the record");
    drop(src);
}

/// 写盘前的复核失败时，盘上不得留下本次作业的记录：记录是恢复路径的
/// 唯一入口，先落盘再复核会让一次"未写盘"的拒绝在下一次运行里变成一份可续跑现场
#[test]
pub(crate) fn apply_refuses_changed_table_before_any_record_is_written() {
    let (mut src, path) = plan_fixture(&[
        (1, [0x11; 16], 2048, 6143),
        (2, [0x22; 16], 8192, 24575),
    ]);
    let record_path = src.identity.record_path().expect("image targets have a record path").to_path_buf();
    let plan = make_plan(&mut src, 1).unwrap();
    let (g, _repair) = gpt_policy::resolve_geometry(&src).unwrap().unwrap();
    let chunk: u64 = 1024 * 1024;
    let d = prepare_apply(&src, &g, &plan, chunk, true, false, None, None, None, &mut |_| {}).unwrap();
    assert!(!record_path.exists(), "prepare must not create a record");
    tamper_disk_guid(&mut src);
    let before = std::fs::read(&path).unwrap();
    let mut post: Vec<Pending> = Vec::new();
    let r = execute_apply(&mut src, &plan, d, &mut |_| {}, &mut post);
    let err = r.expect_err("execute must refuse a table changed after prepare").to_string();
    assert!(err.contains("changed outside this tool"), "unexpected error: {err}");
    assert!(!record_path.exists(), "a failed pre-write recheck must not leave a record");
    assert_eq!(std::fs::read(&path).unwrap(), before, "the target must stay byte-identical");
    drop(src);
}

/// "操作已提交"不成立时：条目循环未走完（swap 重建失败）提前收场，记录必须留在原处，
/// 且本次运行的输出要说明扩容未应用。记录字节仍须可判读（它正是下一次续跑的输入）
#[cfg(target_os = "linux")]
#[test]
pub(crate) fn uncommitted_relocation_keeps_its_record() {
    const SWAP: [u8; 16] = [0x6D, 0xFD, 0x57, 0x06, 0xAB, 0xA4, 0xC4, 0x43, 0x84, 0xE5, 0x09, 0x33, 0xC8, 0x4B, 0x4F, 0x4F];
    // root 2048..6143 | home 6144..10239 | swap 10240..10303（64 扇区，低于 mkswap 最小容量）
    let (mut src, _path) = plan_fixture(&[
        (1, [0x11; 16], 2048, 6143),
        (2, [0x22; 16], 6144, 10239),
        (3, SWAP, 10240, 10303),
    ]);
    let chunk: u64 = 1024 * 1024;
    let mut plan = make_plan(&mut src, 1).unwrap();
    // 预置中断现场：entry_index 指向第一条（swap），续跑不做能力拒绝，重建失败即停
    let size = src.size;
    let record_path = attach_relocation(
        &mut src,
        (2048, 6143),
        (plan.grow_part, 2048, grow_end_for(&plan, 2048).unwrap()),
        reloc_for(&plan, chunk, size),
        None,
    );

    let mut logs = Vec::new();
    let o = ap(&mut src, &mut plan, chunk, true, &mut |l: &str| logs.push(l.to_string()));
    assert!(!o.is_complete(), "swap rebuild pending is not a complete run: {o:?}");
    assert_eq!(o.exit_code(), 5);
    assert!(record_path.exists(), "an uncommitted job must keep its record");
    let c = relocation_of(&src).expect("the retained record must still parse as a relocation job");
    assert_eq!((c.entry_index, c.chunks_in_entry), (0, 0));
    assert!(
        logs.iter().any(|l| l.contains("relocation incomplete")),
        "the run must say the growth was not applied: {logs:?}"
    );

    drop(src);
}

/// 续跑前的位置一致性校验：记录是持久化输入，而续跑用的 plan 由记录直接重建，
/// 拿两者互比是自循环——唯一独立证据是盘上**当前**起点。此处把一条尚未开始搬移的
/// 条目挪到别处，模拟"表在作业中断后被第三方改过"：必须拒绝，而不是把别人的布局
/// 当成自己的续跑点继续搬
#[cfg(target_os = "linux")]
#[test]
pub(crate) fn resume_refuses_when_a_pending_partition_moved_on_disk() {
    const SWAP: [u8; 16] = [0x6D, 0xFD, 0x57, 0x06, 0xAB, 0xA4, 0xC4, 0x43, 0x84, 0xE5, 0x09, 0x33, 0xC8, 0x4B, 0x4F, 0x4F];
    // root 2048..6143 | home 6144..10239 | swap 10240..12287
    let (mut src, path) = plan_fixture(&[
        (1, [0x11; 16], 2048, 6143),
        (2, [0x22; 16], 6144, 10239),
        (3, SWAP, 10240, 12287),
    ]);
    let chunk: u64 = 1024 * 1024;
    let plan = make_plan(&mut src, 1).unwrap();
    // 执行序：[swap, home]；home 的 first_lba 是 6144，目标位在 first_lba + delta
    assert!(plan.moves.len() >= 2, "the fixture must produce relocations");
    let home = plan.moves.iter().find(|m| m.part_num == 2).expect("home must be a movable");
    assert_ne!(home.delta_lba, 0, "home must need a real relocation for this test");
    // 预置"尚未开始搬移"的现场：entry_index=0 ⇒ home（moves 未达下标）必须仍在 first_lba
    let size = src.size;
    let record_path = attach_relocation(
        &mut src,
        (2048, 6143),
        (plan.grow_part, 2048, grow_end_for(&plan, 2048).unwrap()),
        reloc_for(&plan, chunk, size),
        None,
    );
    // 释放写句柄但保留目录：后面还要读写同一个镜像文件
    let _keep = src.release();

    // 篡改现场：把 home 挪到一个空闲位置（既非 first_lba、也非任何目标位），改主副本
    // 起点/终点并重算主数组与主头的 CRC。盘尾备份保持原样——解析仍有一份自洽副本
    let arr = 2 * 512usize; // 主 Entry Array 起点 LBA 2
    let e = arr + 128; // 条目 2（索引 1）
    let mut raw = std::fs::read(&path).unwrap();
    raw[e + 32..e + 40].copy_from_slice(&20000u64.to_le_bytes());
    raw[e + 40..e + 48].copy_from_slice(&24095u64.to_le_bytes());
    let arr_crc = table::crc32(&raw[arr..arr + 128 * 128]);
    raw[512 + 88..512 + 92].copy_from_slice(&arr_crc.to_le_bytes());
    raw[512 + 16..512 + 20].fill(0);
    let hdr_crc = table::crc32(&raw[512..512 + 92]);
    raw[512 + 16..512 + 20].copy_from_slice(&hdr_crc.to_le_bytes());
    std::fs::write(&path, &raw).unwrap();
    let before = std::fs::read(&path).unwrap();

    let src = plan_open(&path);
    let g = geom_of(&src);
    // 篡改确实落到几何上：home 现在停在 20000，而非记录里的 first_lba=6144
    assert_eq!(crate::gpt_policy::live_entry(&g, 2).unwrap().starting_lba, 20000);
    // 直呼事前判定，隔离出拒绝来源：必须是位置校验，而不是更靠前的占用/场景/槽位判定
    let reloc = relocation_of(&src).expect("the seeded record must still decode");
    match prepare_apply(&src, &g, &plan, chunk, true, false, None, Some(&reloc), None, &mut |_| {}) {
        Err(RunOutcome::Refused(m)) => assert!(m.contains("not where the recovery record says"), "{m}"),
        Err(other) => panic!("expected a position refusal, got {other:?}"),
        Ok(_) => panic!("expected a position refusal, but the resume was accepted"),
    }
    assert!(record_path.exists(), "a refusal must not consume the record");
    // 拒绝承诺未写盘：第三方留下的盘面（含被篡改的表）逐字节不变
    assert_eq!(std::fs::read(&path).unwrap(), before, "a refusal must leave the image byte-identical");
    drop(src);
}

/// UUID 回退：旧起点已被上一次 mkswap 的新头覆盖时，从新起点取同一身份；
/// 旧起点仍有有效 UUID 时不得被新起点的值替代
#[test]
pub(crate) fn swap_identity_falls_back_to_the_new_location_only_without_an_old_uuid() {
    fn uuid_of(i: &crate::fsops::SwapIdentity) -> Option<[u8; 16]> {
        match i {
            crate::fsops::SwapIdentity::Known { uuid, .. } => *uuid,
            _ => None,
        }
    }
    let ss = 512u64;
    let len_lba = 8u64;
    let old_start = 0u64;
    let new_start = 8u64;
    let mut data = vec![0u8; ((new_start + len_lba) * ss) as usize];
    let put = |d: &mut [u8], start_lba: u64, uuid: [u8; 16]| {
        let base = (start_lba * ss) as usize;
        d[base + 4096 - 10..base + 4096].copy_from_slice(b"SWAPSPACE2");
        d[base + 1036..base + 1052].copy_from_slice(&uuid);
    };
    // 旧起点无签名，新起点有新头 → 回退取新起点的 UUID
    put(&mut data, new_start, [0x7A; 16]);
    let src = crate::support::src_from("swapfb", &data);
    assert_eq!(uuid_of(&swap_identity_preferring(&src, old_start, new_start, len_lba, ss)), Some([0x7A; 16]));
    // 旧起点有有效头 → 以旧起点为准
    put(&mut data, old_start, [0x11; 16]);
    let src2 = crate::support::src_from("swapfb2", &data);
    assert_eq!(uuid_of(&swap_identity_preferring(&src2, old_start, new_start, len_lba, ss)), Some([0x11; 16]));
    drop(src2);
    drop(src);
}

/// swap 身份读取的字节保真：sws_volume 是 16 字节定长字段，mkswap -L 存入时不校验编码，
/// 解成 String 必改写非法序列（Latin-1 会把 0xFF 变成 U+00FF 的两个字节），
/// 于是重建 swap 时 -L 写回的值与盘上原值不同
#[test]
pub(crate) fn swap_identity_label_is_raw_bytes() {
    let ss = 512u64;
    let ps = 4096usize;
    let mut data = vec![0u8; 4 * ps];
    data[ps - 10..ps].copy_from_slice(b"SWAPSPACE2");
    data[1036..1052].copy_from_slice(&[0x5A; 16]);
    data[1052..1068].copy_from_slice(&[b'A', 0xFF, b'B', 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
    let src = crate::support::src_from("swaplabel", &data);
    let crate::fsops::SwapIdentity::Known { uuid, label } = read_swap_identity(&src, 0, data.len() as u64 / ss, ss) else {
        panic!("a labelled swap header must come back as a known identity");
    };
    assert_eq!(uuid, Some([0x5A; 16]));
    match label {
        crate::fsops::SwapLabel::Present(bytes) => assert_eq!(bytes, vec![b'A', 0xFF, b'B']),
        other => panic!("expected the raw volume label, got {other:?}"),
    }
    drop(src);
}

/// 卷标字段读不出来：必须与"确实没有卷标"分开。后者省略 -L 是合法的，
/// 前者若也省略就是把 fstab 里能看到的卷标悄悄丢掉
#[test]
pub(crate) fn swap_identity_label_read_failure_is_not_an_absent_label() {
    let ss = 512u64;
    let ps = 4096usize;
    let mut data = vec![0u8; 4 * ps];
    data[ps - 10..ps].copy_from_slice(b"SWAPSPACE2");
    data[1036..1052].copy_from_slice(&[0x5A; 16]);
    data[1052..1068].copy_from_slice(&[b'A', 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
    let src = crate::support::src_from("swaplabelfault", &data);
    let header = read_swap_identity(&src, 0, data.len() as u64 / ss, ss);
    let crate::fsops::SwapIdentity::Known { label, .. } = &header else {
        panic!("the signature and UUID are readable, only the label is not");
    };
    assert!(matches!(label, crate::fsops::SwapLabel::Present(_)), "without the fault the label is present");
    {
        let _fault = crate::dev::ReadFaultGuard::at(1052);
        match read_swap_identity(&src, 0, data.len() as u64 / ss, ss) {
            crate::fsops::SwapIdentity::Known { uuid, label: crate::fsops::SwapLabel::Unreadable(why) } => {
                assert_eq!(uuid, Some([0x5A; 16]));
                assert!(why.contains("volume label"), "{why}");
            }
            other => panic!("label read failure must not fold into an absent label: {other:?}"),
        }
    }
    drop(src);
}

/// 32K 页格式的 swap 在本机（4K 页）认不出类型：identify 走 swapon 口径给出 "unknown"，
/// 但 swap 探测认得出它。该支不写盘，不与表项的普通分区声明对账，按内容判据产出 Swap
/// 待办；此刻表已提交，待办以退出码 5 浮出，不能报完全成功
#[test]
pub(crate) fn unactivatable_swap_growth_is_pending_not_silent_success() {
    let (src, p) = plan_fixture(&[(1, [0x11; 16], 2048, 6143)]);
    let _keep = src.release();
    // 只在 32K 候选位置放 swap 签名：swapon 口径（候选集不含 32K）看不到它 → identify 给 unknown
    let base = 2048 * 512;
    let mut raw = std::fs::read(&p).unwrap();
    raw[base + 32768 - 10..base + 32768].copy_from_slice(b"SWAPSPACE2");
    std::fs::write(&p, &raw).unwrap();

    let mut src = plan_open(&p);
    // 本请求不搬移：分区内的一段字节在整轮作业里都不该被改写
    let marker = base as u64 + 4096;
    let sentinel = [0xA5u8; 512];
    src.write_at(marker, &sentinel).unwrap();
    assert_eq!(crate::fsid::identify(&src, base as u64, 4096 * 512).unwrap(), crate::fsid::ContentKind::Unknown);
    assert!(matches!(
        crate::fsid::probe_fs(&src, base as u64, 4096 * 512).unwrap().swap,
        Some(h) if !h.activatable
    ));

    // 起点不变、向右扩
    let o = rsize(&mut src,1, 2048, 8191, 1024 * 1024, false, &mut |_| {});
    match &o {
        crate::outcome::RunOutcome::Applied { pending, .. } => {
            assert_eq!(pending.len(), 1, "须报一条待办：{pending:?}");
            assert_eq!(pending[0].kind, crate::outcome::PendingKind::Swap, "{:?}", pending[0]);
        }
        other => panic!("expected Applied, got {other:?}"),
    }
    assert!(!o.is_complete(), "swap 未重建，不能报完全成功");
    assert_eq!(o.exit_code(), 5, "swap 未重建，不能报完全成功");
    let g = table::load_gpt(&src).unwrap().unwrap();
    assert_eq!(
        (g.entries[0].starting_lba, g.entries[0].ending_lba),
        (2048, 8191),
        "grow 必须把请求终点写进表项；复用旧终点或算错终点都会让分区实际未扩"
    );
    let mut after = [0u8; 512];
    src.read_at(marker, &mut after).unwrap();
    assert_eq!(after, sentinel, "不搬移的请求不得改写分区内已有的字节");

    drop(src);
}

/// 表项声明普通分区、内容是 4K 页可激活的 swap：这一支会改写（mkswap 重建），两侧
/// 对账因此在使用写原语之前执行，冲突即拒绝。表已提交，拒绝由本次写事实改判为
/// Partial（5），且不附带待办——冲突不是"未完成的后置条件"。
/// 非 root：swap 重建的能力门（`require_root`）在首次目标写入之前拒绝，走不到
/// "表已提交、对账才发现冲突"这一现场
#[cfg(target_os = "linux")]
#[test]
pub(crate) fn target_swap_conflict_is_refused_after_the_table_commit() {
    let (src, p) = plan_fixture(&[(1, [0x11; 16], 2048, 6143)]);
    let _keep = src.release();
    // 4K 页签名 + 头内自洽（version=1，last_page < 分区页数）：identify 给 Swap，
    // 与表项的普通分区声明冲突
    let base = 2048 * 512;
    let mut raw = std::fs::read(&p).unwrap();
    raw[base + 1024..base + 1028].copy_from_slice(&1u32.to_le_bytes());
    raw[base + 4096 - 10..base + 4096].copy_from_slice(b"SWAPSPACE2");
    std::fs::write(&p, &raw).unwrap();

    let mut src = plan_open(&p);
    assert_eq!(crate::fsid::identify(&src, base as u64, 4096 * 512).unwrap(), crate::fsid::ContentKind::Swap);
    let o = rsize(&mut src, 1, 2048, 8191, 1024 * 1024, false, &mut |_| {});
    // 非 root：能力门在首个目标写入前把这次 resize 拒成 NoMutation(4)，表项仍是旧值
    if unsafe { libc::geteuid() } != 0 {
        match &o {
            crate::outcome::RunOutcome::NoMutation { cause } => {
                assert!(cause.contains("root required"), "{cause}");
            }
            other => panic!("non-root must stop at the privilege gate before writing, got {other:?}"),
        }
        assert_eq!(o.exit_code(), 4);
        let g = table::load_gpt(&src).unwrap().unwrap();
        assert_eq!(
            (g.entries[0].starting_lba, g.entries[0].ending_lba),
            (2048, 6143),
            "the refusal precedes the table write: the entry still names the old end"
        );
        drop(src);
        return;
    }
    match &o {
        crate::outcome::RunOutcome::Partial { cause, pending, .. } => {
            assert!(cause.contains("the two readings disagree"), "{cause}");
            assert!(pending.is_empty(), "冲突不是未完成的后置条件，不带待办：{pending:?}");
        }
        other => panic!("expected Partial, got {other:?}"),
    }
    assert_eq!(o.exit_code(), 5);
    let g = table::load_gpt(&src).unwrap().unwrap();
    assert_eq!(
        (g.entries[0].starting_lba, g.entries[0].ending_lba),
        (2048, 8191),
        "冲突在表提交之后才发现：表项终点已是新值"
    );

    drop(src);
}

/// FS 步留下待办时记录保留，重跑同一请求落到"已提交、补 FS 步"分支：
/// 重跑既不能静默完成（旧区间=新区间 ⇒ Leave ⇒ 报 0 而 swap 仍未重建），
/// 也不得再次搬动数据。夹具带搬移：chunk 循环的逐 chunk save 是这条路径的落盘来源
#[cfg(target_os = "linux")]
#[test]
pub(crate) fn swap_pending_keeps_record_and_rerun_retries_the_fs_step() {
    let (src, p) = plan_fixture(&[(1, [0x11; 16], 2048, 10239)]);
    let _keep = src.release();
    // 32K 候选位置放 swap 签名：identify 认不出它；搬移把它一并拷到新区间的
    // 同一相对偏移，收尾的 swap 探测在新区间探得到 → 记成待办
    let base = 2048 * 512;
    let mut raw = std::fs::read(&p).unwrap();
    raw[base + 32768 - 10..base + 32768].copy_from_slice(b"SWAPSPACE2");
    std::fs::write(&p, &raw).unwrap();

    let mut src = plan_open(&p);
    let record_path = attach_record(&mut src, crate::recovery::OpKind::Resize, (2048, 10239), (1, 12288, 24575));
    // 旧区间哨兵：正向搬移不与目标区重叠，旧区字节在整轮作业里都不该被改写
    let marker = base as u64 + 4096;
    let sentinel = [0xA5u8; 512];
    src.write_at(marker, &sentinel).unwrap();

    // move+grow：起点 2048 → 12288，区间 8192 LBA → 12288 LBA，
    // 拷贝量取 min = 4 MiB ⇒ 4 个 chunk
    let o1 = rsize(&mut src, 1, 12288, 24575, 1024 * 1024, false, &mut |_| {});
    assert_eq!(o1.exit_code(), 5, "{o1:?}");
    assert!(record_path.exists(), "FS 步留下待办时记录必须保留——重跑只能靠它落到续传分支");

    let o2 = rsize(&mut src, 1, 12288, 24575, 1024 * 1024, false, &mut |_| {});
    match &o2 {
        crate::outcome::RunOutcome::Applied { pending, .. } => {
            assert_eq!(pending.len(), 1, "重跑须再次报出 swap 待办：{pending:?}");
        }
        other => panic!("expected Applied, got {other:?}"),
    }
    assert!(!o2.is_complete(), "重跑不得把未完成的 FS 步报成完成");
    assert_eq!(o2.exit_code(), 5);
    assert!(record_path.exists(), "重跑仍留待办时记录继续保留");

    let mut after = [0u8; 512];
    src.read_at(marker, &mut after).unwrap();
    assert_eq!(after, sentinel, "续传只重试 FS 步，不得再次搬动数据");
    let g2 = table::load_gpt(&src).unwrap().unwrap();
    assert_eq!((g2.entries[0].starting_lba, g2.entries[0].ending_lba), (12288, 24575));

    drop(src);
}

/// 左移（Δ<0）在"表已提交、收尾未做"窗口的重跑：记录游标停在已搬完的条目上
/// （`chunks_in_entry` == 块数、条目游标未推进），盘上表项已在记录的目的位。
/// 命令层必须按 `src_first + delta` 的有符号算式认出目的位并把这一条收尾，
/// 而不是按"未完成"拒绝，也不得把负位移取绝对值后比对
#[cfg(target_os = "linux")]
#[test]
pub(crate) fn leftward_move_finishes_from_the_committed_table() {
    let (mut src, p) = plan_fixture(&[
        (1, [0x11; 16], 1024, 2047),
        (2, [0x22; 16], 4096, 8191),
    ]);
    let chunk: u64 = 1024 * 1024;
    let reloc = crate::recovery::Relocation {
        entries: vec![crate::recovery::RelocEntry {
            partition: 2,
            src_first: 4096,
            len: 4096,
            delta: -2048,
            is_swap: false,
        }],
        entry_index: 0,
        // 2 MiB / 1 MiB：数据已搬完，游标还停在这一条
        chunks_in_entry: 2,
        chunk_bytes: chunk,
        kind: crate::recovery::RelocKind::Shift,
        grow_part: 0,
        disk_size: src.size,
        ss: 512,
    };
    let record_path = attach_record(&mut src, crate::recovery::OpKind::Move, (4096, 8191), (2, 2048, 6143));
    src.record_journal_mut()
        .expect("attach_record adopts a record journal")
        .bind_relocation(reloc, None)
        .expect("a legal relocation must bind");
    let _keep = src.release();

    // 表已提交：p2 已在记录的目的位 2048..6143
    {
        use gptman::GPT;
        let mut raw = std::fs::read(&p).unwrap();
        let mut cur = std::io::Cursor::new(&mut raw);
        let mut gpt = GPT::read_from(&mut cur, 512).unwrap();
        gpt[2].starting_lba = 2048;
        gpt[2].ending_lba = 6143;
        gpt.write_into(&mut cur).unwrap();
        std::fs::write(&p, raw).unwrap();
    }

    let req = crate::cli::MoveRequest {
        target: p.display().to_string(),
        part: Some(crate::dev::PartSelector::Number(2)),
        sector_size: None,
        start: Some(crate::cli::StartSelector::Lba(2048)),
        align: crate::domain::Align::none(),
        align_given: false,
        chunk_mib: 1,
        no_fs: false,
        yes: true,
    };
    let o = crate::cmd::layout::cmd_move(&req);
    assert_eq!(o.exit_code(), 0, "the rerun must finish the move: {o:?}");
    assert!(o.is_complete(), "the rerun must finish the move: {o:?}");
    assert!(!record_path.exists(), "a finished move must release the record");

    let reopened = plan_open(&p);
    let g = table::load_gpt(&reopened).unwrap().unwrap();
    assert_eq!(
        (g.entries[1].starting_lba, g.entries[1].ending_lba),
        (2048, 6143),
        "收尾只释放记录：表项必须仍在记录的目的位"
    );
    drop(reopened);
}

/// 纯扩容（不搬移）FS 步留下待办时记录同样保留、重跑补试 FS 步：
/// 无搬移就没有 chunk 循环的 save 落点，写表前的这一次落盘是重跑落到
/// "已提交、补 FS 步"分支的唯一凭据——缺了它重跑按现算几何算出
/// 旧区间 == 新区间 ⇒ Leave ⇒ 报 0，待办从此无人认领
#[cfg(target_os = "linux")]
#[test]
pub(crate) fn pure_growth_fs_pending_keeps_record_and_rerun_retries() {
    let (src, p) = plan_fixture(&[(1, [0x11; 16], 2048, 6143)]);
    let _keep = src.release();
    // 32K 候选位置放 swap 签名：identify 认不出它，FS 步按"不可激活的 swap"记待办
    let base = 2048 * 512;
    let mut raw = std::fs::read(&p).unwrap();
    raw[base + 32768 - 10..base + 32768].copy_from_slice(b"SWAPSPACE2");
    std::fs::write(&p, &raw).unwrap();

    let mut src = plan_open(&p);
    let record_path = attach_record(&mut src, crate::recovery::OpKind::Resize, (2048, 6143), (1, 2048, 8191));

    let o1 = rsize(&mut src, 1, 2048, 8191, 1024 * 1024, false, &mut |_| {});
    assert_eq!(o1.exit_code(), 5, "{o1:?}");
    assert!(record_path.exists(), "纯扩容的 FS 待办同样要留记录——重跑只能靠它落到补试分支");
    let g1 = table::load_gpt(&src).unwrap().unwrap();
    assert_eq!(
        (g1.entries[0].starting_lba, g1.entries[0].ending_lba),
        (2048, 8191),
        "纯扩容也必须在表项里真正落终点"
    );

    let o2 = rsize(&mut src, 1, 2048, 8191, 1024 * 1024, false, &mut |_| {});
    match &o2 {
        crate::outcome::RunOutcome::Applied { pending, .. } => {
            assert_eq!(pending.len(), 1, "重跑须再次报出 swap 待办：{pending:?}");
        }
        other => panic!("expected Applied, got {other:?}"),
    }
    assert!(!o2.is_complete(), "重跑不得把未完成的 FS 步报成完成");
    assert_eq!(o2.exit_code(), 5);
    let g2 = table::load_gpt(&src).unwrap().unwrap();
    assert_eq!(
        (g2.entries[0].starting_lba, g2.entries[0].ending_lba),
        (2048, 8191),
        "补试 FS 步的重跑不得把终点缩回旧值"
    );

    drop(src);
}
