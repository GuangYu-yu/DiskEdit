#![allow(clippy::let_underscore_must_use)] // 测试的清理步骤有意忽略失败
use super::*;
use super::support::*;
use gptman::{GPT, GPTPartitionEntry};
use std::io::Cursor;

/// prepare 与首写之间第三方改表：执行半程须以携带的几何复核拒绝，目标区一字未写。
/// 源区首扇区填非零——若拒绝失效，搬移会把这些字节带进目标区
#[test]
pub(crate) fn execute_resize_refuses_when_table_changed_after_prepare() {
    let ss = 512u64;
    let (mut src, path) = plan_fixture(&[(1, [0x11; 16], 2048, 6143)]);
    src.write_at(2048 * ss, &[0xA5u8; 512]).unwrap();
    let (g, repair) = gpt_policy::resolve_geometry(&src).unwrap().unwrap();
    let mut log = |_: &str| {};
    let p = prepare_resize(&src, &g, repair, 1, 8192, 12287, 1024 * 1024, true, false, None, &mut log).unwrap();
    tamper_disk_guid(&mut src);
    let before = std::fs::read(&path).unwrap();
    let mut post = PostWrite::new();
    let r = execute_resize(&mut src, p, &mut |_| {}, &mut post);
    assert!(r.is_err(), "execute must refuse a table changed after prepare");
    let err = r.unwrap_err().to_string();
    assert!(err.contains("changed outside this tool"), "unexpected error: {err}");
    let mut b = vec![0u8; 512];
    src.read_at(8192 * ss, &mut b).unwrap();
    assert!(b.iter().all(|&x| x == 0), "target must stay untouched");
    // 目标首扇区只是整段里的一格：拒绝承诺的是整镜像一字未写（含表与源区）
    assert_eq!(std::fs::read(&path).unwrap(), before, "a refusal must leave the image byte-identical");
    drop(src);
    let _ = std::fs::remove_file(&path);
}

/// 纯扩容（不搬移）同样不得先落 checkpoint 再复核：复核失败时恢复路径里没有新增
/// 现场，目标盘一字未写
#[test]
pub(crate) fn pure_growth_refuses_changed_table_before_any_checkpoint_is_written() {
    let (src, path) = plan_fixture(&[(1, [0x11; 16], 2048, 6143)]);
    drop(src);
    // 32K 位置的 swap 签名：identify 认不出，扩容器按"不可激活的 swap"放行
    let base = 2048 * 512;
    let mut raw = std::fs::read(&path).unwrap();
    raw[base + 32768 - 10..base + 32768].copy_from_slice(b"SWAPSPACE2");
    std::fs::write(&path, &raw).unwrap();
    let mut src = plan_open(&path);
    let ckpt_path = src.identity.checkpoint_path().to_path_buf();
    let (g, repair) = gpt_policy::resolve_geometry(&src).unwrap().unwrap();
    let chunk: u64 = 1024 * 1024;
    let p = prepare_resize(&src, &g, repair, 1, 2048, 8191, chunk, false, false, None, &mut |_| {}).unwrap();
    assert_eq!(p.fs_action, FsAction::Grow);
    assert_eq!(p.new_start, p.old_start, "this regression covers the pure-growth branch");
    assert!(!ckpt_path.exists(), "prepare must not create a checkpoint");
    tamper_disk_guid(&mut src);
    let before = std::fs::read(&path).unwrap();
    let mut post = PostWrite::new();
    let r = execute_resize(&mut src, p, &mut |_| {}, &mut post);
    let err = r.expect_err("execute must refuse a table changed after prepare").to_string();
    assert!(err.contains("changed outside this tool"), "unexpected error: {err}");
    assert!(!ckpt_path.exists(), "a failed pre-write recheck must not leave a checkpoint");
    assert_eq!(std::fs::read(&path).unwrap(), before, "the target must stay byte-identical");
    drop(src);
    let _ = std::fs::remove_file(&path);
    let _ = crate::dev::remove_recovery_file(&ckpt_path);
}

