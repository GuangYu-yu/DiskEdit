#![allow(clippy::let_underscore_must_use)] // 测试的清理步骤有意忽略失败
use super::*;
use super::support::*;

/// 两族作业共用一个 checkpoint 槽位。"对方在现场"必须与"空槽"分开对待：当成空槽会覆盖掉
/// 那份唯一的续传信息，而被中断的作业盘上几何可能正停在半途
#[test]
pub(crate) fn checkpoint_slot_ownership_is_enforced() {
    let chunk: u64 = 1024 * 1024;
    let (mut src, path) = plan_fixture(&[(1, [0x11; 16], 2048, 6143), (2, [0x22; 16], 8192, 10239)]);
    let g = table::load_gpt(&src).unwrap().unwrap();
    let ss = g.ss;
    let ckpt_path = src.identity.checkpoint_path().to_path_buf();
    let plan = make_plan(&mut src, 1).unwrap();
    assert!(!plan.moves.is_empty(), "fixture must have a blocker to relocate");

    // (a) 槽位里是单分区 resize 的 ckpt → apply 拒绝，且不得动盘、不得覆盖它
    let rs = RsCheckpoint {
        proof: crate::dev::LiveProof::Inode,
        disk_size: src.size, ss, part: 1,
        old_start: 2048, old_end: 6143, new_start: 2048, new_end: 3071,
        fs_shrunk: FsShrink::NotShrunk, chunks_done: 0, chunk_bytes: chunk,
        fp: Default::default(),
        map: None,
        fstype: None,
    };
    atomic_write_ckpt(&ckpt_path, &rs.serialize()).unwrap();
    let o = ap(&mut src, &plan, chunk, true, &mut |_| {});
    assert_eq!(o.exit_code(), crate::outcome::EXIT_REFUSED, "apply must refuse while a resize job owns the slot");
    assert!(matches!(read_checkpoint(&src, &geom_of(&src)).unwrap().0, CheckpointSlot::Resize(_)), "the resize checkpoint must survive");
    assert_eq!(table::load_gpt(&src).unwrap().unwrap().entries[1].starting_lba, 8192, "nothing may be relocated");

    // (b) 槽位里是 plan 型搬移的 ckpt → resize_part 拒绝（不能按"无 ckpt"重做）
    std::fs::remove_file(&ckpt_path).unwrap();
    let ck = Checkpoint {
        proof: crate::dev::LiveProof::Inode,
        disk_size: src.size, ss, grow_part: 1, last_usable_lba: plan.last_usable_lba,
        moves: plan.moves.clone(), kind: PlanKind::TailPacked, cur_index: 0, chunks_done: 0, chunk_bytes: chunk,
        fp: Default::default(),
        map: None,
    };
    atomic_write_ckpt(&ckpt_path, &ck.serialize()).unwrap();
    let o = rsize(&mut src,1, 2048, 3071, chunk, false, &mut |_| {});
    assert!(
        matches!(&o, Outcome::Refused(m) if m.contains("relocation job")),
        "resize_part must refuse while a relocation job owns the slot: {o:?}"
    );
    assert!(matches!(read_checkpoint(&src, &geom_of(&src)).unwrap().0, CheckpointSlot::Relocation(_)), "the relocation checkpoint must survive");

    drop(src);
    let _ = std::fs::remove_file(&ckpt_path);
    let _ = std::fs::remove_file(&path);
}

/// 落点不存在即空槽（从未跑过搬移是常态）；文件在而解不出来则是故障——
/// 若把它当空槽，中断的搬移会被降级成一次全新规划
#[test]
pub(crate) fn corrupt_checkpoint_is_an_error_not_an_empty_slot() {
    let (src, path) = plan_fixture(&[(1, [0x11; 16], 2048, 6143)]);
    let ckpt_path = src.identity.checkpoint_path().to_path_buf();

    assert!(matches!(read_checkpoint(&src, &geom_of(&src)).unwrap().0, CheckpointSlot::Empty));

    // 头部魔数在、内容被截断：正是断电撕裂写下的样子。
    // 新契约：文件在而全部不可读 = Infra，且文案指向 abandon（不再吞成空槽）
    std::fs::write(&ckpt_path, CKPT_MAGIC).unwrap();
    let e = read_checkpoint(&src, &geom_of(&src)).err().expect("a corrupt checkpoint must not read as an empty slot");
    assert!(matches!(&e, Fail::Infra(m) if m.contains("is unreadable") && m.contains("diskedit abandon")), "{e:?}");

    drop(src);
    let _ = std::fs::remove_file(&ckpt_path);
    let _ = std::fs::remove_file(&path);
}

