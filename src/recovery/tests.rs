//! 状态转移表：每一行对应 [`Evidence`] 或清理许可的一次判定。

use super::*;
use crate::platform::{IdentityMaterial, PathError, TargetId};

fn scene(seed: u8) -> SceneIdentity {
    SceneIdentity::from_target(TargetId::from_material(
        IdentityMaterial::image(None, Some([seed; 16])).expect("image identity needs a file ID"),
    ))
}

fn record_for(target: SceneIdentity) -> Record {
    Record::new(
        TxId::from_bytes([9; 16]),
        target,
        PlanId::from_bytes([8; 16]),
        PlanVersion::new(1),
        OpKind::ResizePart,
        PlanDigest::from_bytes([7; 32]),
        crate::dev::DiskGuidState::Unavailable,
    )
}

fn encoded(target: SceneIdentity, retention: Retention, commit: CommitState) -> Vec<u8> {
    let mut record = record_for(target);
    record.retention = retention;
    record.commit = commit;
    record.encode().unwrap()
}

fn crc32(bytes: &[u8]) -> u32 {
    crc::Crc::<u32>::new(&crc::CRC_32_ISO_HDLC).checksum(bytes)
}

/// 编解码往返保留全部字段与头部
#[test]
fn record_roundtrip_preserves_every_field() {
    let mut record = record_for(scene(1));
    record.progress = Progress { completed_steps: 3, transfer_cursor: 4096, fs_phase: 2 };
    record.action = RecoveryAction::Rollback(8192);
    record.retention = Retention::CommittedPending;
    record.commit = CommitState::Committed;

    let bytes = record.encode().unwrap();
    let decoded = decode(&bytes).expect("freshly encoded record decodes");
    assert_eq!(decoded, record);
    assert_eq!(decoded.header.payload_len as usize, bytes.len() - HEADER_LEN);
}

/// `copy` 记录缺源区间时编码返回错误，不 panic
#[test]
fn copy_record_without_source_refuses_to_encode() {
    let mut record = record_for(scene(3));
    record.op = OpKind::Copy;
    assert!(record.encode().is_err());
}

/// LVM 收尾基线随记录往返；只有 `resize` 记录允许带它
#[test]
fn lvm_finish_baseline_belongs_to_resize_records() {
    let baseline = LvmPost { lv_size_before: 4096, lv_target: 8192, extent_bytes: 1 << 20 };

    let mut record = record_for(scene(2));
    record.op = OpKind::Resize;
    record.lvm_post = Some(baseline);
    let decoded = decode(&record.encode().unwrap()).expect("a resize record with a baseline decodes");
    assert_eq!(decoded.lvm_post, Some(baseline));

    let mut other = record_for(scene(2));
    other.op = OpKind::Move;
    other.lvm_post = Some(baseline);
    assert!(decode(&other.encode().unwrap()).is_err());
}

/// 搬移前几何随记录往返；只有会改动分区落位的操作允许带它
#[test]
fn pre_commit_range_belongs_to_placement_operations() {
    let old = (2048u64, 10239u64);

    let mut record = record_for(scene(3));
    record.old_range = Some(old);
    let decoded = decode(&record.encode().unwrap()).expect("a resize-part record with a pre-commit range decodes");
    assert_eq!(decoded.old_range, Some(old));

    let mut resize = record_for(scene(3));
    resize.op = OpKind::Resize;
    resize.old_range = Some(old);
    let decoded = decode(&resize.encode().unwrap()).expect("a resize record with a pre-commit range decodes");
    assert_eq!(decoded.old_range, Some(old));

    let mut moves = record_for(scene(3));
    moves.op = OpKind::Move;
    moves.old_range = Some(old);
    let decoded = decode(&moves.encode().unwrap()).expect("a move record with a pre-commit range decodes");
    assert_eq!(decoded.old_range, Some(old));

    let mut other = record_for(scene(3));
    other.op = OpKind::Copy;
    other.source = Some(CopySource {
        partition: 1,
        first_lba: 2048,
        sectors: 8,
        sector_size: 512,
        digest: [0x11; 32],
    });
    other.old_range = Some(old);
    assert!(decode(&other.encode().unwrap()).is_err());
}