/// HiddenSectors 修正：0x1C 处写入 LE32 起始 LBA；≥2^32 值取 0（纯函数分支 + 落盘各验一次）
#[test]
pub(crate) fn ntfs_hidden_sectors_fix() {
    assert_eq!(ntfs_hidden_value(30687), (30687, false));
    assert_eq!(ntfs_hidden_value(u32::MAX as u64), (u32::MAX, false));
    assert_eq!(ntfs_hidden_value(1u64 << 32), (0, true));
    let (src, path) = plan_fixture(&[(1, [0x11; 16], 2048, 6143)]);
    drop(src);
    let f = std::fs::OpenOptions::new().read(true).write(true).open(&path).unwrap();
    let size = std::fs::metadata(&path).unwrap().len();
    let mut src = WriteSource::unowned(FileSource {
        identity: crate::dev::TargetIdentity::resolve_image(&path),
        file: f,
        path: path.clone(),
        sector_size: 512,
        size,
        is_block: false,
        journal: None,
        fingerprint: Default::default(),
        loop_mapping: None,
    });
    let mut log = |_: &str| {};
    fix_ntfs_hidden_sectors(&mut src, 30687, 512, &mut log).unwrap();
    let mut b = [0u8; 4];
    src.read_at(30687 * 512 + 0x1C, &mut b).unwrap();
    assert_eq!(b, 30687u32.to_le_bytes());
}

/// move 中断续传：模拟"chunk 0 已搬完即崩溃"——预置 checkpoint（chunks_done=1）
/// + 手工完成 chunk 0 的复制 + 污染源首字节（若续传错误地重搬 chunk 0 会被检出）
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
    let mut tmp = std::env::temp_dir();
    tmp.push(format!("diskedit_resume_{}.img", std::process::id()));
    std::fs::write(&tmp, cur.into_inner()).unwrap();
    let f = std::fs::OpenOptions::new().read(true).write(true).open(&tmp).unwrap();
    let size = std::fs::metadata(&tmp).unwrap().len();
    let mut src = WriteSource::unowned(FileSource {
        identity: crate::dev::TargetIdentity::resolve_image(&tmp),
        file: f,
        path: tmp.clone(),
        sector_size: 512,
        size,
        is_block: false,
        journal: None,
        fingerprint: Default::default(),
        loop_mapping: None,
    });
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

    // 预置 checkpoint：chunks_done = 1
    let ckpt_path = src.identity.checkpoint_path().to_path_buf();
    let ckpt = RsCheckpoint {
        proof: crate::dev::LiveProof::Inode,
        disk_size: size, ss, part: 1,
        old_start: 2048, old_end: 10239, new_start: 12288, new_end: 20479,
        fs_shrunk: FsShrink::NotShrunk, chunks_done: 1, chunk_bytes: chunk,
        fp: Default::default(),
        map: None,
        lvm: None,
        fstype: None,
    };
    atomic_write_ckpt(&ckpt_path, &ckpt.serialize()).unwrap();

    // 续传：跳过 chunk 0，补齐 chunk 1..3，提交表项
    let o = rsize(&mut src,1, 12288, 20479, chunk, false, &mut |_| {});
    assert!(o.has_layout_write(), "resize must apply (exit {})", o.exit_code());

    // 终验：目标 4 块 = 指纹 0x10..0x13（chunk 0 未被重搬污染）
    for i in 0..4u8 {
        let mut buf = vec![0u8; chunk as usize];
        src.read_at(dst_off + i as u64 * chunk, &mut buf).unwrap();
        assert!(buf.iter().all(|&b| b == 0x10 + i), "chunk {i} mismatch");
    }
    let g2 = table::load_gpt(&src).unwrap().unwrap();
    assert_eq!(g2.entries[0].starting_lba, 12288);
    assert_eq!(g2.entries[0].ending_lba, 20479);
    assert!(!ckpt_path.exists(), "checkpoint must be removed on success");
    drop(src);
    let _ = std::fs::remove_file(&tmp);
    let _ = crate::dev::remove_recovery_file(&ckpt_path);
}