/// 盘上可控的分区号必须在构造点拦掉：`Plan.moves` 可整份来自 ckpt，而下游拿它直接索引
/// `entries[(n-1)]`，越界会 panic。四种畸形都重算 CRC——证明拦住它们的是字段校验，不是 CRC
#[test]
pub(crate) fn checkpoint_rejects_implausible_partition_numbers() {
    let ckpt = Checkpoint {
        proof: crate::dev::LiveProof::Inode,
        disk_size: 64 * 1024 * 1024,
        ss: 512,
        grow_part: 3,
        last_usable_lba: 100_000,
        moves: vec![PlanEntry { part_num: 1, first_lba: 2048, len_lba: 100, delta_lba: 200, is_swap: false }],
        kind: PlanKind::TailPacked,
        cur_index: 0,
        chunks_done: 0,
        chunk_bytes: 1024 * 1024,
        fp: Default::default(),
        map: None,
    };
    assert!(Checkpoint::deserialize(&ckpt.serialize(), &lim(128)).is_ok(), "the baseline must be readable");

    // 字段偏移：magic(8) + ver(4) + disk_size(8) + ss(8) ⇒ grow_part @28；
    // 其后 last_usable_lba(8) + count(4) ⇒ 首条 move 的 part_num @44
    const GROW_PART: usize = 28;
    const MOVE_PART: usize = 44;
    for (off, value, what) in [
        (GROW_PART, 0u32, "grow_part = 0"),
        (GROW_PART, 129, "grow_part = 129"),
        (MOVE_PART, 0, "part_num = 0"),
        (MOVE_PART, 129, "part_num = 129"),
    ] {
        let mut b = ckpt.serialize();
        b[off..off + 4].copy_from_slice(&value.to_le_bytes());
        let crc_off = b.len() - 4;
        let crc = table::crc32(&b[..crc_off]);
        b[crc_off..].copy_from_slice(&crc.to_le_bytes());
        let e = match Checkpoint::deserialize(&b, &lim(128)) {
            Ok(_) => panic!("{what} must be rejected"),
            Err(e) => e,
        };
        assert_eq!(e.kind(), io::ErrorKind::InvalidData, "{what}: {e}");
    }

    // 分区号上界来自**表的几何**，不是常量 128：同一份合法记录（grow_part = 200、
    // 首条 move 的 part_num = 3）在 256 槽位的几何下必须可读，在 128 槽位下被拒。
    // 少了这条，一块 256 槽位的表产生的 checkpoint 会被判成"损坏"，中断的搬移永久无法续传
    let wide = Checkpoint {
        proof: crate::dev::LiveProof::Inode,
        grow_part: 200,
        last_usable_lba: 100_000,
        fp: Default::default(),
        map: None,
        ..ckpt
    };
    assert!(
        Checkpoint::deserialize(&wide.serialize(), &lim(256)).is_ok(),
        "a record written against a 256-slot table must stay readable"
    );
    assert!(
        Checkpoint::deserialize(&wide.serialize(), &lim(128)).is_err(),
        "the same record must be rejected when the table only has 128 slots"
    );
}

/// 原子写的临时文件必须落在目标同目录：相对路径的 parent() 是空而非 None，
/// 空父目录若拼上根组件，临时文件会写到当前盘的根目录
#[test]
pub(crate) fn relative_checkpoint_path_writes_beside_the_target() {
    let name = format!("rel_ckpt_test.{}.tmp", std::process::id());
    let p = std::path::PathBuf::from(&name);
    atomic_write_ckpt(&p, b"payload").unwrap();
    assert!(p.exists(), "the ckpt must land beside its relative target");
    let _ = std::fs::remove_file(&p);
}