fn reloc_entry(partition: u32, src_first: u64, len: u64, delta: i64, is_swap: bool) -> RelocEntry {
    RelocEntry { partition, src_first, len, delta, is_swap }
}

fn relocation(entries: Vec<RelocEntry>) -> Relocation {
    Relocation {
        entries,
        entry_index: 0,
        chunks_in_entry: 0,
        chunk_bytes: 1 << 20,
        kind: RelocKind::TailPacked,
        grow_part: 1,
        disk_size: 64 << 20,
        ss: 512,
    }
}

/// 搬移计划随记录往返：条目顺序就是执行顺序（编码链式依赖），不能靠重排恢复
#[test]
fn relocation_roundtrip_preserves_entry_order() {
    let plan = Relocation {
        entries: vec![
            reloc_entry(3, 10240, 64, 2048, true),
            reloc_entry(2, 6144, 4096, 2048, false),
            reloc_entry(4, 20000, 1024, 0, false),
        ],
        entry_index: 1,
        chunks_in_entry: 7,
        chunk_bytes: 1 << 20,
        kind: RelocKind::Shift,
        grow_part: 1,
        disk_size: 64 << 20,
        ss: 4096,
    };
    let mut record = record_for(scene(4));
    record.op = OpKind::Resize;
    record.relocation = Some(plan.clone());
    let decoded = decode(&record.encode().unwrap()).expect("a resize record with a relocation decodes");
    assert_eq!(decoded.relocation, Some(plan.clone()));
    assert_eq!(decoded.relocation.unwrap().entries, plan.entries);
}

/// 位移是有符号量：向左（Δ<0）的条目往返后仍是同一个负数，不被当成无符号量重读
#[test]
fn relocation_roundtrip_preserves_a_leftward_shift() {
    let plan = Relocation {
        entries: vec![reloc_entry(2, 6144, 4096, -2048, false)],
        ..relocation(Vec::new())
    };
    let mut record = record_for(scene(4));
    record.op = OpKind::Move;
    record.relocation = Some(plan.clone());
    let decoded = decode(&record.encode().unwrap()).expect("a leftward move record decodes");
    let got = decoded.relocation.expect("the relocation survives");
    assert_eq!(got.entries[0].delta, -2048);
    assert_eq!(got.entries, plan.entries);
}

/// swap 待重建由游标表达：entry_index 指向 swap 条目、chunks 为 0。记录里没有
/// swap 签名或重建标志，重跑只能据此补完而不是靠盘面猜
#[test]
fn swap_pending_is_encoded_by_the_cursor() {
    let plan = Relocation {
        entries: vec![reloc_entry(3, 10240, 64, 0, true), reloc_entry(2, 6144, 4096, 2048, false)],
        entry_index: 0,
        chunks_in_entry: 0,
        ..relocation(Vec::new())
    };
    let mut record = record_for(scene(5));
    record.op = OpKind::Resize;
    record.relocation = Some(plan);
    let decoded = decode(&record.encode().unwrap()).expect("a pending swap record decodes");
    let got = decoded.relocation.expect("the relocation survives");
    assert_eq!(got.entry_index, 0);
    assert_eq!(got.chunks_in_entry, 0);
    assert!(got.entries[0].is_swap);
}