/// 续传现场由生产路径产出：注入一次读失败打断 move，盘上 ckpt 的进度必须停在已同步
/// 落盘的那一块；续跑以该 ckpt 为边界继续，把失败后剩下的块搬完。与上面的手工预置
/// 用例互补——那条证明"给定 ckpt 时续传行为正确"，这条证明"生产确实写对了 ckpt"
#[test]
pub(crate) fn resize_crash_checkpoint_records_durable_progress() {
    let (mut src, path) = plan_fixture(&[(1, [0x11; 16], 2048, 10239)]);
    let ckpt_path = src.identity.checkpoint_path().to_path_buf();
    let chunk: u64 = 1024 * 1024;
    let src_off = 2048u64 * 512;
    let dst_off = 4096u64 * 512;
    for i in 0..4u8 {
        src.write_at(src_off + i as u64 * chunk, &vec![0x10 + i; 512]).unwrap();
    }
    // 右移 1 MiB（自重叠，块收敛到 1 MiB）：尾→头推进，第 2 轮读失败时首块已同步落盘
    let fault = crate::dev::ReadFaultGuard::at(src_off + 2 * chunk);
    let o = rsize(&mut src,1, 4096, 12287, chunk, true, &mut |_| {});
    assert_eq!(o.exit_code(), 30, "mid-move read failure must stay Failed: {o:?}");
    drop(fault);

    let (slot, _) = read_checkpoint(&src, &geom_of(&src)).unwrap();
    let c = match slot {
        CheckpointSlot::Resize(c) => c,
        _ => panic!("a failed resize must leave an Rs checkpoint"),
    };
    assert_eq!(c.part, 1);
    assert_eq!((c.old_start, c.old_end, c.new_start, c.new_end), (2048, 10239, 4096, 12287));
    assert_eq!(c.chunk_bytes, chunk);
    assert_eq!(c.chunks_done, 1, "只有已同步落盘的 chunk 可以进入 durable 边界");

    // 已完成块（within = 3 MiB）的源区改写成垃圾：续跑若从 0 重来会把它带回目标区
    src.write_at(src_off + 3 * chunk, &[0xEE; 512]).unwrap();
    let resumed = rsize(&mut src,1, 4096, 12287, chunk, true, &mut |_| {});
    assert!(resumed.is_complete(), "the resume must finish the move: {}", resumed.exit_code());
    for i in 0..4u8 {
        let mut b = [0u8; 512];
        src.read_at(dst_off + i as u64 * chunk, &mut b).unwrap();
        assert_eq!(b, [0x10 + i; 512], "chunk {i} was copied from the wrong source block");
    }
    let g = table::load_gpt(&src).unwrap().unwrap();
    assert_eq!((g.entries[0].starting_lba, g.entries[0].ending_lba), (4096, 12287));
    assert!(!ckpt_path.exists(), "a completed resume must release the checkpoint");
    drop(src);
    let _ = std::fs::remove_file(&path);
}