/// 进度字段同属盘上可控值：CRC 自洽但值荒谬的 ckpt 若被放行，续传会把该条目的
/// 全部 chunk 跳过后仍提交表项——表将指向从未复制过的位置。重算 CRC 以证明
/// 拦住它的是上界校验；两族 ckpt 各验一遍
#[test]
pub(crate) fn checkpoint_rejects_implausible_chunk_progress() {
    // 搬移型：len_lba=100 × ss=512 = 50 KiB < 1 MiB ⇒ 该条目只有 1 个 chunk
    let ckpt = Checkpoint {
        proof: crate::dev::LiveProof::Inode,
        disk_size: 64 * 1024 * 1024,
        ss: 512,
        grow_part: 2,
        last_usable_lba: 100_000,
        moves: vec![PlanEntry { part_num: 1, first_lba: 2048, len_lba: 100, delta_lba: 200, is_swap: false }],
        kind: PlanKind::TailPacked,
        cur_index: 0,
        chunks_done: 1,
        chunk_bytes: 1024 * 1024,
        fp: Default::default(),
        map: None,
    };
    assert!(Checkpoint::deserialize(&ckpt.serialize(), &lim(128)).is_ok(), "baseline must stay readable");

    const CUR_INDEX: usize = 73;  // 头 73 字节（magic..moves[0]）之后
    const CHUNKS_DONE: usize = 77;
    let rewrite_crc = |b: &mut Vec<u8>| {
        let crc_off = b.len() - 4;
        let crc = table::crc32(&b[..crc_off]);
        b[crc_off..].copy_from_slice(&crc.to_le_bytes());
    };
    let mut b = ckpt.serialize();
    b[CHUNKS_DONE..CHUNKS_DONE + 8].copy_from_slice(&2u64.to_le_bytes());
    rewrite_crc(&mut b);
    let e = Checkpoint::deserialize(&b, &lim(128)).err().expect("chunks_done beyond the entry's chunk count must be rejected");
    assert_eq!(e.kind(), io::ErrorKind::InvalidData, "{e}");

    // 全部条目已提交（cur_index == moves.len()）时进度必须归零
    let mut b = ckpt.serialize();
    b[CUR_INDEX..CUR_INDEX + 4].copy_from_slice(&1u32.to_le_bytes());
    rewrite_crc(&mut b);
    assert!(Checkpoint::deserialize(&b, &lim(128)).is_err(), "residual progress after the last entry must be rejected");

    // cur_index 越过条目数（chunks_done 归零可绕过进度校验）：放行会在恢复比对里
    // 被夹回 len，续传循环空转、搬移全部跳过而表仍按新几何提交
    let mut b = ckpt.serialize();
    b[CUR_INDEX..CUR_INDEX + 4].copy_from_slice(&2u32.to_le_bytes());
    rewrite_crc(&mut b);
    let e = Checkpoint::deserialize(&b, &lim(128)).err().expect("cur_index beyond the move list must be rejected");
    assert_eq!(e.kind(), io::ErrorKind::InvalidData, "{e}");

    // 单分区 resize 型：区间 1024 LBA × 512 B = 512 KiB < 1 MiB ⇒ 同样只有 1 个 chunk
    let rs = RsCheckpoint {
        proof: crate::dev::LiveProof::Inode,
        disk_size: 64 * 1024 * 1024, ss: 512, part: 1,
        old_start: 2048, old_end: 3071, new_start: 2048, new_end: 3071,
        fs_shrunk: FsShrink::NotShrunk, chunks_done: 1, chunk_bytes: 1024 * 1024,
        fp: Default::default(),
        map: None,
        fstype: None,
    };
    assert!(RsCheckpoint::deserialize(&rs.serialize(), &lim(128)).is_ok(), "baseline must stay readable");
    // 带 loop 映射与真实指纹的现场同样要原样回读：标志字节与它后面那两个 u64 是一组，
    // 偏移记账错位会在这里立刻现形
    let mapped = RsCheckpoint {
        proof: crate::dev::LiveProof::Inode,
        disk_size: 64 * 1024 * 1024, ss: 512, part: 1,
        old_start: 2048, old_end: 3071, new_start: 2048, new_end: 3071,
        fs_shrunk: FsShrink::NotShrunk, chunks_done: 1, chunk_bytes: 1024 * 1024,
        fp: crate::dev::TargetFingerprint { dev: 7, ino: 42, size: 1 << 20, id: 0 },
        map: Some((1024 * 1024, 0)),
        fstype: Some(FsKind::Ext),
    };
    let back = RsCheckpoint::deserialize(&mapped.serialize(), &lim(128)).expect("mapped checkpoint must round-trip");
    assert_eq!(back.fp, crate::dev::TargetFingerprint { dev: 7, ino: 42, size: 1 << 20, id: 0 });
    assert_eq!(back.map, Some((1024 * 1024, 0)));
    assert_eq!(back.fstype, Some(FsKind::Ext), "the filesystem kind must survive the round-trip");
    let mut b = rs.serialize();
    b[65..73].copy_from_slice(&2u64.to_le_bytes()); // chunks_done @ 12+6×8+4+1
    rewrite_crc(&mut b);
    let e = RsCheckpoint::deserialize(&b, &lim(128)).err().expect("chunks_done beyond the moved range must be rejected");
    assert_eq!(e.kind(), io::ErrorKind::InvalidData, "{e}");
}

/// last_usable_lba 的上界是可用区末端，不是容器末端：moves 为空时它直接成为
/// grow 终点写进表项 ending_lba，越过可用区会覆盖 GPT 备份头
#[test]
pub(crate) fn relocation_ckpt_last_usable_is_bounded_by_the_usable_area() {
    let ckpt = Checkpoint {
        proof: crate::dev::LiveProof::Inode,
        disk_size: 64 * 1024 * 1024,
        ss: 512,
        grow_part: 1,
        last_usable_lba: 100_000,
        moves: Vec::new(),
        kind: PlanKind::TailPacked,
        cur_index: 0,
        chunks_done: 0,
        chunk_bytes: 1024 * 1024,
        fp: Default::default(),
        map: None,
    };
    // 容器比可用区大（200_000 vs 100_000）：两值相等时测不出上界取的是哪一个
    let bounds = GeometryLimits {
        sector_size: 512,
        entry_count: 128,
        first_usable_lba: 34,
        last_usable_lba: 100_000,
        file_last_lba: 200_000,
    };
    assert!(Checkpoint::deserialize(&ckpt.serialize(), &bounds).is_ok(), "the baseline must be readable");

    // 字段偏移：magic(8) + ver(4) + disk_size(8) + ss(8) + grow_part(4) ⇒ last_usable @32
    const LAST_USABLE: usize = 32;
    let mut b = ckpt.serialize();
    b[LAST_USABLE..LAST_USABLE + 8].copy_from_slice(&150_000u64.to_le_bytes());
    let crc_off = b.len() - 4;
    let crc = table::crc32(&b[..crc_off]);
    b[crc_off..].copy_from_slice(&crc.to_le_bytes());
    let e = Checkpoint::deserialize(&b, &bounds)
        .err()
        .expect("last_usable past the usable area but inside the file must be rejected");
    assert_eq!(e.kind(), io::ErrorKind::InvalidData, "{e}");
}