/// 只有 `move` / `resize` / `resize-part` 记录允许带搬移计划
#[test]
fn relocation_belongs_to_move_resize_and_resize_part_records() {
    let mut allowed = record_for(scene(6));
    allowed.op = OpKind::Move;
    allowed.relocation = Some(relocation(vec![reloc_entry(2, 6144, 4096, 2048, false)]));
    assert!(decode(&allowed.encode().unwrap()).is_ok(), "a move record carries a relocation");

    let mut resize_part = record_for(scene(6));
    resize_part.op = OpKind::ResizePart;
    resize_part.relocation = Some(relocation(vec![reloc_entry(2, 6144, 4096, 2048, false)]));
    assert!(
        decode(&resize_part.encode().unwrap()).is_ok(),
        "a resize-part record carries the relocation of its start-change move"
    );

    let mut rejected = record_for(scene(6));
    rejected.op = OpKind::Add;
    rejected.relocation = Some(relocation(vec![reloc_entry(2, 6144, 4096, 2048, false)]));
    assert!(decode(&rejected.encode().unwrap()).is_err(), "Add must not carry a relocation");
}

/// 计划字段的非法组合在解码时拒绝：分区号 0 / 重复、零长度、grow_part 参与搬移、
/// 游标越过条目数、全体提交仍带 chunk 进度、块大小或扇区为 0、负位移越过 LBA 0
#[test]
fn relocation_rejects_impossible_jobs() {
    let legal = || relocation(vec![reloc_entry(2, 6144, 4096, 2048, false)]);
    let cases = [
        Relocation { entries: vec![reloc_entry(0, 6144, 4096, 2048, false)], ..legal() },
        Relocation { entries: vec![reloc_entry(2, 6144, 0, 2048, false)], ..legal() },
        Relocation { entries: vec![reloc_entry(2, 6144, 4096, 2048, false), reloc_entry(2, 8192, 512, 0, false)], ..legal() },
        Relocation { entries: vec![reloc_entry(1, 2048, 4096, 0, false)], ..legal() },
        // 负位移越过 LBA 0：目标端点算不出来
        Relocation { entries: vec![reloc_entry(2, 0, 4096, -1, false)], ..legal() },
        Relocation { entry_index: 2, ..legal() },
        Relocation { entry_index: 1, chunks_in_entry: 3, ..legal() },
        Relocation { chunk_bytes: 0, ..legal() },
        Relocation { ss: 0, ..legal() },
    ];
    for case in cases {
        let mut record = record_for(scene(7));
        record.op = OpKind::Resize;
        record.relocation = Some(case.clone());
        assert!(decode(&record.encode().unwrap()).is_err(), "must reject {case:?}");
    }
}

/// 载荷长度是形状的唯一声明：尾部多一个字节即整份拒绝
#[test]
fn records_refuse_payload_trailing_bytes() {
    let mut bytes = encoded(scene(8), Retention::Active, CommitState::Prepared);
    let declared = bytes.len() as u64;
    bytes.push(0);
    match decode(&bytes) {
        Err(DecodeFailure::Incomplete { observed, expected }) => {
            assert_eq!(observed, declared + 1);
            assert_eq!(expected, declared);
        }
        other => panic!("trailing bytes must not be read: {other:?}"),
    }
}