/// 提交已落盘、ckpt 尚未更新时中断：判据必须落在"盘上几何已是目标"上，否则续传会把
/// "参数不匹配"判成永久拒绝。反向断言：源区指纹不得出现在目标区——若误走续传路径，
/// 数据会被再搬一次
#[test]
pub(crate) fn resize_part_resume_after_commit_without_ckpt_update() {
    let ss = 512u64;
    let (mut src, path) = plan_fixture(&[(1, [0x11; 16], 2048, 10239)]);
    let (old_start, old_end) = (2048u64, 10239u64);
    let (new_start, new_end) = (12288u64, 20479u64);
    let chunk: u64 = 1024 * 1024;
    // 整区载荷：每个扇区各不相同。若续跑误把源区再搬一次，目标区就会带着这些字节，
    // 源区也会被改写——只看目标首扇区为零看不出这两件事
    let sectors = old_end - old_start + 1;
    let mut want = Vec::with_capacity((sectors * ss) as usize);
    for lba in old_start..=old_end {
        let mut s = [0u8; 512];
        for (j, b) in s.iter_mut().enumerate() {
            *b = (lba as u8).wrapping_mul(37).wrapping_add(j as u8);
        }
        want.extend_from_slice(&s);
    }
    src.write_at(old_start * ss, &want).unwrap();

    // 模拟"commit 已完成"：把表项改到目标位置并落盘
    let mut g = table::load_gpt(&src).unwrap().unwrap();
    {
        let e = &mut g.entries[0];
        e.starting_lba = new_start;
        e.ending_lba = new_end;
    }
    let last_lba = src.size / ss - 1;
    table::commit_gpt(&mut src, &g, last_lba).unwrap();
    table::ensure_protective_mbr(&mut src, ss).unwrap();

    // ckpt 停留在搬移前形状：正是 commit 与 ckpt 更新之间的那个窗口
    let ckpt_path = src.identity.checkpoint_path().to_path_buf();
    let ckpt = RsCheckpoint {
        proof: crate::dev::LiveProof::Inode,
        disk_size: src.size, ss, part: 1,
        old_start, old_end, new_start, new_end,
        fs_shrunk: FsShrink::NotShrunk,
        chunks_done: (old_end - old_start + 1) * ss / chunk,
        chunk_bytes: chunk,
        fp: Default::default(),
        map: None,
        lvm: None,
        fstype: None,
    };
    atomic_write_ckpt(&ckpt_path, &ckpt.serialize()).unwrap();

    let o = rsize(&mut src,1, new_start, new_end, chunk, true, &mut |_| {});
    assert!(o.has_layout_write(), "must finish the FS step instead of refusing (exit {})", o.exit_code());
    assert!(!ckpt_path.exists(), "checkpoint must be removed on success");
    let g2 = table::load_gpt(&src).unwrap().unwrap();
    assert_eq!((g2.entries[0].starting_lba, g2.entries[0].ending_lba), (new_start, new_end));
    let mut b = [0u8; 512];
    src.read_at(new_start * ss, &mut b).unwrap();
    assert!(b.iter().all(|&x| x == 0), "committed resize must not move data again");
    // 逐字节核对整段：目标区仍为空（没有再搬一次），源区载荷也未被续跑改写
    let mut dst = vec![0u8; want.len()];
    src.read_at(new_start * ss, &mut dst).unwrap();
    assert!(dst.iter().all(|&x| x == 0), "the committed run must leave the target range empty");
    let mut orig = vec![0u8; want.len()];
    src.read_at(old_start * ss, &mut orig).unwrap();
    assert_eq!(orig, want, "the committed run must not rewrite the source range either");
    drop(src);
    let _ = std::fs::remove_file(&path);
}