/// ss 是 LBA→字节换算的唯一单位，写地址全经它换算：与目标几何不一致的 ckpt 即使
/// CRC 正确也必须拦下（重算 CRC——证明拦住它的是字段校验，不是 CRC），且两族
/// decoder 同一防线
#[test]
pub(crate) fn checkpoint_rejects_sector_size_mismatch() {
    let base = Checkpoint {
        proof: crate::dev::LiveProof::Inode,
        disk_size: 64 * 1024 * 1024,
        ss: 512,
        grow_part: 2,
        last_usable_lba: 100_000,
        moves: vec![PlanEntry { part_num: 1, first_lba: 2048, len_lba: 100, delta_lba: 200, is_swap: false }],
        kind: PlanKind::TailPacked,
        cur_index: 0,
        chunks_done: 0,
        chunk_bytes: 1024 * 1024,
        fp: Default::default(),
        map: None,
    };
    assert!(Checkpoint::deserialize(&base.serialize(), &lim(128)).is_ok(), "the baseline must be readable");

    // 字段偏移：magic(8) + ver(4) + disk_size(8) ⇒ ss @20（两族布局同前缀）
    const SS: usize = 20;
    let mut b = base.serialize();
    b[SS..SS + 8].copy_from_slice(&4096u64.to_le_bytes());
    let crc_off = b.len() - 4;
    let crc = table::crc32(&b[..crc_off]);
    b[crc_off..].copy_from_slice(&crc.to_le_bytes());
    let e = Checkpoint::deserialize(&b, &lim(128)).err().expect("a valid CRC must not smuggle a foreign sector size");
    assert_eq!(e.kind(), io::ErrorKind::InvalidData, "{e}");

    let rs = RsCheckpoint {
        proof: crate::dev::LiveProof::Inode,
        disk_size: 64 * 1024 * 1024,
        ss: 512,
        part: 1,
        old_start: 2048, old_end: 2147, new_start: 2048, new_end: 2147,
        fs_shrunk: FsShrink::NotShrunk, chunks_done: 0, chunk_bytes: 1024 * 1024,
        fp: Default::default(),
        map: None,
        fstype: None,
    };
    assert!(RsCheckpoint::deserialize(&rs.serialize(), &lim(128)).is_ok(), "the baseline must be readable");
    let mut b = rs.serialize();
    b[SS..SS + 8].copy_from_slice(&4096u64.to_le_bytes());
    let crc_off = b.len() - 4;
    let crc = table::crc32(&b[..crc_off]);
    b[crc_off..].copy_from_slice(&crc.to_le_bytes());
    let e = RsCheckpoint::deserialize(&b, &lim(128)).err().expect("a valid CRC must not smuggle a foreign sector size");
    assert_eq!(e.kind(), io::ErrorKind::InvalidData, "{e}");
}