/// 证据四态的逐行判定
#[test]
fn evidence_rows_stay_in_their_own_state() {
    let current = scene(1);
    let good = encoded(current.clone(), Retention::Active, CommitState::Prepared);
    let total = good.len() as u64;

    // 不存在的路径
    assert_eq!(Evidence::absent(), Evidence::Absent);

    // magic 不属于本工具
    match Evidence::classify(&[0x58u8; 40], &current) {
        Evidence::UnrecognizedFormat { magic, recorded } => {
            assert_eq!(magic, [0x58; 4]);
            assert_eq!(recorded, None);
        }
        other => panic!("unknown magic became {other:?}"),
    }

    // magic 是本工具的、载荷形状不认识（长度与两段 CRC 自洽，但内容多一个字节）
    let mut unknown_shape = good.clone();
    unknown_shape.push(0);
    let payload_len = (unknown_shape.len() - HEADER_LEN) as u32;
    unknown_shape[4..8].copy_from_slice(&payload_len.to_le_bytes());
    let payload_crc = crc32(&unknown_shape[HEADER_LEN..]);
    unknown_shape[8..12].copy_from_slice(&payload_crc.to_le_bytes());
    let header_crc = crc32(&unknown_shape[..12]);
    unknown_shape[12..16].copy_from_slice(&header_crc.to_le_bytes());
    match Evidence::classify(&unknown_shape, &current) {
        Evidence::UnrecognizedFormat { magic, recorded } => {
            assert_eq!(magic, MAGIC);
            assert_eq!(recorded, None);
        }
        other => panic!("unknown shape became {other:?}"),
    }

    // 头还没读满
    match Evidence::classify(&good[..12], &current) {
        Evidence::Incomplete { observed, expected } => {
            assert_eq!(observed, 12);
            assert_eq!(expected, HEADER_LEN as u64);
        }
        other => panic!("truncated header became {other:?}"),
    }

    // 头 CRC 不符（长度字段被改、CRC 未重算）
    let mut damaged_header = good.clone();
    damaged_header[10] ^= 0xFF;
    assert_eq!(
        Evidence::classify(&damaged_header, &current),
        Evidence::CrcMismatch { region: CrcRegion::Header }
    );

    // 头完整、载荷被截短
    match Evidence::classify(&good[..good.len() - 8], &current) {
        Evidence::Incomplete { observed, expected } => {
            assert_eq!(observed, total - 8);
            assert_eq!(expected, total);
        }
        other => panic!("truncated payload became {other:?}"),
    }

    // 载荷 CRC 不符
    let mut damaged_payload = good.clone();
    let last = damaged_payload.len() - 1;
    damaged_payload[last] ^= 0xFF;
    assert_eq!(
        Evidence::classify(&damaged_payload, &current),
        Evidence::CrcMismatch { region: CrcRegion::Payload }
    );

    // 记录比声明的长度更长
    let mut longer = good.clone();
    longer.extend_from_slice(&[0xAA; 4]);
    match Evidence::classify(&longer, &current) {
        Evidence::Incomplete { observed, expected } => {
            assert_eq!(observed, total + 4);
            assert_eq!(expected, total);
        }
        other => panic!("overlong record became {other:?}"),
    }

    // 全部检查通过、身份相同
    let evidence = Evidence::classify(&good, &current);
    assert!(matches!(evidence, Evidence::Recognized(_)));

    // 全部检查通过、身份不同
    match Evidence::classify(&good, &scene(2)) {
        Evidence::ForeignTarget { recorded, current: seen } => {
            assert_eq!(recorded, current);
            assert_eq!(seen, scene(2));
        }
        other => panic!("foreign record became {other:?}"),
    }

    // 身份材料相同、loop 映射不同 ⇒ 不是同一目标
    let mapped = SceneIdentity::with_details(
        current.id().clone(),
        Some(Mapping::new(7, 8)),
        SceneProof::FileId,
        None,
        None,
    );
    let mapped_bytes = encoded(mapped, Retention::Active, CommitState::Prepared);
    assert!(matches!(
        Evidence::classify(&mapped_bytes, &current),
        Evidence::ForeignTarget { .. }
    ));

    // 留存状态不阻止识别
    let unverified = encoded(current.clone(), Retention::Unverified, CommitState::Prepared);
    assert!(matches!(Evidence::classify(&unverified, &current), Evidence::Recognized(_)));

    // 路径式 inspect 没有当前目标可比，按结构仍判识别成功
    assert!(matches!(Evidence::classify_structure(&good), Evidence::Recognized(_)));
}

/// 路径解析失败映射到各自的证据形态
#[test]
fn path_errors_map_to_separate_evidence_states() {
    assert_eq!(Evidence::from_path_error(PathError::NotFound), Evidence::Absent);
    assert_eq!(
        Evidence::from_path_error(PathError::Symlink),
        Evidence::not_ours(EntryKind::Symlink)
    );
    assert_eq!(
        Evidence::from_path_error(PathError::IsDirectory),
        Evidence::not_ours(EntryKind::Directory)
    );
    assert_eq!(
        Evidence::from_path_error(PathError::NotAFile),
        Evidence::not_ours(EntryKind::OtherFile)
    );
    assert!(matches!(
        Evidence::from_path_error(PathError::Inaccessible("permission denied".to_string())),
        Evidence::Unreadable { .. }
    ));
}