/// 崩溃后改了 --end 重跑：ckpt 是那次作业的、盘上几何也停在它的终点，但**本次请求不是它**。
/// 判据若只取"盘上几何 == ckpt.new_"，这一次会被判成"已提交，只剩 FS 收尾"：表不动、
/// FS 不动，却报成功——用户拿着一次什么都没做的成功去用那块空间
#[test]
pub(crate) fn retargeted_resize_after_crash_is_refused() {
    let ss = 512u64;
    let chunk: u64 = 1024 * 1024;
    let (old_start, old_end) = (2048u64, 10239u64);
    let (new_start, new_end) = (12288u64, 20479u64);
    let (mut src, path) = plan_fixture(&[(1, [0x11; 16], old_start, old_end)]);

    // 现场：表已提交到 (new_start, new_end)，ckpt 记着这次作业（提交后、FS 收尾前中断）
    let mut g = table::load_gpt(&src).unwrap().unwrap();
    {
        let e = &mut g.entries[0];
        e.starting_lba = new_start;
        e.ending_lba = new_end;
    }
    let last_lba = src.size / ss - 1;
    table::commit_gpt(&mut src, &g, last_lba).unwrap();
    table::ensure_protective_mbr(&mut src, ss).unwrap();
    let ckpt_path = src.identity.checkpoint_path().to_path_buf();
    let ckpt = RsCheckpoint {
        proof: crate::dev::LiveProof::Inode,
        disk_size: src.size, ss, part: 1,
        old_start, old_end, new_start, new_end,
        fs_shrunk: FsShrink::NotShrunk, chunks_done: 4, chunk_bytes: chunk,
        fp: Default::default(),
        map: None,
        lvm: None,
        fstype: None,
    };

    // (a) 请求与 ckpt 记的目标不同 → 拒绝，且不写盘
    atomic_write_ckpt(&ckpt_path, &ckpt.serialize()).unwrap();
    let before = std::fs::read(&path).unwrap();
    let o = rsize(&mut src,1, new_start, new_end + 2048, chunk, true, &mut |_| {});
    assert_eq!(o.exit_code(), crate::outcome::EXIT_REFUSED, "a retargeted run must be refused");
    assert_eq!(std::fs::read(&path).unwrap(), before, "a refused run must not write");
    assert!(ckpt_path.exists(), "the checkpoint must stay for the job it belongs to");

    // (b) 请求与 ckpt 一致，但盘上几何既不是搬移前也不是搬移后（第三方工具动过表）→ 同样拒绝
    let mut g = table::load_gpt(&src).unwrap().unwrap();
    {
        let e = &mut g.entries[0];
        e.starting_lba = 16384;
        e.ending_lba = 24575;
    }
    table::commit_gpt(&mut src, &g, last_lba).unwrap();
    table::ensure_protective_mbr(&mut src, ss).unwrap();
    let before = std::fs::read(&path).unwrap();
    let o = rsize(&mut src,1, new_start, new_end, chunk, true, &mut |_| {});
    assert_eq!(o.exit_code(), crate::outcome::EXIT_REFUSED, "an unknown on-disk geometry must be refused");
    assert_eq!(std::fs::read(&path).unwrap(), before, "a refused run must not write");
    assert_eq!(
        table::load_gpt(&src).unwrap().unwrap().entries[0].starting_lba,
        16384,
        "the table must stay where the third party put it"
    );

    drop(src);
    let _ = crate::dev::remove_recovery_file(&ckpt_path);
    let _ = std::fs::remove_file(&path);
}

/// 恢复判定对不上（ckpt 记的是另一次作业）仍是事前拒绝：与"槽位被别的作业占着"同码，
/// 因为此刻确实一字未写。同理 `--no-fs` 与缩容不可共存的判定也必须在写盘之前
#[test]
pub(crate) fn divergent_and_no_fs_shrink_are_refused_before_any_write() {
    let chunk: u64 = 1024 * 1024;
    // (a) Divergent：ckpt 的目标区间与本次请求不同
    let (src, p) = plan_fixture(&[(1, [0x11; 16], 2048, 6143)]);
    drop(src);
    make_stale(&p);
    let mut src = plan_open(&p);
    let ckpt_path = src.identity.checkpoint_path().to_path_buf();
    let rs = RsCheckpoint {
        proof: crate::dev::LiveProof::Inode,
        disk_size: src.size, ss: 512, part: 1, old_start: 2048, old_end: 6143,
        new_start: 3072, new_end: 5119, fs_shrunk: FsShrink::NotShrunk, chunks_done: 0, chunk_bytes: chunk,
        fp: Default::default(),
        map: None,
        lvm: None,
        fstype: None,
    };
    atomic_write_ckpt(&ckpt_path, &rs.serialize()).unwrap();
    let o = rsize(&mut src, 1, 2048, 4095, chunk, true, &mut |_| {});
    assert_eq!(o.exit_code(), 10, "Divergent 属事前拒绝");
    assert!(
        matches!(table::load_gpt(&src).unwrap().unwrap().state, table::GptState::NeedsRepair { .. }),
        "Divergent 拒绝不得写盘"
    );
    let _ = std::fs::remove_file(&ckpt_path);
    drop(src);
    let _ = std::fs::remove_file(&p);

    // (b) --no-fs + 缩容：分区末端会切进未缩的 FS 元数据，必须事前拒绝
    let (src, p) = plan_fixture(&[(1, [0x11; 16], 2048, 6143)]);
    drop(src);
    make_stale(&p);
    let mut src = plan_open(&p);
    let o = rsize(&mut src, 1, 2048, 4095, chunk, true, &mut |_| {});
    assert_eq!(o.exit_code(), 10, "--no-fs 缩容属事前拒绝");
    assert!(
        matches!(table::load_gpt(&src).unwrap().unwrap().state, table::GptState::NeedsRepair { .. }),
        "--no-fs 缩容拒绝不得写盘"
    );
    drop(src);
    let _ = std::fs::remove_file(&p);
}