/// Checkpoint 头部与载荷判据的拒绝分支逐字段覆盖。CRC 之前的判据直接改字节即可
/// 触发；CRC 之后的判据（chunk_bytes）须重算 CRC，证明拦住它的是字段校验本身。
/// 布局推导沿用上方测试：moves 从 @44 起每条 29 字节，
/// count=1 时 cur_index@73、chunks_done@77、chunk_bytes@85、kind@93
#[test]
pub(crate) fn checkpoint_rejects_malformed_header_and_payload() {
    let ckpt = Checkpoint {
        proof: crate::dev::LiveProof::Inode,
        disk_size: 64 * 1024 * 1024,
        ss: 512,
        grow_part: 2,
        last_usable_lba: 100_000,
        moves: vec![PlanEntry { part_num: 1, first_lba: 2048, len_lba: 100, delta_lba: 200, is_swap: false }],
        kind: PlanKind::TailPacked,
        cur_index: 0,
        chunks_done: 0,
        chunk_bytes: 1024 * 1024,
        fp: Default::default(),
        map: None,
    };
    assert!(Checkpoint::deserialize(&ckpt.serialize(), &lim(128)).is_ok(), "the baseline must be readable");
    let rewrite_crc = |b: &mut Vec<u8>| {
        let crc_off = b.len() - 4;
        let crc = table::crc32(&b[..crc_off]);
        b[crc_off..].copy_from_slice(&crc.to_le_bytes());
    };

    // 魔数与版本在 CRC 之前判定：无需重算
    let mut b = ckpt.serialize();
    b[0..8].copy_from_slice(b"NOTMAGI!");
    let e = Checkpoint::deserialize(&b, &lim(128)).err().expect("foreign magic must be rejected");
    assert_eq!(e.kind(), io::ErrorKind::InvalidData, "{e}");

    let mut b = ckpt.serialize();
    b[8..12].copy_from_slice(&99u32.to_le_bytes());
    let e = Checkpoint::deserialize(&b, &lim(128)).err().expect("a foreign version must be rejected");
    assert_eq!(e.kind(), io::ErrorKind::InvalidData, "{e}");

    // last_usable_lba 越容器（@32）：放行会拿一个容器上并不存在的可用区去判区间
    const LAST_USABLE: usize = 32;
    let mut b = ckpt.serialize();
    b[LAST_USABLE..LAST_USABLE + 8].copy_from_slice(&200_001u64.to_le_bytes());
    let e = Checkpoint::deserialize(&b, &lim(128)).err().expect("last_usable beyond the container must be rejected");
    assert_eq!(e.kind(), io::ErrorKind::InvalidData, "{e}");

    // 条目数越上界（@40）：防巨型 with_capacity，上界 = 表条目数
    const COUNT: usize = 40;
    let mut b = ckpt.serialize();
    b[COUNT..COUNT + 4].copy_from_slice(&129u32.to_le_bytes());
    let e = Checkpoint::deserialize(&b, &lim(128)).err().expect("a move count beyond the table's entries must be rejected");
    assert_eq!(e.kind(), io::ErrorKind::InvalidData, "{e}");

    // 首条 move 的字段：first_lba@48、len_lba@56、delta_lba@64（区间算术全 checked）
    const FIRST_LBA: usize = 48;
    const LEN_LBA: usize = 56;
    const DELTA_LBA: usize = 64;
    // 源区间溢出
    let mut b = ckpt.serialize();
    b[FIRST_LBA..FIRST_LBA + 8].copy_from_slice(&u64::MAX.to_le_bytes());
    b[LEN_LBA..LEN_LBA + 8].copy_from_slice(&1u64.to_le_bytes());
    let e = Checkpoint::deserialize(&b, &lim(128)).err().expect("an overflowing entry range must be rejected");
    assert_eq!(e.kind(), io::ErrorKind::InvalidData, "{e}");
    // 目标区间溢出
    let mut b = ckpt.serialize();
    b[DELTA_LBA..DELTA_LBA + 8].copy_from_slice(&u64::MAX.to_le_bytes());
    let e = Checkpoint::deserialize(&b, &lim(128)).err().expect("an overflowing target range must be rejected");
    assert_eq!(e.kind(), io::ErrorKind::InvalidData, "{e}");
    // 出可用区（first_usable = 34）
    let mut b = ckpt.serialize();
    b[FIRST_LBA..FIRST_LBA + 8].copy_from_slice(&33u64.to_le_bytes());
    b[LEN_LBA..LEN_LBA + 8].copy_from_slice(&1u64.to_le_bytes());
    let e = Checkpoint::deserialize(&b, &lim(128)).err().expect("an entry outside the usable area must be rejected");
    assert_eq!(e.kind(), io::ErrorKind::InvalidData, "{e}");

    // plan kind 只认 0/1（@93）
    const KIND: usize = 93;
    let mut b = ckpt.serialize();
    b[KIND] = 7;
    let e = Checkpoint::deserialize(&b, &lim(128)).err().expect("an unknown plan kind must be rejected");
    assert_eq!(e.kind(), io::ErrorKind::InvalidData, "{e}");

    // CRC 在字段判据之前：撕一位必须先撞 CRC
    let mut b = ckpt.serialize();
    let last = b.len() - 1;
    b[last] ^= 0xFF;
    let e = Checkpoint::deserialize(&b, &lim(128)).err().expect("a torn tail must be rejected by CRC");
    assert_eq!(e.kind(), io::ErrorKind::InvalidData, "{e}");

    // chunk_bytes = 0 在 CRC 之后判定：重算 CRC 证明拦它的是判据本身
    const CHUNK_BYTES: usize = 85;
    let mut b = ckpt.serialize();
    b[CHUNK_BYTES..CHUNK_BYTES + 8].copy_from_slice(&0u64.to_le_bytes());
    rewrite_crc(&mut b);
    let e = Checkpoint::deserialize(&b, &lim(128)).err().expect("chunk_bytes = 0 must be rejected");
    assert_eq!(e.kind(), io::ErrorKind::InvalidData, "{e}");
}

