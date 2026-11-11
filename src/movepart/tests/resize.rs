#![allow(clippy::let_underscore_must_use)] // 测试的清理步骤有意忽略失败
use super::*;
use super::support::*;
#[cfg(target_os = "linux")]
use gptman::{GPT, GPTPartitionEntry};
#[cfg(target_os = "linux")]
use std::io::Cursor;

/// HiddenSectors 修正：0x1C 处写入 LE32 起始 LBA；≥2^32 值取 0（纯函数分支 + 落盘各验一次）
#[test]
pub(crate) fn ntfs_hidden_sectors_fix() {
    assert_eq!(ntfs_hidden_value(30687), (30687, false));
    assert_eq!(ntfs_hidden_value(u32::MAX as u64), (u32::MAX, false));
    assert_eq!(ntfs_hidden_value(1u64 << 32), (0, true));
    let (src, path) = plan_fixture(&[(1, [0x11; 16], 2048, 6143)]);
    let _keep = src.release();
    let mut src = plan_open(&path);
    // 写入前复核引导扇区签名：目标扇区先垫一个最小 NTFS 引导扇区
    let mut boot = vec![0u8; 512];
    boot[3..11].copy_from_slice(b"NTFS    ");
    src.write_at(30687 * 512, &boot).unwrap();
    let mut log = |_: &str| {};
    fix_ntfs_hidden_sectors(&mut src, 30687, 512, 1, &mut log).unwrap();
    let mut b = [0u8; 4];
    src.read_at(30687 * 512 + 0x1C, &mut b).unwrap();
    assert_eq!(b, 30687u32.to_le_bytes());
}

/// move 中断续传：模拟"chunk 0 已搬完即崩溃"——预置记录进度（transfer_cursor=1）
/// + 手工完成 chunk 0 的复制 + 污染源首字节（若续传错误地重搬 chunk 0 会被检出）
#[cfg(target_os = "linux")]
#[test]
pub(crate) fn resize_part_resume_skips_done_chunks() {
    let ss = 512u64;
    let data = vec![0u8; 16 * 1024 * 1024];
    let mut cur = Cursor::new(data);
    let mut gpt = GPT::new_from(&mut cur, ss, [0xAB; 16]).unwrap();
    gpt[1] = GPTPartitionEntry {
        partition_type_guid: [0x11; 16],
        unique_partition_guid: [0x12; 16],
        starting_lba: 2048,
        ending_lba: 10239, // 8192 扇区 = 4 MiB = 4×1MiB chunk
        attribute_bits: 0,
        partition_name: "a".into(),
    };
    gpt.write_into(&mut cur).unwrap();
    let dir = crate::testdir::test_dir("resume");
    let tmp = dir.join("resume.img");
    std::fs::write(&tmp, cur.into_inner()).unwrap();
    let mut src = plan_open(&tmp);
    // gptman 只写 GPT 结构，保护 MBR 需自行补——load_gpt 以前者为前置
    crate::table::ensure_protective_mbr(&mut src, ss).unwrap();

    let chunk: u64 = 1024 * 1024;
    let src_off = 2048u64 * 512;
    let dst_off = 12288u64 * 512; // delta = +4096 扇区
    // 每块填不同指纹
    let mut w = |off: u64, b: u8| {
        let buf = vec![b; chunk as usize];
        src.write_at(off, &buf).unwrap();
    };
    w(src_off, 0x10);
    w(src_off + chunk, 0x11);
    w(src_off + 2 * chunk, 0x12);
    w(src_off + 3 * chunk, 0x13);
    // 模拟崩溃前已完成 chunk 3（右移 = 尾→头推进，chunks_done 按执行序计数）：
    // 手工复制尾块到目标
    let mut last = vec![0u8; chunk as usize];
    src.read_at(src_off + 3 * chunk, &mut last).unwrap();
    src.write_at(dst_off + 3 * chunk, &last).unwrap();
    // 污染源尾块首字节：若续传错误重搬 chunk 3，目标尾块会变 0xEE
    src.write_at(src_off + 3 * chunk, &[0xEEu8]).unwrap();

    // 预置记录：已同步落盘 1 块。本请求的新区间与下面的 rsize 入参同值，
    // 摘要按生产的取法绑定（操作取记录自己的，扇区与盘大小取目标现场）
    let (new_start, new_end) = (12288u64, 20479u64);
    let record_path = src.identity.record_path().expect("image targets have a record path").to_path_buf();
    std::fs::create_dir_all(record_path.parent().unwrap()).unwrap();
    let probe = crate::platform::probe::probe_of(&src).unwrap();
    let scene = crate::recovery::SceneIdentity::of_source(&probe, &src);
    let mut journal = crate::recovery::RecordJournal::create_image(
        &record_path,
        scene,
        crate::recovery::OpKind::ResizePart,
        ss,
        crate::dev::DiskGuidState::Unavailable,
        None,
    )
    .unwrap();
    let digest = crate::recovery::PlanDigest::of_request(
        journal.op(),
        1,
        new_start,
        new_end,
        ss,
        src.size,
    );
    journal.bind_plan(digest).expect("a fresh record binds the request");
    journal.bind_old_range((2048, 10239)).unwrap();
    journal.record_progress(1, 1, 0).unwrap();
    src.adopt_record_journal(journal);

    // 续传：跳过 chunk 0，补齐 chunk 1..3，提交表项
    let o = rsize(&mut src, 1, new_start, new_end, chunk, false, &mut |_| {});
    assert!(o.is_layout_applied(), "resize must apply (exit {})", o.exit_code());

    // 终验：目标 4 块 = 指纹 0x10..0x13（chunk 0 未被重搬污染）
    for i in 0..4u8 {
        let mut buf = vec![0u8; chunk as usize];
        src.read_at(dst_off + i as u64 * chunk, &mut buf).unwrap();
        assert!(buf.iter().all(|&b| b == 0x10 + i), "chunk {i} mismatch");
    }
    let g2 = table::load_gpt(&src).unwrap().unwrap();
    assert_eq!(g2.entries[0].starting_lba, 12288);
    assert_eq!(g2.entries[0].ending_lba, 20479);
    assert!(record_path.exists(), "收口由命令层做，resize_part 自身不删记录");
    drop(src);
}