/// shrink 状态在盘上的三态编码：状态字节取 2 表示"命令已返回、落点未证实"。
/// 其余取值只能当损坏——读成"已验证"会让后续搬移切进未缩的元数据
#[test]
pub(crate) fn shrink_state_is_tri_state_on_the_wire() {
    let chunk: u64 = 1024 * 1024;
    let ckpt = |fs_shrunk| RsCheckpoint {
        proof: crate::dev::LiveProof::Inode,
        disk_size: 32 * 1024 * 1024, ss: 512, part: 1,
        old_start: 2048, old_end: 6143, new_start: 3072, new_end: 5119,
        fs_shrunk, chunks_done: 0, chunk_bytes: chunk,
        fp: Default::default(), map: None, fstype: None,
        lvm: None,
    };
    // 布局未变，只是语义变：状态字节固定在 65（magic(8) + ver(4) + LVM 标志(1) 之后起算）
    let b = ckpt(FsShrink::Unverified).serialize();
    assert_eq!(u32::from_le_bytes(b[8..12].try_into().unwrap()), CKPT2_VERSION);
    assert_eq!(b[65], 2, "unverified shrink must be distinguishable on disk");
    assert_eq!(ckpt(FsShrink::Verified).serialize()[65], 1);
    assert_eq!(ckpt(FsShrink::NotShrunk).serialize()[65], 0);
}

/// 续跑前的回读：读不出来就报"无从判断"，不得按"未缩"或"已缩"任一侧猜——
/// 猜"已缩"会接着搬数据切进未缩的元数据，猜"未缩"会把同一次缩容再做一遍
#[test]
pub(crate) fn shrink_state_readback_reports_unknown_instead_of_guessing() {
    let (src, p) = plan_fixture(&[(1, [0x11; 16], 2048, 6143)]);
    // 分区里没有可识别的 FS：任何回读都不可能成功
    assert_eq!(
        crate::fsops::shrink_already_at(&src, 1, Some(crate::fsid::FsKind::Ext), 1024 * 1024),
        None,
        "an unreadable filesystem must not be reported as shrunk or unshrunk"
    );
    assert_eq!(crate::fsops::shrink_already_at(&src, 1, None, 1024 * 1024), None);
    drop(src);
    let _ = std::fs::remove_file(&p);
}

/// 现场记着 LVM 收尾事实，而本次入口不跑链（`resize-part` / `move` 传 defer = false）：
/// 盘上表已提交到新几何，重跑判定 Committed，LVM 那一步仍没人做。此时不能删唯一的
/// 现场（删掉就没有"该扩到哪"的基线），未做的链以 Pending 浮出，退出码 20
#[test]
pub(crate) fn recorded_lvm_facts_keep_the_checkpoint_when_the_caller_runs_no_chain() {
    let chunk: u64 = 1024 * 1024;
    // 盘上几何已是那次 PV 作业的目标（4096..8191），与 ckpt 记的 new 一致
    let (mut src, p) = plan_fixture(&[(1, [0x11; 16], 4096, 8191)]);
    let ckpt_path = src.identity.checkpoint_path().to_path_buf();
    let ck = RsCheckpoint {
        proof: crate::dev::LiveProof::Inode,
        disk_size: src.size, ss: 512, part: 1,
        old_start: 2048, old_end: 6143, new_start: 4096, new_end: 8191,
        fs_shrunk: FsShrink::NotShrunk, chunks_done: 0, chunk_bytes: chunk,
        fp: Default::default(), map: None, fstype: None,
        lvm: Some(LvmPostFacts {
            lv_path: Some("/dev/vg0/data".into()),
            lv_size_before: 1 << 20,
            lv_target: (1 << 20) + 8 * 4096,
            extent_bytes: 4096,
        }),
    };
    atomic_write_ckpt(&ckpt_path, &ck.serialize()).unwrap();

    let o = rsize(&mut src, 1, 4096, 8191, chunk, true, &mut |_| {});
    assert_eq!(o.exit_code(), 20, "the unrun LVM chain is a pending step: {o:?}");
    match &o {
        Outcome::Applied { pending, .. } => {
            assert_eq!(pending.len(), 1, "exactly the LVM chain must be reported: {pending:?}");
            assert!(pending[0].detail.contains("/dev/vg0/data"), "{:?}", pending[0]);
            assert!(pending[0].hint.contains("re-run"), "{:?}", pending[0]);
        }
        other => panic!("expected Applied, got {other:?}"),
    }
    assert!(ckpt_path.exists(), "dropping the checkpoint would erase the only baseline for the chain");
    assert_eq!(read_lvm_post(&ckpt_path).unwrap().unwrap().lv_target, (1 << 20) + 8 * 4096);

    drop(src);
    let _ = std::fs::remove_file(&ckpt_path);
    let _ = std::fs::remove_file(&p);
}