/// 单分区 resize 型的对应拒绝分支（与搬移型逐条对称）。
/// 布局：magic(8)+ver(4)+disk_size(8)+ss(8)+part(4) ⇒ old_start@32、old_end@40、
/// new_start@48、new_end@56、fs_shrunk@64、chunks_done@65、chunk_bytes@73
#[test]
pub(crate) fn resize_checkpoint_rejects_malformed_records() {
    let rs = RsCheckpoint {
        proof: crate::dev::LiveProof::Inode,
        disk_size: 64 * 1024 * 1024, ss: 512, part: 1,
        old_start: 2048, old_end: 3071, new_start: 2048, new_end: 3071,
        fs_shrunk: FsShrink::NotShrunk, chunks_done: 0, chunk_bytes: 1024 * 1024,
        fp: Default::default(),
        map: None,
        fstype: None,
    };
    assert!(RsCheckpoint::deserialize(&rs.serialize(), &lim(128)).is_ok(), "the baseline must be readable");
    let rewrite_crc = |b: &mut Vec<u8>| {
        let crc_off = b.len() - 4;
        let crc = table::crc32(&b[..crc_off]);
        b[crc_off..].copy_from_slice(&crc.to_le_bytes());
    };

    let mut b = rs.serialize();
    b[0..8].copy_from_slice(b"NOTMAGI!");
    let e = RsCheckpoint::deserialize(&b, &lim(128)).err().expect("foreign magic must be rejected");
    assert_eq!(e.kind(), io::ErrorKind::InvalidData, "{e}");

    let mut b = rs.serialize();
    b[8..12].copy_from_slice(&99u32.to_le_bytes());
    let e = RsCheckpoint::deserialize(&b, &lim(128)).err().expect("a foreign version must be rejected");
    assert_eq!(e.kind(), io::ErrorKind::InvalidData, "{e}");

    // 区间倒挂：放行会让 chunk 总数算式 old_end − old_start + 1 在 debug 下 panic
    const OLD_START: usize = 32;
    const OLD_END: usize = 40;
    let mut b = rs.serialize();
    b[OLD_START..OLD_START + 8].copy_from_slice(&3071u64.to_le_bytes());
    b[OLD_END..OLD_END + 8].copy_from_slice(&2048u64.to_le_bytes());
    let e = RsCheckpoint::deserialize(&b, &lim(128)).err().expect("an inverted range must be rejected");
    assert_eq!(e.kind(), io::ErrorKind::InvalidData, "{e}");

    // 分区号越上界
    const PART: usize = 28;
    let mut b = rs.serialize();
    b[PART..PART + 4].copy_from_slice(&129u32.to_le_bytes());
    let e = RsCheckpoint::deserialize(&b, &lim(128)).err().expect("an implausible partition number must be rejected");
    assert_eq!(e.kind(), io::ErrorKind::InvalidData, "{e}");

    // 出可用区（first_usable = 34）
    let mut b = rs.serialize();
    b[OLD_START..OLD_START + 8].copy_from_slice(&33u64.to_le_bytes());
    let e = RsCheckpoint::deserialize(&b, &lim(128)).err().expect("a range outside the usable area must be rejected");
    assert_eq!(e.kind(), io::ErrorKind::InvalidData, "{e}");

    // CRC 先于字段判据：撕一位必须先撞 CRC
    let mut b = rs.serialize();
    let last = b.len() - 1;
    b[last] ^= 0xFF;
    let e = RsCheckpoint::deserialize(&b, &lim(128)).err().expect("a torn tail must be rejected by CRC");
    assert_eq!(e.kind(), io::ErrorKind::InvalidData, "{e}");

    // chunk_bytes = 0 在 CRC 之后：重算 CRC 证明拦它的是判据本身
    const CHUNK_BYTES: usize = 73;
    let mut b = rs.serialize();
    b[CHUNK_BYTES..CHUNK_BYTES + 8].copy_from_slice(&0u64.to_le_bytes());
    rewrite_crc(&mut b);
    let e = RsCheckpoint::deserialize(&b, &lim(128)).err().expect("chunk_bytes = 0 must be rejected");
    assert_eq!(e.kind(), io::ErrorKind::InvalidData, "{e}");
}

/// 进度上界按拷贝总长（min(old, new)）算，不按旧区间全长：move+shrink 时写入侧
/// 只拷新旧区间的较小者，旧区间全长给出的上限会放过超过真实拷贝量的进度
#[test]
pub(crate) fn resize_ckpt_progress_bound_follows_copy_len() {
    // move+shrink：old = 1024 LBA、new = 512 LBA、位移 2048 LBA ≥ old_len ⇒ chunk 不收敛
    let rs = RsCheckpoint {
        proof: crate::dev::LiveProof::Inode,
        disk_size: 64 * 1024 * 1024, ss: 512, part: 1,
        old_start: 2048, old_end: 3071, new_start: 4096, new_end: 4607,
        fs_shrunk: FsShrink::NotShrunk, chunks_done: 0, chunk_bytes: 64 * 1024,
        fp: Default::default(),
        map: None,
        fstype: None,
    };
    let rewrite_crc = |b: &mut Vec<u8>| {
        let crc_off = b.len() - 4;
        let crc = table::crc32(&b[..crc_off]);
        b[crc_off..].copy_from_slice(&crc.to_le_bytes());
    };
    // 拷贝总长 = 512 × 512 = 262144 B = 4 × 65536 ⇒ 上限 4 chunk；
    // 旧区间全长 = 1024 × 512 = 524288 B ⇒ 8 chunk——5..8 这段只有按旧区间算才放行
    const CHUNKS_DONE: usize = 65;
    let mut b = rs.serialize();
    b[CHUNKS_DONE..CHUNKS_DONE + 8].copy_from_slice(&4u64.to_le_bytes());
    rewrite_crc(&mut b);
    RsCheckpoint::deserialize(&b, &lim(128)).expect("progress at the copy bound must stay readable");

    let mut b = rs.serialize();
    b[CHUNKS_DONE..CHUNKS_DONE + 8].copy_from_slice(&6u64.to_le_bytes());
    rewrite_crc(&mut b);
    let e = RsCheckpoint::deserialize(&b, &lim(128)).err().expect(
        "progress beyond the copy bound must be rejected even when it fits the old range",
    );
    assert_eq!(e.kind(), io::ErrorKind::InvalidData, "{e}");
}