/// 续传现场由生产路径产出：注入一次读失败打断 move，记录里的进度必须停在已同步
/// 落盘的那一块；续跑以该边界继续，把失败后剩下的块搬完。与上面的手工预置
/// 用例互补——那条证明"给定进度时续传行为正确"，这条证明"生产确实写对了进度"
#[cfg(target_os = "linux")]
#[test]
pub(crate) fn resize_crash_records_durable_progress() {
    let (mut src, _path) = plan_fixture(&[(1, [0x11; 16], 2048, 10239)]);
    let chunk: u64 = 1024 * 1024;
    let src_off = 2048u64 * 512;
    let dst_off = 4096u64 * 512;
    for i in 0..4u8 {
        src.write_at(src_off + i as u64 * chunk, &vec![0x10 + i; 512]).unwrap();
    }
    // 本请求的新区间与下面的 rsize 入参同值；摘要按生产的取法在首次落盘前绑定
    let (new_start, new_end) = (4096u64, 12287u64);
    let record_path = src.identity.record_path().expect("image targets have a record path").to_path_buf();
    std::fs::create_dir_all(record_path.parent().unwrap()).unwrap();
    let probe = crate::platform::probe::probe_of(&src).unwrap();
    let scene = crate::recovery::SceneIdentity::of_source(&probe, &src);
    let mut journal = crate::recovery::RecordJournal::create_image(
        &record_path,
        scene,
        crate::recovery::OpKind::Resize,
        512,
        crate::dev::DiskGuidState::Unavailable,
        None,
    )
    .unwrap();
    let digest = crate::recovery::PlanDigest::of_request(
        journal.op(),
        1,
        new_start,
        new_end,
        512,
        src.size,
    );
    journal.bind_plan(digest).expect("a fresh record binds the request");
    journal.bind_old_range((2048, 10239)).unwrap();
    src.adopt_record_journal(journal);

    // 右移 1 MiB（自重叠，块收敛到 1 MiB）：尾→头推进，第 2 轮读失败时首块已同步落盘
    let fault = crate::dev::ReadFaultGuard::at(src_off + 2 * chunk);
    let o = rsize(&mut src, 1, new_start, new_end, chunk, true, &mut |_| {});
    assert_eq!(o.exit_code(), 5, "mid-move read failure must stay Partial: {o:?}");
    drop(fault);

    let progress = src.record_journal().expect("the fixture attaches a record").progress();
    assert_eq!(progress.transfer_cursor, 1, "只有已同步落盘的 chunk 可以进入 durable 边界");

    // 已完成块（within = 3 MiB）的源区改写成垃圾：续跑若从 0 重来会把它带回目标区
    src.write_at(src_off + 3 * chunk, &[0xEE; 512]).unwrap();
    let resumed = rsize(&mut src, 1, new_start, new_end, chunk, true, &mut |_| {});
    assert!(resumed.is_complete(), "the resume must finish the move: {}", resumed.exit_code());
    for i in 0..4u8 {
        let mut b = [0u8; 512];
        src.read_at(dst_off + i as u64 * chunk, &mut b).unwrap();
        assert_eq!(b, [0x10 + i; 512], "chunk {i} was copied from the wrong source block");
    }
    let g = table::load_gpt(&src).unwrap().unwrap();
    assert_eq!((g.entries[0].starting_lba, g.entries[0].ending_lba), (4096, 12287));
    assert!(record_path.exists(), "收口由命令层在拿到完整结果后做");
}