/// 解析类失败不携带记录，也无法进入提交
#[test]
fn parse_failures_never_commit() {
    let current = scene(1);
    let failures = [
        Evidence::absent(),
        Evidence::not_ours(EntryKind::Symlink),
        Evidence::unreadable(Cause::Io("read failed".to_string())),
        Evidence::Incomplete { observed: 0, expected: 1 },
        Evidence::CrcMismatch { region: CrcRegion::Header },
        Evidence::UnrecognizedFormat { magic: [0; 4], recorded: None },
        Evidence::ForeignTarget { recorded: scene(2), current: current.clone() },
    ];
    for evidence in failures {
        assert!(evidence.recognized_record().is_none(), "{evidence:?}");
        assert!(RecoveryEngine::commit(evidence.clone(), false).is_err(), "{evidence:?}");
    }
}

/// 清理许可的逐行判定：留存状态、待办与提交前状态各自一行
#[test]
fn cleanup_permit_rows() {
    let current = scene(1);
    let rows = [
        ("active", Retention::Active, false, true, None),
        ("committed pending", Retention::CommittedPending, false, true, None),
        (
            "unverified",
            Retention::Unverified,
            false,
            false,
            Some(NoPermitReason::Retention(Retention::Unverified)),
        ),
        (
            "quarantined",
            Retention::Quarantined,
            false,
            false,
            Some(NoPermitReason::Retention(Retention::Quarantined)),
        ),
        (
            "active with a table change pending",
            Retention::Active,
            true,
            false,
            Some(NoPermitReason::PendingTableChange),
        ),
    ];

    for (name, retention, pending, expect_permit, expect_reason) in rows {
        let evidence = Evidence::classify(
            &encoded(current.clone(), retention, CommitState::Prepared),
            &current,
        );
        let outcome = RecoveryEngine::commit(evidence, pending)
            .expect("a recognized record commits");
        assert_eq!(outcome.permit().is_some(), expect_permit, "{name}");
        assert_eq!(outcome.withheld(), expect_reason, "{name}");
        assert_eq!(outcome.committed().record().commit, CommitState::Committed, "{name}");
    }

    // 已经提交、上次清理未完成的现场：再次提交仍可签发许可
    let evidence = Evidence::classify(
        &encoded(current.clone(), Retention::Active, CommitState::Committed),
        &current,
    );
    let outcome = RecoveryEngine::commit(evidence, false).expect("committed field commits");
    assert!(outcome.permit().is_some());
}

/// 回滚载荷按扇区粒度往返：完整扇区，以及文件末尾只存到实际长度的半扇区
#[test]
fn restore_entries_roundtrip_by_sector() {
    let mut record = record_for(scene(1));
    record.restore = vec![
        RestoreEntry {
            sector_size: 512,
            first_lba: 1,
            sectors: 2,
            original: vec![0xA5; 1024],
            after: vec![0x5A; 1024],
        },
        RestoreEntry {
            sector_size: 4096,
            first_lba: 7,
            sectors: 1,
            original: vec![0x5A; 300],
            after: vec![0xA5; 300],
        },
    ];
    let bytes = record.encode().unwrap();
    let decoded = decode(&bytes).expect("restore entries decode");
    assert_eq!(decoded.restore, record.restore);
    assert!(decoded.restore.iter().all(RestoreEntry::is_well_formed));
    assert_eq!(decoded.restore[0].byte_offset(), Some(512));
    assert_eq!(decoded.restore[1].byte_offset(), Some(7 * 4096));
}