/// 槽位被**另一族**作业（单分区 resize 的 RsCheckpoint）占着：plan 型搬移的两条生成
/// 路径都必须拒绝：若与空槽同归 `Empty`，plan 会现算一份计划并报
/// 成功，而 apply 在 prepare_apply 的分派处必然拒绝——同一条命令两个入口给出相反结论
#[test]
pub(crate) fn foreign_resize_ckpt_refuses_relocation_plans() {
    let (mut src, path) = plan_fixture(&[(1, [0x11; 16], 2048, 6143), (2, [0x22; 16], 8192, 10239)]);
    let ckpt_path = src.identity.checkpoint_path().to_path_buf();
    // 单分区 resize 作业的现场：分区号与几何取 fixture 里分区 1 的值，判定只看族别
    let rs = RsCheckpoint {
        proof: crate::dev::LiveProof::Inode,
        disk_size: src.size,
        ss: 512,
        part: 1,
        old_start: 2048,
        old_end: 6143,
        new_start: 2048,
        new_end: 6143,
        fs_shrunk: FsShrink::NotShrunk,
        chunks_done: 0,
        chunk_bytes: 1024 * 1024,
        fp: Default::default(),
        map: None,
        fstype: None,
    };
    atomic_write_ckpt(&ckpt_path, &rs.serialize()).unwrap();

    let e = plan_resuming(&mut src, 1).err().expect("a foreign resize job must refuse a fresh plan");
    assert!(matches!(&e, Fail::Refused(m) if m.contains("single-partition resize job")), "{e:?}");
    let e = plan_shift(&mut src, 1, Some(1024)).err().expect("the shift path must refuse it too");
    assert!(matches!(&e, Fail::Refused(m) if m.contains("single-partition resize job")), "{e:?}");
    // 归属仍判"本分区无搬移作业"：resize 自己要按这份 RsCheckpoint 续跑，不该被搬移的门挡住
    assert!(!pending_relocation(&src, 1).unwrap());

    drop(src);
    let _ = std::fs::remove_file(&ckpt_path);
    let _ = std::fs::remove_file(&path);
}

/// checkpoint 把身份种类写在 CRC 之前：三种可落盘种类原样回读；
/// 未知种类按损坏拒绝
#[test]
pub(crate) fn checkpoint_records_identity_kind() {
    use crate::dev::{IdentityProof, LiveProof};
    let id = 0x1122_3344_5566_7788u64;
    let mk = |proof: LiveProof| Checkpoint {
        proof,
        disk_size: 64 * 1024 * 1024,
        ss: 512,
        grow_part: 2,
        last_usable_lba: 100_000,
        moves: vec![PlanEntry { part_num: 1, first_lba: 2048, len_lba: 100, delta_lba: 200, is_swap: false }],
        kind: PlanKind::TailPacked,
        cur_index: 0,
        chunks_done: 0,
        chunk_bytes: 1024 * 1024,
        fp: crate::dev::TargetFingerprint { dev: 7, ino: 42, size: 1 << 20, id },
        map: None,
    };
    for p in [LiveProof::DeviceId, LiveProof::Inode, LiveProof::Unavailable] {
        let back = Checkpoint::deserialize(&mk(p).serialize(), &lim(128)).expect("a checkpoint must round-trip");
        assert_eq!(back.proof, IdentityProof::Live(p));
        assert_eq!(back.fp.id, id, "the device id must survive the round-trip");
    }

    // 布局（count = 1）：身份种类 @127
    const PROOF: usize = 127;
    let mut b = mk(LiveProof::Inode).serialize();
    b[PROOF] = 0; // 0 不是任何合法种类
    let crc_off = b.len() - 4;
    let crc = table::crc32(&b[..crc_off]);
    b[crc_off..].copy_from_slice(&crc.to_le_bytes());
    let e = Checkpoint::deserialize(&b, &lim(128)).err().expect("an unknown identity kind must be rejected");
    assert_eq!(e.kind(), io::ErrorKind::InvalidData, "{e}");
}