/// checkpoint 的撤除时机：`defer` 下 execute 成功也不撤，现场留给调用方的收尾
/// （PV 的 pvresize/lvextend 在表提交之后才跑）；同一作业在非 defer 下成功即清
#[test]
pub(crate) fn deferred_resize_leaves_the_checkpoint_to_the_caller() {
    let chunk: u64 = 1024 * 1024;
    let (old_start, old_end) = (2048u64, 10239u64);
    let (new_start, new_end) = (4096u64, 12287u64);

    // defer：搬移作业（起点变化）必然在开工时落 ckpt
    let (mut src, p) = plan_fixture(&[(1, [0x11; 16], old_start, old_end)]);
    // 整区载荷：每个扇区各不相同，复制错位或只搬首扇区都会与期望不等
    let sectors = old_end - old_start + 1;
    let mut want = Vec::with_capacity((sectors * 512) as usize);
    for lba in old_start..=old_end {
        let mut s = [0u8; 512];
        for (j, b) in s.iter_mut().enumerate() {
            *b = (lba as u8).wrapping_mul(37).wrapping_add(j as u8);
        }
        want.extend_from_slice(&s);
    }
    src.write_at(old_start * 512, &want).unwrap();
    let ckpt_path = src.identity.checkpoint_path().to_path_buf();
    let o = rsize_deferred(&mut src, 1, new_start, new_end, chunk, true, true, &mut |_| {});
    assert!(o.has_layout_write(), "resize must apply (exit {})", o.exit_code());
    let mut got = vec![0u8; want.len()];
    src.read_at(new_start * 512, &mut got).unwrap();
    assert_eq!(got, want, "the deferred resize must have copied every byte into the new range");
    assert!(ckpt_path.exists(), "defer must leave the checkpoint for the caller's finish step");
    // 调用方收尾成功之后自行撤除
    assert!(crate::dev::remove_recovery_file(&ckpt_path));
    drop(src);
    let _ = std::fs::remove_file(&p);

    // 非 defer：同一形状的作业成功即撤
    let (mut src, p) = plan_fixture(&[(1, [0x11; 16], old_start, old_end)]);
    src.write_at(old_start * 512, &[0xA5u8; 512]).unwrap();
    let ckpt_path = src.identity.checkpoint_path().to_path_buf();
    let o = rsize_deferred(&mut src, 1, new_start, new_end, chunk, true, false, &mut |_| {});
    assert!(o.has_layout_write(), "resize must apply (exit {})", o.exit_code());
    assert!(!ckpt_path.exists(), "a completed job must not leave a checkpoint behind");
    drop(src);
    let _ = std::fs::remove_file(&p);
}