/// 记录型"分区层已提交、FS 收尾未做"的现场：表项停在目标区间，记录里留着搬移前几何。
/// 分区首扇区写一个 LVM PV 标签——FS 判据认它，且它的扩容不需要外部工具
#[cfg(target_os = "linux")]
fn committed_record_fixture(part: u32, op: crate::recovery::OpKind, old: (u64, u64), new: (u64, u64)) -> crate::testdir::TestSource<WriteSource> {
    let ss = 512u64;
    let (mut src, _path) = plan_fixture(&[(1, [0x11; 16], old.0, old.1)]);

    let mut g = table::load_gpt(&src).unwrap().unwrap();
    g.entries[0].starting_lba = new.0;
    g.entries[0].ending_lba = new.1;
    let last_lba = src.size / ss - 1;
    table::commit_gpt(&mut src, &g, last_lba).unwrap();
    table::ensure_protective_mbr(&mut src, ss).unwrap();

    let mut label = [0u8; 32];
    label[..8].copy_from_slice(b"LABELONE");
    label[24..32].copy_from_slice(b"LVM2 001");
    src.write_at(new.0 * ss + 512, &label).unwrap();

    let record_path = src.identity.record_path().expect("image targets have a record path").to_path_buf();
    std::fs::create_dir_all(record_path.parent().unwrap()).unwrap();
    let probe = crate::platform::probe::probe_of(&src).unwrap();
    let scene = crate::recovery::SceneIdentity::of_source(&probe, &src);
    let mut journal = crate::recovery::RecordJournal::create_image(
        &record_path,
        scene,
        op,
        ss,
        crate::dev::DiskGuidState::Unavailable,
        None,
    )
    .unwrap();
    let digest = crate::recovery::PlanDigest::of_request(journal.op(), part, new.0, new.1, ss, src.size);
    journal.bind_plan(digest).expect("a fresh record binds the request");
    journal.bind_old_range(old).unwrap();
    src.adopt_record_journal(journal);
    src
}

/// 记录型续跑：分区层提交之后崩、重跑必须把 FS 步留下。
///
/// 已提交的现场盘上只剩新区间，记录里的搬移前几何是判断该不该扩的唯一依据。
/// 若拿盘上现状当"原尺寸"，`old_bytes == new_bytes`，FS 步被判成 Leave——分区扩了、
/// 文件系统没扩，命令却报成功。`resize` 与 `resize-part` 各一条
#[cfg(target_os = "linux")]
#[test]
pub(crate) fn record_resume_after_commit_keeps_the_filesystem_step() {
    let old = (2048u64, 6143u64);
    let new = (2048u64, 8191u64);
    for (op, name) in [
        (crate::recovery::OpKind::Resize, "resize"),
        (crate::recovery::OpKind::ResizePart, "resize-part"),
    ] {
        let src = committed_record_fixture(1, op, old, new);
        let (g, repair) = gpt_policy::resolve_geometry(&src).unwrap().unwrap();
        let p = crate::cmd::layout::prepare_resize_part(
            &src, &g, repair, 1, new.0, new.1, 1024 * 1024, false, None, false, &mut |_| {},
        )
        .unwrap_or_else(|f| panic!("{name}: a committed record must resume, got {f:?}"));
        assert_eq!(
            p.committed_old,
            Some(old),
            "{name}: the pre-commit range must come from the record, not from the on-disk entry"
        );
        assert_eq!(
            p.fs_action,
            FsAction::Grow,
            "{name}: the filesystem step must not be skipped after a committed resume"
        );
    }
}

/// `--no-fs` 与缩容不可共存：分区末端会切进未缩的 FS 元数据。判定必须在写盘之前，
/// 此刻确实一字未写
#[test]
pub(crate) fn no_fs_shrink_is_refused_before_any_write() {
    let chunk: u64 = 1024 * 1024;
    let (src, p) = plan_fixture(&[(1, [0x11; 16], 2048, 6143)]);
    let _keep = src.release();
    make_stale(&p);
    let mut src = plan_open(&p);
    let o = rsize(&mut src, 1, 2048, 4095, chunk, true, &mut |_| {});
    assert_eq!(o.exit_code(), 3, "--no-fs 缩容属事前拒绝");
    assert!(
        matches!(table::load_gpt(&src).unwrap().unwrap().state, table::GptState::NeedsRepair { .. }),
        "--no-fs 缩容拒绝不得写盘"
    );
    drop(src);
}