/// RsCheckpoint 同一条边界：两族 decoder 各自解析身份段
#[test]
pub(crate) fn resize_checkpoint_records_identity_kind() {
    use crate::dev::{IdentityProof, LiveProof};
    let id = 0x1122_3344_5566_7788u64;
    let mk = |proof: LiveProof| RsCheckpoint {
        proof,
        disk_size: 64 * 1024 * 1024, ss: 512, part: 1,
        old_start: 2048, old_end: 3071, new_start: 2048, new_end: 3071,
        fs_shrunk: FsShrink::NotShrunk, chunks_done: 0, chunk_bytes: 1024 * 1024,
        fp: crate::dev::TargetFingerprint { dev: 7, ino: 42, size: 1 << 20, id },
        map: None,
        fstype: Some(FsKind::Ext),
    };
    for p in [LiveProof::DeviceId, LiveProof::Inode, LiveProof::Unavailable] {
        let back = RsCheckpoint::deserialize(&mk(p).serialize(), &lim(128)).expect("a checkpoint must round-trip");
        assert_eq!(back.proof, IdentityProof::Live(p));
        assert_eq!(back.fp.id, id, "the device id must survive the round-trip");
        assert_eq!(back.fstype, Some(FsKind::Ext));
    }

    // 布局：身份种类 @115
    const PROOF: usize = 115;
    let mut b = mk(LiveProof::Inode).serialize();
    b[PROOF] = 0;
    let crc_off = b.len() - 4;
    let crc = table::crc32(&b[..crc_off]);
    b[crc_off..].copy_from_slice(&crc.to_le_bytes());
    let e = RsCheckpoint::deserialize(&b, &lim(128)).err().expect("an unknown identity kind must be rejected");
    assert_eq!(e.kind(), io::ErrorKind::InvalidData, "{e}");
}

/// 槽位被别的分区的作业占着：现算路径必须拒绝——当空槽会覆盖别人的现场，
/// 当自己的续跑点会执行与盘上几何不符的 plan；has_pending_relocation 对
/// 请求分区仍是"无作业"
#[test]
pub(crate) fn foreign_checkpoint_slot_refuses_fresh_plans() {
    let (mut src, path) = plan_fixture(&[(1, [0x11; 16], 2048, 6143), (2, [0x22; 16], 8192, 10239)]);
    let ckpt_path = src.identity.checkpoint_path().to_path_buf();
    // grow_part = 2 的作业记录，moves 内容任意：resume 判定只看归属
    let ck = Checkpoint {
        proof: crate::dev::LiveProof::Inode,
        disk_size: src.size,
        ss: 512,
        grow_part: 2,
        last_usable_lba: 32734,
        moves: vec![PlanEntry { part_num: 1, first_lba: 2048, len_lba: 4096, delta_lba: 2048, is_swap: false }],
        kind: PlanKind::TailPacked,
        cur_index: 0,
        chunks_done: 0,
        chunk_bytes: 1024 * 1024,
        fp: Default::default(),
        map: None,
    };
    atomic_write_ckpt(&ckpt_path, &ck.serialize()).unwrap();

    let e = plan_resuming(&mut src, 1).err().expect("a foreign job must refuse a fresh plan");
    assert!(matches!(&e, Fail::Refused(m) if m.contains("occupies the checkpoint slot")), "{e:?}");
    let e = plan_shift(&mut src, 1, Some(4096)).err().expect("a foreign job must refuse a fresh shift plan");
    assert!(matches!(&e, Fail::Refused(m) if m.contains("occupies the checkpoint slot")), "{e:?}");
    assert!(!pending_relocation(&src, 1).unwrap(), "the occupying job belongs to another partition");

    drop(src);
    let _ = std::fs::remove_file(&ckpt_path);
    let _ = std::fs::remove_file(&path);
}

/// 提交批量的自适应：耗时反馈只在重做窗口内择优，窗口是硬约束
#[test]
pub(crate) fn ckpt_batch_stays_inside_redo_window() {
    let fast = std::time::Duration::from_millis(5);
    let slow = std::time::Duration::from_millis(400);
    let mid = std::time::Duration::from_millis(100);
    // 4 MiB 块：窗口允许 256 MiB / 4 MiB = 64
    assert_eq!(next_ckpt_batch(CKPT_BATCH_CHUNKS, fast, 4 << 20), 32);
    assert_eq!(next_ckpt_batch(64, fast, 4 << 20), 64, "批量不得越过窗口与硬上界");
    assert_eq!(next_ckpt_batch(16, slow, 4 << 20), 8);
    assert_eq!(next_ckpt_batch(1, slow, 4 << 20), 1, "减半的下限是一次提交");
    assert_eq!(next_ckpt_batch(16, mid, 4 << 20), 16, "两个阈值之间不调整");
    // 块字节等于窗口上界：只能逐 chunk 提交，缩小批量以守住窗口
    assert_eq!(next_ckpt_batch(16, fast, REDO_WINDOW_BYTES), 1);
    assert_eq!(next_ckpt_batch(16, fast, REDO_WINDOW_BYTES * 4), 1);
    // 条目开始时的批量上界：4 MiB 块允许攒满 64，单块超过窗口则只能逐块
    assert_eq!(ckpt_batch_cap(4 << 20), 64);
    assert_eq!(ckpt_batch_cap(512 << 20), 1);
}