/// 形状不成立的载荷被拒：零扇区、非 2 的幂、空字节、超出所声明的扇区区间
#[test]
fn malformed_restore_entries_are_refused() {
    let cases = [
        RestoreEntry {
            sector_size: 512,
            first_lba: 0,
            sectors: 0,
            original: vec![1],
            after: vec![1],
        },
        RestoreEntry {
            sector_size: 100,
            first_lba: 0,
            sectors: 1,
            original: vec![0; 100],
            after: vec![0; 100],
        },
        RestoreEntry {
            sector_size: 512,
            first_lba: 0,
            sectors: 1,
            original: Vec::new(),
            after: Vec::new(),
        },
        RestoreEntry {
            sector_size: 512,
            first_lba: 0,
            sectors: 1,
            original: vec![0; 513],
            after: vec![0; 513],
        },
    ];
    for entry in cases {
        assert!(!entry.is_well_formed(), "{entry:?}");
    }

    let overflow = RestoreEntry {
        sector_size: 512,
        first_lba: u64::MAX,
        sectors: 1,
        original: vec![0; 512],
        after: vec![0; 512],
    };
    assert_eq!(overflow.byte_offset(), None);
}

/// 零写入的 resize 记录：MBR 表项路径只落旧区间、不落搬移表，记录自身即证明未动工；
/// 进度一旦前进，缺搬移表就不再足以判定，按不可判定拒绝
#[test]
fn a_zero_write_resize_record_without_relocation_is_planned() {
    let mut record = record_for(scene(4));
    record.op = OpKind::Resize;
    assert!(record.relocation.is_none());
    assert_eq!(record.stage().unwrap(), Stage::Planned);

    record.progress.completed_steps = 1;
    assert_eq!(record.stage().unwrap(), Stage::Undecidable);
}

/// 记录目录里的一份空记录，用于观测绑定与表步的落盘结果
#[cfg(target_os = "linux")]
fn journal_in(dir: &crate::testdir::TestDir, op: OpKind) -> RecordJournal {
    let path = dir.path().join("t.diskedit").join("record");
    std::fs::create_dir_all(path.parent().expect("the record path has a directory")).unwrap();
    RecordJournal::create_image(
        &path,
        scene(1),
        op,
        512,
        crate::dev::DiskGuidState::Unavailable,
        None,
    )
    .expect("a fresh record opens in a writable directory")
}

#[cfg(target_os = "linux")]
fn relocation_of(entries: Vec<RelocEntry>, entry_index: u32) -> Relocation {
    Relocation {
        entries,
        entry_index,
        chunks_in_entry: 0,
        chunk_bytes: 1024 * 1024,
        kind: RelocKind::Shift,
        grow_part: 1,
        disk_size: 16 * 1024 * 1024,
        ss: 512,
    }
}

/// 没有条目的搬移计划绑不进记录：空计划没有可续跑的对象
#[cfg(target_os = "linux")]
#[test]
fn an_empty_relocation_plan_is_refused_at_bind() {
    let dir = crate::testdir::test_dir("journal_empty_relocation");
    let mut journal = journal_in(&dir, OpKind::Move);
    let err = journal.bind_relocation(relocation_of(Vec::new(), 0), None).unwrap_err();
    assert!(err.to_string().contains("no entries"), "{err}");
    assert!(journal.record().relocation.is_none(), "a refused bind leaves no plan in the record");
}

/// 条目越界的表步开始不是可跳过的调用：拒绝，而不是把少写的一条事实当成功
#[cfg(target_os = "linux")]
#[test]
fn a_table_step_outside_the_recorded_plan_is_refused() {
    let dir = crate::testdir::test_dir("journal_entry_bounds");
    let mut journal = journal_in(&dir, OpKind::Move);
    let entry = RelocEntry { partition: 1, src_first: 2048, len: 4096, delta: 4096, is_swap: false };
    journal
        .bind_relocation(relocation_of(vec![entry], 0), None)
        .expect("a plan with one entry binds");
    let err = journal.mark_entry_table_in_flight(1).unwrap_err();
    assert!(err.to_string().contains("outside the recorded relocation plan"), "{err}");
    assert_eq!(journal.record().entry_table_in_flight, None);
    assert!(
        journal.mark_entry_table_in_flight(0).is_ok(),
        "the current entry still records its table step"
    );
}