/// 续跑前的回读：读不出来就报"无从判断"，不得按"未缩"或"已缩"任一侧猜——
/// 猜"已缩"会接着搬数据切进未缩的元数据，猜"未缩"会把同一次缩容再做一遍
#[test]
pub(crate) fn shrink_state_readback_reports_unknown_instead_of_guessing() {
    let (src, _p) = plan_fixture(&[(1, [0x11; 16], 2048, 6143)]);
    // 分区里没有可识别的 FS：任何回读都不可能成功
    assert_eq!(
        crate::fsops::shrink_already_at(&src, 1, Some(crate::fsid::ContentKind::Filesystem(crate::fsid::FsKind::Ext)), 1024 * 1024),
        None,
        "an unreadable filesystem must not be reported as shrunk or unshrunk"
    );
    assert_eq!(crate::fsops::shrink_already_at(&src, 1, None, 1024 * 1024), None);
}

/// 记录里带着 LVM 收尾事实，而本次入口不跑链（`resize-part` / `move` 传 defer = false）：
/// 盘上表已提交到新几何，重跑判定已提交，LVM 那一步仍没人做。未做的链以 Pending 浮出、
/// 退出码 5；记录留给调用方——基线只有它能给
#[cfg(target_os = "linux")]
#[test]
pub(crate) fn recorded_lvm_facts_surface_the_unrun_chain() {
    let chunk: u64 = 1024 * 1024;
    let old = (2048u64, 6143u64);
    let new = (4096u64, 8191u64);
    let mut src = committed_record_fixture(1, crate::recovery::OpKind::Resize, old, new);
    src.record_journal_mut()
        .expect("the fixture attaches a record")
        .record_lvm_post(crate::recovery::LvmPost {
            lv_size_before: 1 << 20,
            lv_target: (1 << 20) + 8 * 4096,
            extent_bytes: 4096,
        })
        .unwrap();

    let o = rsize(&mut src, 1, new.0, new.1, chunk, true, &mut |_| {});
    assert_eq!(o.exit_code(), 5, "the unrun LVM chain is a pending step: {o:?}");
    match &o {
        RunOutcome::Applied { pending, .. } => {
            assert_eq!(pending.len(), 1, "exactly the LVM chain must be reported: {pending:?}");
            assert!(pending[0].detail.contains("pvresize"), "{:?}", pending[0]);
            assert!(pending[0].hint.contains("re-run"), "{:?}", pending[0]);
        }
        other => panic!("expected Applied, got {other:?}"),
    }
    let kept = src.record_journal().and_then(|j| j.lvm_post()).expect("the record keeps the chain baseline");
    assert_eq!(kept.lv_target, (1 << 20) + 8 * 4096);
}

/// 32K 页格式的 swap 在本机（4K 页）认不出类型：identify 走 swapon 口径给出 "unknown"，
/// 但 swap 探测认得出它。该支不写盘，不与表项的普通分区声明对账，按内容判据产出 Swap
/// 待办；此刻表已提交，待办以退出码 5 浮出，不能被当成成功
#[test]
pub(crate) fn pending_fs_step_is_reported() {
    let chunk: u64 = 1024 * 1024;
    let (src, p) = plan_fixture(&[(1, [0x11; 16], 2048, 6143)]);
    let _keep = src.release();
    // 只在 32K 候选位置放 swap 签名：identify 给 unknown，但 swap 探测认得出它，
    // 本机激活不了它，收尾按"扩了但未重建"记待办
    let base = 2048 * 512;
    let mut raw = std::fs::read(&p).unwrap();
    raw[base + 32768 - 10..base + 32768].copy_from_slice(b"SWAPSPACE2");
    std::fs::write(&p, &raw).unwrap();

    let mut src = plan_open(&p);
    // 纯扩容没有搬移：分区内的一段字节在整轮作业里都不该被改写
    let marker = base as u64 + 4096;
    let sentinel = [0xA5u8; 512];
    src.write_at(marker, &sentinel).unwrap();
    let o = rsize(&mut src, 1, 2048, 8191, chunk, false, &mut |_| {});
    assert_eq!(o.exit_code(), 5, "{o:?}");
    match &o {
        RunOutcome::Applied { pending, .. } => {
            assert_eq!(pending.len(), 1, "the swap rebuild is one pending step: {pending:?}");
            assert_eq!(pending[0].kind, crate::outcome::PendingKind::Swap, "{:?}", pending[0]);
        }
        other => panic!("expected Applied, got {other:?}"),
    }
    assert!(!o.is_complete(), "a pending swap rebuild must not report completion");
    let g = table::load_gpt(&src).unwrap().unwrap();
    assert_eq!(
        (g.entries[0].starting_lba, g.entries[0].ending_lba),
        (2048, 8191),
        "grow 必须把请求终点写进表项"
    );
    let mut after = [0u8; 512];
    src.read_at(marker, &mut after).unwrap();
    assert_eq!(after, sentinel, "纯扩容不得改写分区内已有的字节");

    drop(src);
}