/// "最终状态已核验"不成立时：表已提交、FS 步留下待办（32K 页格式的 swap 在本机认不出
/// 且未重建）。现场必须保留，并且本次输出要说明它留在原处——静默保留会让重跑者
/// 不知道自己撞见的是同一作业的续传点。字节仍须可判读
#[test]
pub(crate) fn pending_fs_step_reports_the_retained_checkpoint() {
    let chunk: u64 = 1024 * 1024;
    let (src, p) = plan_fixture(&[(1, [0x11; 16], 2048, 6143)]);
    drop(src);
    // 只在 32K 候选位置放 swap 签名：identify 给 unknown，收尾按"分区已扩、swap 未重建"记待办
    let base = 2048 * 512;
    let mut raw = std::fs::read(&p).unwrap();
    raw[base + 32768 - 10..base + 32768].copy_from_slice(b"SWAPSPACE2");
    std::fs::write(&p, &raw).unwrap();

    let mut src = plan_open(&p);
    let ckpt_path = src.identity.checkpoint_path().to_path_buf();
    let mut logs = Vec::new();
    let o = rsize(&mut src, 1, 2048, 8191, chunk, false, &mut |l: &str| logs.push(l.to_string()));
    assert_eq!(o.exit_code(), 20, "{o:?}");
    assert!(ckpt_path.exists(), "a verified-incomplete job must keep its checkpoint");
    let (slot, _) = read_checkpoint(&src, &geom_of(&src)).unwrap();
    assert!(matches!(slot, CheckpointSlot::Resize(_)), "the retained bytes must still parse as a resize job");
    assert!(
        logs.iter().any(|l| l.contains("retained") && l.contains("not verified")),
        "the run must say the checkpoint stays and why: {logs:?}"
    );

    drop(src);
    let _ = std::fs::remove_file(&ckpt_path);
    let _ = std::fs::remove_file(&p);
}

/// "后续恢复不再依赖"不成立时：调用方推迟收尾（`defer`），现场要留给 LVM 链。
/// 本次执行成功且无待办，但删除判据必须看调用方的后续步骤，并说明现场留在了原处
#[test]
pub(crate) fn deferred_finish_reports_the_retained_checkpoint() {
    let chunk: u64 = 1024 * 1024;
    let (mut src, p) = plan_fixture(&[(1, [0x11; 16], 2048, 10239)]);
    src.write_at(2048 * 512, &[0xA5u8; 512]).unwrap();
    let ckpt_path = src.identity.checkpoint_path().to_path_buf();
    let mut logs = Vec::new();
    let o = rsize_deferred(&mut src, 1, 4096, 12287, chunk, true, true, &mut |l: &str| logs.push(l.to_string()));
    assert!(o.has_layout_write(), "resize must apply (exit {})", o.exit_code());
    assert!(ckpt_path.exists(), "the caller's finish step still needs the checkpoint");
    let (slot, _) = read_checkpoint(&src, &geom_of(&src)).unwrap();
    assert!(matches!(slot, CheckpointSlot::Resize(_)), "the retained bytes must still parse as a resize job");
    assert!(
        logs.iter().any(|l| l.contains("retained") && l.contains("later")),
        "the run must say the checkpoint stays and why: {logs:?}"
    );

    drop(src);
    let _ = std::fs::remove_file(&ckpt_path);
    let _ = std::fs::remove_file(&p);
}

/// 三条件全成立：表已提交、无待办、调用方不推迟收尾 → 现场删除，且输出里没有保留声明
#[test]
pub(crate) fn completed_resize_removes_the_checkpoint_without_a_retention_notice() {
    let chunk: u64 = 1024 * 1024;
    let (mut src, p) = plan_fixture(&[(1, [0x11; 16], 2048, 10239)]);
    src.write_at(2048 * 512, &[0xA5u8; 512]).unwrap();
    let ckpt_path = src.identity.checkpoint_path().to_path_buf();
    let mut logs = Vec::new();
    let o = rsize(&mut src, 1, 4096, 12287, chunk, true, &mut |l: &str| logs.push(l.to_string()));
    assert!(o.has_layout_write(), "resize must apply (exit {})", o.exit_code());
    assert!(!ckpt_path.exists(), "all three conditions hold: the checkpoint must be removed");
    assert!(
        !logs.iter().any(|l| l.contains("retained")),
        "a removed checkpoint must not be reported as retained: {logs:?}"
    );

    drop(src);
    let _ = std::fs::remove_file(&p);
}
