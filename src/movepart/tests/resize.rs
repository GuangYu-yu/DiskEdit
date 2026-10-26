#![allow(clippy::let_underscore_must_use)] // 测试的清理步骤有意忽略失败
use super::*;
use super::support::*;
use gptman::{GPT, GPTPartitionEntry};
use std::io::Cursor;

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
        proof: crate::dev::IdentityProof::Inode,
        disk_size: size, ss, part: 1,
        old_start: 2048, old_end: 10239, new_start: 12288, new_end: 20479,
        fs_shrunk: FsShrink::NotShrunk, chunks_done: 1, chunk_bytes: chunk,
        fp: Default::default(),
        map: None,
        fstype: None,
    };
    atomic_write_ckpt(&ckpt_path, &ckpt.serialize()).unwrap();

    // 续传：跳过 chunk 0，补齐 chunk 1..3，提交表项
    let o = rsize(&mut src,1, 12288, 20479, chunk, false, &mut |_| {});
    assert!(o.is_applied(), "resize must apply (exit {})", o.exit_code());

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
    src.write_at(old_start * ss, &[0xA5u8; 512]).unwrap();

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
        proof: crate::dev::IdentityProof::Inode,
        disk_size: src.size, ss, part: 1,
        old_start, old_end, new_start, new_end,
        fs_shrunk: FsShrink::NotShrunk,
        chunks_done: (old_end - old_start + 1) * ss / chunk,
        chunk_bytes: chunk,
        fp: Default::default(),
        map: None,
        fstype: None,
    };
    atomic_write_ckpt(&ckpt_path, &ckpt.serialize()).unwrap();

    let o = rsize(&mut src,1, new_start, new_end, chunk, true, &mut |_| {});
    assert!(o.is_applied(), "must finish the FS step instead of refusing (exit {})", o.exit_code());
    assert!(!ckpt_path.exists(), "checkpoint must be removed on success");
    let g2 = table::load_gpt(&src).unwrap().unwrap();
    assert_eq!((g2.entries[0].starting_lba, g2.entries[0].ending_lba), (new_start, new_end));
    let mut b = [0u8; 512];
    src.read_at(new_start * ss, &mut b).unwrap();
    assert!(b.iter().all(|&x| x == 0), "committed resize must not move data again");
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
        proof: crate::dev::IdentityProof::Inode,
        disk_size: src.size, ss, part: 1,
        old_start, old_end, new_start, new_end,
        fs_shrunk: FsShrink::NotShrunk, chunks_done: 4, chunk_bytes: chunk,
        fp: Default::default(),
        map: None,
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
        proof: crate::dev::IdentityProof::Inode,
        disk_size: src.size, ss: 512, part: 1, old_start: 2048, old_end: 6143,
        new_start: 3072, new_end: 5119, fs_shrunk: FsShrink::NotShrunk, chunks_done: 0, chunk_bytes: chunk,
        fp: Default::default(),
        map: None,
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

/// shrink 状态在盘上的三态编码：版本升到 7、状态字节取 2 表示"命令已返回、落点未证实"。
/// v6 及更早该字节是 bool，读到 2 只能当损坏——否则旧现场会被读成"已验证"
#[test]
pub(crate) fn shrink_state_is_tri_state_on_the_wire() {
    let chunk: u64 = 1024 * 1024;
    let ckpt = |fs_shrunk| RsCheckpoint {
        proof: crate::dev::IdentityProof::Inode,
        disk_size: 32 * 1024 * 1024, ss: 512, part: 1,
        old_start: 2048, old_end: 6143, new_start: 3072, new_end: 5119,
        fs_shrunk, chunks_done: 0, chunk_bytes: chunk,
        fp: Default::default(), map: None, fstype: None,
    };
    // 布局未变，只是语义变：状态字节固定在 64
    let b = ckpt(FsShrink::Unverified).serialize();
    assert_eq!(u32::from_le_bytes(b[8..12].try_into().unwrap()), CKPT2_VERSION);
    assert_eq!(b[64], 2, "unverified shrink must be distinguishable on disk");
    assert_eq!(ckpt(FsShrink::Verified).serialize()[64], 1);
    assert_eq!(ckpt(FsShrink::NotShrunk).serialize()[64], 0);
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