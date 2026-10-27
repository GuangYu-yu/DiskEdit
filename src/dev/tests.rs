#![allow(clippy::let_underscore_must_use)] // 清理临时文件有意忽略失败
use super::*;

/// `:N` 后缀的判定：分区号 1-based，`:0` 必须拒绝而不是折叠成"整盘"——
/// 折叠会把一次针对具体分区的操作放大成对整盘的表操作。
/// `:last` 与数字号并列，大小写不敏感
#[test]
pub(crate) fn parse_target_partition_suffix() {
    assert_eq!(parse_target("img").unwrap(), ("img".to_string(), None));
    assert_eq!(parse_target("img:1").unwrap(), ("img".to_string(), Some(PartSelector::Number(1))));
    assert_eq!(parse_target("img:4294967295").unwrap(), ("img".to_string(), Some(PartSelector::Number(u32::MAX))));
    assert!(parse_target("img:0").is_err());
    assert!(parse_target("img:4294967296").is_err());
    assert!(parse_target("img:").is_err());
    assert_eq!(parse_target("img:last").unwrap(), ("img".to_string(), Some(PartSelector::Last)));
    assert_eq!(parse_target("img:LAST").unwrap(), ("img".to_string(), Some(PartSelector::Last)));
    // 文件名的冒号不是分区后缀（其后不是纯数字，也不是 last）
    assert_eq!(parse_target("/a/b:c.img").unwrap(), ("/a/b:c.img".to_string(), None));
}

pub(crate) fn journal_path(tag: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("diskedit_jr_{tag}_{}.log", std::process::id()));
    let _ = std::fs::remove_file(&p);
    p
}

/// 测试用的目标身份：全零指纹 + 无 loop 映射 = "取不到稳定指纹的载体"那一类，
/// 身份核对因此不参与（判据见 `scene_mismatch`）。需要核对时另给具体值
pub(crate) fn jid() -> LiveScene {
    LiveScene { fp: TargetFingerprint::default(), map: None, proof: LiveProof::Inode }
}

/// 取错误文本。不用 `unwrap_err`：那要求 Ok 侧实现 Debug，而这些类型不必为了测试去实现它
pub(crate) fn err_text<T>(r: io::Result<T>) -> String {
    match r {
        Ok(_) => panic!("expected an error"),
        Err(e) => e.to_string(),
    }
}

/// "这是不是本工具的 journal"看的是 `DEJL` 前缀与可读版本。当前版本才可读，
/// 故 `DEJL` 之后的版本字节不是当前值即不算
#[test]
pub(crate) fn own_header_covers_every_readable_version() {
    assert!(Journal::is_own_header(Journal::MAGIC));
    assert!(!Journal::is_own_header(b"DEJL\x03"), "an unreadable version is not ours");
    assert!(!Journal::is_own_header(b"XXXX\x05"), "a foreign magic is not ours");
}

/// 四态分类的边界：只有 NotFound 是 absent；0 字节残骸与非普通文件也不是现场；
/// 零记录的头是 shell；半截头与陌生文件是 damaged
#[test]
pub(crate) fn journal_candidate_classification_covers_every_state() {
    let p = journal_path("classify");
    assert!(matches!(classify_journal(&p), JournalCandidate::Absent), "a missing path is absent");

    std::fs::write(&p, b"").unwrap();
    assert!(matches!(classify_journal(&p), JournalCandidate::Absent), "a zero-byte remnant is not a scene");

    drop(Journal::open(&p, jid()).unwrap());
    assert!(matches!(classify_journal(&p), JournalCandidate::Shell), "a header with no records is a shell");

    std::fs::write(&p, b"DEJ").unwrap();
    assert!(matches!(classify_journal(&p), JournalCandidate::Damaged(_)), "a torn header must not be provable as empty");

    let _ = std::fs::remove_file(&p);
    let mut j = Journal::open(&p, jid()).unwrap();
    j.record(0, &[0xAA; 4], &[0xBB; 4]).unwrap();
    drop(j);
    assert!(
        matches!(classify_journal(&p), JournalCandidate::Records(JournalRead::Complete(v)) if v.len() == 1),
        "a journal with a complete record must be classified as records"
    );

    let _ = std::fs::remove_file(&p);
    std::fs::create_dir(&p).unwrap();
    assert!(
        matches!(classify_journal(&p), JournalCandidate::Damaged(_)),
        "a non-regular-file path is not this tool's journal and must not be adopted"
    );
    let _ = std::fs::remove_dir(&p);
}

/// 属性读不出来不是"不在"：只有 `NotFound` 才算缺席，其余错误必须按 `Damaged` 收口。
/// 权限位在 root 下不生效，机器上造不出这个错误时跳过
#[cfg(target_os = "linux")]
#[test]
pub(crate) fn candidate_classification_fails_closed_on_stat_errors() {
    use std::os::unix::fs::PermissionsExt;
    if unsafe { libc::geteuid() } == 0 {
        return;
    }
    let dir = std::env::temp_dir().join(format!("diskedit_classify_denied_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let p = dir.join("j.journal");
    std::fs::write(&p, b"DEJL").unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o000)).unwrap();
    let got = classify_journal(&p);
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert!(matches!(got, JournalCandidate::Damaged(_)), "a stat error must fail closed");
    let _ = std::fs::remove_dir_all(&dir);
}

/// 分区节点不暴露 loop 属性：沿 `partition` 属性上溯到整设备节点再读。
/// 非 loop 的盘同样上溯后得到"没有 backing_file"，不是"读不出来"
#[cfg(target_os = "linux")]
#[test]
pub(crate) fn loop_backing_is_read_from_the_device_above_the_partition() {
    let root = std::env::temp_dir().join(format!("diskedit_sysfs_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("loop0/loop")).unwrap();
    std::fs::create_dir_all(root.join("loop0/loop0p1")).unwrap();
    std::fs::create_dir_all(root.join("sda/sda1")).unwrap();
    std::fs::write(root.join("loop0/loop/backing_file"), "/tmp/backing.img\n").unwrap();
    std::fs::write(root.join("loop0/loop0p1/partition"), "1\n").unwrap();
    std::fs::write(root.join("sda/sda1/partition"), "1\n").unwrap();

    assert_eq!(
        loop_backing_at(&root.join("loop0/loop0p1")).unwrap(),
        Some(PathBuf::from("/tmp/backing.img"))
    );
    assert_eq!(loop_backing_at(&root.join("loop0")).unwrap(), Some(PathBuf::from("/tmp/backing.img")));
    assert_eq!(loop_backing_at(&root.join("sda/sda1")).unwrap(), None);

    let _ = std::fs::remove_dir_all(&root);
}

/// 指纹的设备 ID 与容量都取**整设备节点**：以分区节点解析与以整盘节点解析得到同一份
/// 指纹，故整盘与它的分区共享同一份恢复现场。`dm/name` 不在身份证据里——映射名不是身份，
/// 只有它可读时按"无 ID"处理（容量仍作为辅助证据）
#[cfg(target_os = "linux")]
#[test]
pub(crate) fn disk_identity_is_shared_by_a_partition_and_its_whole_disk() {
    let root = std::env::temp_dir().join(format!("diskedit_diskid_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("sda/sda1")).unwrap();
    std::fs::create_dir_all(root.join("dm-0/dm")).unwrap();
    std::fs::write(root.join("sda/size"), "2048\n").unwrap();
    std::fs::write(root.join("sda/wwid"), "naa.5000c500aabbccdd\n").unwrap();
    std::fs::write(root.join("sda/sda1/partition"), "1\n").unwrap();
    std::fs::write(root.join("sda/sda1/size"), "1024\n").unwrap();
    // dm/name 是可读的，但它不进身份证据
    std::fs::write(root.join("dm-0/size"), "4096\n").unwrap();
    std::fs::write(root.join("dm-0/dm/name"), "vg-data\n").unwrap();

    let whole = disk_identity(&root.join("sda")).unwrap();
    assert_eq!(whole, (2048 * 512, Some("wwid=naa.5000c500aabbccdd".to_string())));
    assert_eq!(disk_identity(&root.join("sda/sda1")).unwrap(), whole);
    assert_eq!(disk_identity(&root.join("dm-0")).unwrap(), (4096 * 512, None));

    let _ = std::fs::remove_dir_all(&root);
}

/// 字面路径与 canonical 指同一文件（相对路径 / `./` / `..` 这类非规范化成分）时，
/// 候选只能列一份：列两份会让消费侧把"只有一个现场"判成 Ambiguous 而全面拒绝。
/// 真·不同文件（符号链接）仍要保留双候选
#[test]
pub(crate) fn non_canonical_literal_does_not_duplicate_the_candidate() {
    let dir = std::env::temp_dir();
    let img = dir.join(format!("diskedit_iddup_{}.img", std::process::id()));
    std::fs::write(&img, b"x").unwrap();
    let canonical = std::fs::canonicalize(&img).unwrap();
    // 同一文件的另一条写法：`<tmp>/sub/../x.img` 文本上不同于 canonical，指向同一处
    let sub = dir.join(format!("diskedit_iddup_sub_{}", std::process::id()));
    std::fs::create_dir_all(&sub).unwrap();
    let literal = sub.join("..").join(canonical.file_name().unwrap());
    assert_ne!(literal, canonical, "the fixture must be textually different");
    let id = TargetIdentity::image_at(&canonical, &literal);
    assert_eq!(id.journal.len(), 1, "one file must yield one journal candidate: {:?}", id.journal);
    assert_eq!(id.checkpoint.len(), 1, "one file must yield one checkpoint candidate: {:?}", id.checkpoint);
    assert_eq!(id.journal[0], suffix_path(&canonical, JOURNAL_SUFFIX));

    let _ = std::fs::remove_file(&img);
    let _ = std::fs::remove_dir(&sub);
}

/// 尾部撕裂是"未完成的事务"，不是损坏：丢弃那一条、回放完整前缀；
/// 中途 CRC 不符才是损坏，仍整体拒绝（"不做部分回放"的契约只对后者生效）
#[test]
pub(crate) fn journal_tail_truncation_is_recovered_but_corruption_is_not() {
    let p = journal_path("tail");
    const DATA: usize = 512;
    // [len u32][crc u32] + [mutation u8][recovery u8][off u64][pre_len u32][data]×2（PartitionTable 无 meta）
    const REC: usize = REC_HDR + 14 + 2 * DATA;
    {
        let mut j = Journal::open(&p, jid()).unwrap();
        for i in 0..3u64 {
            j.record(1024 * i, &[0xAA; DATA], &[0xBB; DATA]).unwrap();
        }
    }
    assert!(matches!(
        Journal::read_entries(&p).unwrap(),
        JournalRead::Complete(v) if v.len() == 3
    ));

    // 把最后一条记录截掉任意一段非空的长度 → 都应是"2 条完整 + 尾部未完成"。
    // 上界取 REC（不含）：截满 REC 等于整条消失，那就不再是撕裂而是"到此为止"
    let full = std::fs::read(&p).unwrap();
    for cut in 1..REC {
        std::fs::write(&p, &full[..full.len() - cut]).unwrap();
        match Journal::read_entries(&p).unwrap() {
            JournalRead::TruncatedTail { records, cut: torn } => {
                assert_eq!(records.len(), 2, "cut {cut}");
                // 报出的是"回滚到上一条完整记录所丢掉的字节数"，即整条记录的剩余部分
                assert_eq!(torn.get(), REC - cut, "cut {cut}");
            }
            JournalRead::Complete(v) => {
                panic!("cut {cut}: expected a truncated tail, got {} complete record(s)", v.len())
            }
        }
    }
    // 截满一条 = 该记录整条没写进去，此时应报 Complete(2)，不是撕裂
    std::fs::write(&p, &full[..full.len() - REC]).unwrap();
    assert!(matches!(
        Journal::read_entries(&p).unwrap(),
        JournalRead::Complete(v) if v.len() == 2
    ));

    // 中途（非尾部）CRC 损坏 → 整体拒绝
    let mut corrupt = full.clone();
    corrupt[Journal::HEADER_LEN + REC_HDR + 10 + 100] ^= 0xFF; // 第 1 条记录的 pre-image 字节中段
    std::fs::write(&p, &corrupt).unwrap();
    assert!(Journal::read_entries(&p).is_err(), "mid-file corruption must be refused");

    let _ = std::fs::remove_file(&p);
}

/// `len` 参与记录 CRC：长度字段被改小后按校验失败拒绝，不再被当成"载荷只写了一半"
#[test]
pub(crate) fn journal_crc_covers_the_length_field() {
    let p = journal_path("v5len");
    {
        let mut j = Journal::open(&p, jid()).unwrap();
        j.record(0, &[0xAA; 16], &[0xBB; 16]).unwrap();
    }
    let mut bytes = std::fs::read(&p).unwrap();
    assert_eq!(&bytes[..5], Journal::MAGIC);
    let at = Journal::HEADER_LEN;
    let n = u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap());
    bytes[at..at + 4].copy_from_slice(&(n - 1).to_le_bytes());
    std::fs::write(&p, &bytes).unwrap();
    let e = err_text(Journal::read_entries(&p));
    assert!(e.contains("CRC mismatch"), "a corrupted len must be a CRC failure: {e}");

    let _ = std::fs::remove_file(&p);
}

/// 头把身份种类写在身份段最后一个字节：三种可落盘种类原样读回。这一字节是"录制时取不到
/// 身份"的唯一分界，故未知值一律按头损坏拒绝，不降级成某个"没有身份"的种类——
/// 降级会把缺身份的块设备现场当成别的载体放过
#[test]
pub(crate) fn journal_records_identity_kind_and_refuses_an_unknown_one() {
    let with = |proof| LiveScene {
        fp: TargetFingerprint { dev: 7, ino: 42, size: 1 << 20, id: 0x1122_3344_5566_7788 },
        map: Some((4096, 8192)),
        proof,
    };
    for (proof, tag) in [
        (LiveProof::DeviceId, 1u8),
        (LiveProof::Inode, 2),
        (LiveProof::Unavailable, 3),
    ] {
        let bytes = Journal::header(&with(proof));
        assert_eq!(bytes[Journal::MAGIC.len() + IDENTITY_LEN - 1], tag);
        let back = Journal::parse_header(&bytes).unwrap();
        assert_eq!(
            (back.fp, back.map, back.proof),
            (with(proof).fp, with(proof).map, IdentityProof::from(proof))
        );
    }

    // 0 不是任何合法种类：头 CRC 通过也不得降级成别的种类
    let mut bytes = Journal::header(&with(LiveProof::Inode));
    bytes[Journal::MAGIC.len() + IDENTITY_LEN - 1] = 0;
    let crc_at = bytes.len() - 4;
    let crc = crate::table::crc32(&bytes[..crc_at]);
    bytes[crc_at..].copy_from_slice(&crc.to_le_bytes());
    let e = err_text(Journal::parse_header(&bytes));
    assert!(e.contains("identity kind"), "an unknown identity kind must be refused: {e}");
}

/// 惰性创建：打开只做校验、不落痕迹，第一条记录才建文件。若 open 就建，一条在被拒阶段
/// 结束的命令会留下空 journal，让后续 undo 分不清"没有 journal"与"journal 是空的"
#[test]
pub(crate) fn journal_is_created_lazily() {
    let p = journal_path("lazy");
    let mut j = Journal::open(&p, jid()).unwrap();
    assert!(!p.exists(), "opening a journal must not leave a trace on disk");
    j.record(0, &[0xAA; 4], &[0xBB; 4]).unwrap();
    assert!(p.exists(), "the first record must materialize the file");
    drop(j);

    // 已有文件必须是我们的 journal：陌生文件不得被当作 journal 追加
    std::fs::write(&p, b"not-a-journal").unwrap();
    assert!(Journal::open(&p, jid()).is_err(), "a foreign file must be refused, not adopted");

    let _ = std::fs::remove_file(&p);
}

/// 换格式后的头边界：旧版本的 journal 必须报"版本不支持"（不是损坏），且拒绝时**一字不动**；
/// 当前版本的头要能被自己写出去、读回来，身份逐字段一致。
/// v3 的字节取自本机一份真实的 v3 journal（magic `DEJL\x03` + 一条 RelocatePartition 记录）
#[test]
pub(crate) fn journal_format_boundary_refuses_older_versions_and_keeps_them_intact() {
    let p = journal_path("format_v3");
    let mut v3 = b"DEJL\x03".to_vec();
    v3.extend_from_slice(&[
        0x16, 0x00, 0x00, 0x00, 0xdd, 0xc5, 0xf3, 0x92, 0x01, 0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0xb0, 0x04,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x48, 0x06, 0x00, 0x00, 0x00, 0x00, 0x01,
    ]);
    assert_eq!(v3.len(), 35, "the fixture must be the real v3 byte sequence");
    std::fs::write(&p, &v3).unwrap();
    let e = err_text(Journal::read_entries(&p));
    assert!(e.contains("v3") && e.contains("not supported"), "an older version must be reported as such: {e}");
    assert!(!e.contains("CRC"), "an older version must not be reported as record corruption: {e}");
    assert_eq!(std::fs::read(&p).unwrap(), v3, "refusing an older version must leave the file untouched");
    // 采纳路径（写命令打开目标时）同样拒绝，同样不动文件——旧现场只能由 abandon 释放
    let e = err_text(Journal::open(&p, jid()));
    assert!(e.contains("v3"), "{e}");
    assert_eq!(std::fs::read(&p).unwrap(), v3, "the adoption path must leave the older version untouched");
    let _ = std::fs::remove_file(&p);

    // 写出去的头 + 身份，读回来逐字段一致
    let q = journal_path("format_roundtrip");
    let mine = LiveScene { fp: TargetFingerprint { dev: 7, ino: 42, size: 1 << 20, id: 0 }, map: Some((4096, 8192)), proof: LiveProof::Inode };
    {
        let mut j = Journal::open(&q, mine).unwrap();
        j.record(0, &[0xAA; 8], &[0xBB; 8]).unwrap();
    }
    let got = Journal::read_identity(&q).unwrap();
    assert_eq!((got.fp.dev, got.fp.ino, got.fp.size), (7, 42, 1 << 20));
    assert_eq!(got.map, Some((4096, 8192)));
    assert!(matches!(Journal::read_entries(&q).unwrap(), JournalRead::Complete(v) if v.len() == 1));
    // 指纹不符 = 同一路径下换了文件：不采纳，且不动那份 journal
    let before = std::fs::read(&q).unwrap();
    let other = LiveScene { fp: TargetFingerprint { dev: 7, ino: 43, size: 1 << 20, id: 0 }, map: Some((4096, 8192)), proof: LiveProof::Inode };
    let e = err_text(Journal::open(&q, other));
    assert!(e.contains("fingerprint"), "{e}");
    assert_eq!(std::fs::read(&q).unwrap(), before, "a foreign identity must not be adopted or modified");
    // loop 映射的有无之别同样拦下：同一 LBA 在两条途径下指向不同字节
    let no_map = LiveScene { fp: TargetFingerprint { dev: 7, ino: 42, size: 1 << 20, id: 0 }, map: None, proof: LiveProof::Inode };
    assert!(err_text(Journal::open(&q, no_map)).contains("loop device"));
    assert_eq!(std::fs::read(&q).unwrap(), before, "no mismatch may modify the journal");
    let _ = std::fs::remove_file(&q);
}

/// "头不完整 / 头被改坏 / 记录被改坏"三种都拒绝；**只有 0 字节**的残骸按
/// "零次写入"处理（补头继续用）。1..HEADER_LEN-1 字节一律拒绝：那既可能是初始化没写完，
/// 也可能是原 journal 被外部截断（记录已丢），无法证明是前者，就不当后者不存在
#[test]
pub(crate) fn journal_truncated_or_damaged_is_refused_and_only_a_zero_byte_remnant_is_rewritten() {
    let p = journal_path("format_bad");
    let id = LiveScene { fp: TargetFingerprint { dev: 7, ino: 42, size: 1 << 20, id: 0 }, map: None, proof: LiveProof::Inode };
    {
        let mut j = Journal::open(&p, id).unwrap();
        j.record(0, &[0xAA; 8], &[0xBB; 8]).unwrap();
    }
    let full = std::fs::read(&p).unwrap();
    assert!(full.len() > Journal::HEADER_LEN);

    // 头截断
    std::fs::write(&p, &full[..Journal::HEADER_LEN - 1]).unwrap();
    assert!(err_text(Journal::read_entries(&p)).contains("truncated"));
    // 头被改坏：身份段的自带 CRC 先报出来，而不是拿改坏后的指纹去比出"指纹不符"
    let mut bad = full.clone();
    bad[Journal::MAGIC.len() + 8] ^= 0xFF;
    std::fs::write(&p, &bad).unwrap();
    assert!(err_text(Journal::read_entries(&p)).contains("header CRC"));
    // 记录被改坏
    let mut bad = full.clone();
    bad[Journal::HEADER_LEN + REC_HDR] ^= 0xFF;
    std::fs::write(&p, &bad).unwrap();
    assert!(Journal::read_entries(&p).is_err());
    // 原样写回 → 又能读回
    std::fs::write(&p, &full).unwrap();
    assert!(matches!(Journal::read_entries(&p).unwrap(), JournalRead::Complete(v) if v.len() == 1));

    // 0 字节 = 建头没写完的残骸（唯一的残骸判据）：补写成当前身份，不报损坏
    std::fs::write(&p, b"").unwrap();
    assert_eq!(std::fs::metadata(&p).unwrap().len(), 0, "an empty file is the one shape that may be rewritten");
    drop(Journal::open(&p, id).unwrap());
    assert_eq!(Journal::read_identity(&p).unwrap().fp.ino, 42, "the remnant must carry the current identity");
    assert!(matches!(Journal::read_entries(&p).unwrap(), JournalRead::Complete(v) if v.is_empty()));

    // 1..HEADER_LEN-1 字节**不补头**：它可能是"初始化没写完"，也可能是"原 journal 被外部
    // 截断到这么短"（记录因此已经丢了），两者在内容上不可区分。三个消费点都按"读不出来"
    // 处理，且一字不动——按残骸补头就等于替用户宣布"这里什么都没有"
    for short in [
        &b"DEJ"[..],                                   // 只写了 magic 的前 3 字节
        &full[..Journal::HEADER_LEN - 1],               // 差最后 1 字节的头
        &full[..Journal::MAGIC.len() + 24],             // 身份段写了 3 个字段、映射位还没写完
    ] {
        std::fs::write(&p, short).unwrap();
        assert!(err_text(Journal::read_entries(&p)).contains("truncated"), "{} bytes", short.len());
        assert!(err_text(Journal::open(&p, id)).contains("truncated"), "{} bytes", short.len());
        assert_eq!(std::fs::read(&p).unwrap(), short, "a sub-header file must be left byte-identical");
    }
    // 非本工具的半截文件不得被当成残骸改写
    std::fs::write(&p, b"XYZ").unwrap();
    assert!(Journal::open(&p, id).is_err());
    assert_eq!(std::fs::read(&p).unwrap(), b"XYZ", "a foreign short file must be left alone");

    let _ = std::fs::remove_file(&p);
}

/// 记录判据的边界：一份**含记录的完整 journal 被外部截断到 0** 后，与"建好文件、头还没
/// 写"在现有状态下不可区分，判据把它当残骸收养——记录就此丢失且无从提示。这是**已接受
/// 的限制而非安全保证**；1 字节以上的截断走的是拒绝路径（见上一条测试），唯独 0 字节
/// 例外。若产品日后要求"无法证明来源就不重新初始化"，改的是 `Journal::open` 的 0 字节
/// 分支与本测试，不是恢复流程
#[test]
pub(crate) fn journal_truncated_to_zero_is_adopted_as_a_remnant() {
    let p = journal_path("trunc_zero");
    let id = LiveScene { fp: TargetFingerprint { dev: 7, ino: 42, size: 1 << 20, id: 0 }, map: None, proof: LiveProof::Inode };
    {
        let mut j = Journal::open(&p, id).unwrap();
        j.record(0, &[0xAA; 8], &[0xBB; 8]).unwrap();
    }
    assert!(Journal::read_entries(&p).is_ok(), "the journal must be complete before truncation");

    std::fs::write(&p, b"").unwrap();
    assert_eq!(std::fs::metadata(&p).unwrap().len(), 0, "an externally truncated empty file fits the remnant criterion");
    drop(Journal::open(&p, id).unwrap());
    assert!(matches!(Journal::read_entries(&p).unwrap(), JournalRead::Complete(v) if v.is_empty()),
        "the lost records are gone silently — that is the accepted limitation");

    let _ = std::fs::remove_file(&p);
}

/// open 与首次 record 之间存在窗口：此刻冒出来的文件不是我们建的，必须拒绝而不是
/// 把 MAGIC 追加进去（两方记录交错进同一份 journal，一次 undo 会回放出不属于本次的字节）。
/// 抢建的文件还要原样保留——“拒绝”不包括把它截断
#[test]
pub(crate) fn journal_creation_refuses_a_file_that_appeared_late() {
    let p = journal_path("toctou");
    let mut j = Journal::open(&p, jid()).unwrap();
    assert!(!p.exists(), "opening a journal must not leave a trace on disk");

    std::fs::write(&p, b"someone else's file").unwrap();
    assert!(j.record(0, &[0xAA; 4], &[0xBB; 4]).is_err(), "a file that appeared after open must not be adopted");
    assert_eq!(std::fs::read(&p).unwrap(), b"someone else's file", "the intruding file must be left untouched");

    let _ = std::fs::remove_file(&p);
}

/// 单句柄：采纳一份带撕裂尾的既有 journal，截断与之后的追加落在同一个句柄上。
/// 句柄不再是 append 模式，追加位置由 `seek(End)` 显式给出——不 seek 就会把新记录
/// 写在旧 EOF 之后（截断留下的空洞），读回来即 CRC 失败。
/// 被截掉的那段字节在 journal 里不再存在，故截断前必须先归档，且逐字节保真
#[test]
pub(crate) fn journal_adopts_truncates_and_appends_through_one_handle() {
    let p = journal_path("onehandle");
    // 归档名是顺延的：上一次跑留下的归档会让这一次落到 .1，先清干净再断言
    let archived = suffix_path(&p, ".torn-tail");
    let _ = std::fs::remove_file(&archived);
    let _ = std::fs::remove_file(suffix_path(&p, ".torn-tail.1"));
    const DATA: usize = 32;
    {
        let mut j = Journal::open(&p, jid()).unwrap();
        for i in 0..2u64 {
            j.record(i * 512, &[0xAA; DATA], &[0xBB; DATA]).unwrap();
        }
    }
    let full = std::fs::read(&p).unwrap();
    // 撕掉最后一条记录的尾部：模拟一条没走完的追加
    std::fs::write(&p, &full[..full.len() - 7]).unwrap();

    let (mut j, cut) = Journal::adopt(&p, jid()).unwrap();
    let cut = cut.expect("the torn record must be reported as a non-zero cut");
    assert!(
        matches!(Journal::read_entries(&p).unwrap(), JournalRead::Complete(v) if v.len() == 1),
        "adoption must physically cut the torn tail"
    );
    // 落盘的是被截掉的那段：从最后一条完整记录之后，到本次写文件时的 EOF
    let torn_end = full.len() - 7;
    let got = std::fs::read(&archived).unwrap();
    assert_eq!(got.len(), cut.get(), "archive length ({}) vs cut ({})", got.len(), cut);
    assert_eq!(got, &full[torn_end - cut.get()..torn_end], "the discarded bytes must be archived byte-for-byte before the journal is truncated");
    j.record(4096, &[0xCC; DATA], &[0xDD; DATA]).unwrap();
    drop(j);
    assert!(
        matches!(Journal::read_entries(&p).unwrap(), JournalRead::Complete(v) if v.len() == 2),
        "the append after adoption must land directly after the surviving record"
    );

    let _ = std::fs::remove_file(&p);
    let _ = std::fs::remove_file(&archived);
}

/// 身份取自已打开的句柄：符号链接只是别名，身份主体落在真实路径的兄弟文件上；
/// 打开之后路径被改名/替换时必须拒绝——fd 仍指向旧 inode，而字面路径已属于别人，
/// 按字面路径派生伴随文件会把 journal 放到另一份数据旁
#[cfg(target_os = "linux")]
#[test]
pub(crate) fn image_identity_follows_the_opened_handle_not_the_path() {
    let dir = std::env::temp_dir().join(format!("diskedit_idfd_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let a = dir.join("a.img");
    let link = dir.join("link.img");
    std::fs::write(&a, vec![0u8; 4096]).unwrap();
    std::os::unix::fs::symlink(&a, &link).unwrap();

    let f = std::fs::OpenOptions::new().read(true).write(true).open(&link).unwrap();
    let meta = f.metadata().unwrap();
    let id = FileSource::identity_of_opened_image(&link, &f, &meta).unwrap();
    let canonical = std::fs::canonicalize(&a).unwrap();
    assert_eq!(
        id.journal_path(),
        suffix_path(&canonical, JOURNAL_SUFFIX),
        "the identity subject must be the opened file, not the spelling used to open it"
    );
    drop(f);

    // 打开后把文件改名，并在原路径放一个新文件：fd 仍指向旧 inode，字面路径已换人
    let f = std::fs::OpenOptions::new().read(true).write(true).open(&a).unwrap();
    let meta = f.metadata().unwrap();
    std::fs::rename(&a, dir.join("moved.img")).unwrap();
    std::fs::write(&a, vec![0u8; 4096]).unwrap();
    let e = FileSource::identity_of_opened_image(&a, &f, &meta).unwrap_err();
    assert!(e.to_string().contains("no longer names the file that was opened"), "{e}");

    let _ = std::fs::remove_dir_all(&dir);
}

/// 日志落点也由身份推导：镜像与目标同层级，块设备落在 state_dir 下且以 Disk GUID /
/// devname 命名。模块自行拼 state_dir() 会让落点随调用方漂移。
/// 块设备侧的推导断言只在非 Linux 跑：Linux 上 /dev/sdz 的拓扑解析不出（fail-closed），
/// 身份构造本身会拒绝，不存在"退化身份"可用来测命名
#[test]
pub(crate) fn log_path_comes_from_the_identity() {
    let img = PathBuf::from("/tmp/disk.img");
    let id = TargetIdentity::resolve_image(&img);
    assert_eq!(id.log_path(None), suffix_path(&img, LOG_SUFFIX));

    #[cfg(not(target_os = "linux"))]
    {
        let dev = PathBuf::from("/dev/sdz");
        let id = TargetIdentity::resolve_block(&dev).unwrap();
        let guid = [0xABu8; 16];
        assert_eq!(id.log_path(Some(guid)), state_dir().join(format!("{}{LOG_SUFFIX}", guid_hex(&guid))));
        // 读不到表时退回 devname：MBR / 裸盘上没有更稳的标识
        assert_eq!(id.log_path(None), state_dir().join(format!("sdz{LOG_SUFFIX}")));
    }
}

/// 落盘命名的契约锚点：命名空间的字面量只在这一处出现，四类伴随文件各自的后缀
/// 由它派生校验——改命名空间或改某一类后缀，这里该失败
#[test]
pub(crate) fn artifact_names_are_pinned_as_documented() {
    let ns = JOURNAL_SUFFIX.strip_suffix("journal").expect("journal suffix ends with its stem");
    assert_eq!(ns, ".diskedit.");
    assert_eq!(CHECKPOINT_SUFFIX, format!("{ns}ckpt"));
    assert_eq!(LOCK_SUFFIX, format!("{ns}lock"));
    assert_eq!(LOG_SUFFIX, format!("{ns}log"));
    assert_eq!(CKPT_TMP_PREFIX, format!("{ns}ckpt.tmp."));

    // 镜像：四类伴随文件都是目标路径的同目录兄弟
    let img = PathBuf::from("/tmp/x.img");
    let id = TargetIdentity::resolve_image(&img);
    assert_eq!(id.journal_candidates()[0], suffix_path(&img, JOURNAL_SUFFIX));
    assert_eq!(id.checkpoint_candidates(None).paths[0], suffix_path(&img, CHECKPOINT_SUFFIX));
    assert_eq!(id.lock_path(), suffix_path(&img, LOCK_SUFFIX));
    assert_eq!(id.log_path(None), suffix_path(&img, LOG_SUFFIX));
}

/// 中段某帧的 len 被写坏、其后仍有完整记录：这是损坏而不是撕裂尾。按撕裂处理会让
/// `Journal::open` 物理截断，把后续每条已通过 CRC、写入确实发生过的原文永久删掉
#[test]
pub(crate) fn a_corrupt_record_length_ahead_of_valid_records_is_damage() {
    let p = journal_path("midlen");
    let mut j = Journal::open(&p, jid()).unwrap();
    j.record(0, &[0xAA; 4], &[0xBB; 4]).unwrap();
    j.record(8, &[0xCC; 4], &[0xDD; 4]).unwrap();
    drop(j);

    let mut data = std::fs::read(&p).unwrap();
    data[Journal::HEADER_LEN..Journal::HEADER_LEN + 4].copy_from_slice(&0xFF_FFFFu32.to_le_bytes());
    std::fs::write(&p, &data).unwrap();

    let Err(e) = Journal::read_entries(&p) else {
        panic!("a length that runs past its successors must be reported as damage");
    };
    assert!(e.to_string().contains("records follow it"), "unexpected error: {e}");
    assert!(Journal::open(&p, jid()).is_err(), "open must refuse a damaged record instead of truncating it");
    assert_eq!(std::fs::read(&p).unwrap(), data, "open must not have truncated the file");
    let _ = std::fs::remove_file(&p);
}

/// 尾部真的只写了一半（其后没有任何完整帧）仍按撕裂尾处理：读出的前缀完整，
/// 采纳时把文件截到最后一帧之后
#[test]
pub(crate) fn a_torn_tail_after_the_last_record_is_truncated() {
    let p = journal_path("torn");
    let mut j = Journal::open(&p, jid()).unwrap();
    j.record(0, &[0xAA; 4], &[0xBB; 4]).unwrap();
    drop(j);

    let mut data = std::fs::read(&p).unwrap();
    let end = data.len();
    data.extend_from_slice(&[0x10, 0x00]);
    std::fs::write(&p, &data).unwrap();

    assert!(
        matches!(
            Journal::read_entries(&p).unwrap(),
            JournalRead::TruncatedTail { records, cut } if records.len() == 1 && cut.get() == 2
        ),
        "a half-written record head at the end is a torn tail"
    );
    drop(Journal::open(&p, jid()).unwrap());
    assert_eq!(
        std::fs::read(&p).unwrap().len(),
        end,
        "the adopted journal must be cut back to its last complete record"
    );
    let _ = std::fs::remove_file(&p);
}

/// 设备 ID 摘要用标准 FNV-1a 64 向量钉住：换实现或换常量都会让已落盘现场里的 ID 与同一块盘
/// 新算出的值不再相等，恢复现场随之被误判为"换了盘"
#[cfg(target_os = "linux")]
#[test]
pub(crate) fn fnv1a64_matches_the_reference_vectors() {
    assert_eq!(fnv1a64(b""), 0xcbf2_9ce4_8422_2325);
    assert_eq!(fnv1a64(b"a"), 0xaf63_dc4c_8601_ec8c);
    assert_eq!(fnv1a64(b"foobar"), 0x8594_4171_f739_67e8);
}

/// 非 loop 块设备的身份是**盘级**的：journal / checkpoint / lock 按 disk_key 落点。
/// 断言用 state_dir() 自身拼期望值——测试不改环境变量，进程内并行测试共享环境
#[test]
pub(crate) fn block_identity_keys_on_the_whole_disk() {
    let id = TargetIdentity::from_block_keys(Path::new("/dev/sdb1"), "sdb-1234".to_string());
    let disk_tok = key_token("sdb-1234");
    assert_eq!(id.journal[0], state_dir().join(format!("{disk_tok}{JOURNAL_SUFFIX}")));
    assert_eq!(id.journal.len(), 1);
    assert_eq!(id.checkpoint[0], state_dir().join(format!("{disk_tok}{CHECKPOINT_SUFFIX}")));
    assert_eq!(id.checkpoint.len(), 1);
    assert_eq!(id.lock_path(), state_dir().join(format!("{disk_tok}{LOCK_SUFFIX}")));
}

/// loop 设备的身份绑定 backing **对象**：重新打开 backing、用已打开句柄解析名字，现场落
/// 该名字的兄弟文件与其 state_dir 落点。
/// backing 消失后身份不再可解析（名字已经不是对象），此处拒绝而非把现场挂到同名路径旁
///
/// 比对的一侧来自内核给这个 loop 记下的 backing 对象，所以这里必须有一个真绑定：
/// 绑不上（无空闲设备或没有权限，CI 的 runner 用户即如此）就没有可造出的现场，跳过
#[cfg(target_os = "linux")]
#[test]
pub(crate) fn loop_identity_converges_to_the_backing_file() {
    struct Bound {
        dev: PathBuf,
        dir: PathBuf,
    }
    impl Drop for Bound {
        fn drop(&mut self) {
            let _ = std::process::Command::new("losetup").arg("-d").arg(&self.dev).output();
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    let dir = std::env::temp_dir().join(format!("diskedit_loop_id_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let backing = dir.join("backing.img");
    std::fs::write(&backing, vec![0u8; 1 << 20]).unwrap();
    let out = match std::process::Command::new("losetup")
        .args(["--find", "--show"])
        .arg(&backing)
        .output()
    {
        Ok(o) if o.status.success() => o,
        _ => return,
    };
    let dev = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim());
    let _cleanup = Bound { dev: dev.clone(), dir: dir.clone() };

    let id = TargetIdentity::loop_backed(&dev, backing.clone()).expect("a bound backing file resolves");
    assert_eq!(id.journal, vec![suffix_path(&backing, JOURNAL_SUFFIX)]);
    assert_eq!(id.checkpoint, vec![suffix_path(&backing, CHECKPOINT_SUFFIX)]);
    assert_eq!(id.lock_path(), suffix_path(&backing, LOCK_SUFFIX));

    // backing 已删：身份不再可解析，不退到 sysfs 字符串
    std::fs::remove_file(&backing).unwrap();
    assert!(TargetIdentity::loop_backed(&dev, backing.clone()).is_err());
}

/// 镜像身份以 realpath 为主体：经符号链接打开与直接打开收敛到同一现场；
/// 日志命名沿用字面路径，不因 canonicalize 打断连续性
#[cfg(target_os = "linux")]
#[test]
pub(crate) fn image_identity_prefers_the_realpath() {
    let dir = std::env::temp_dir().join(format!("diskedit_img_id_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let real = dir.join("real.img");
    std::fs::write(&real, b"x").unwrap();
    let link = dir.join("link.img");
    std::os::unix::fs::symlink(&real, &link).unwrap();

    let via_link = TargetIdentity::resolve_image(&link);
    let direct = TargetIdentity::resolve_image(&real);
    assert_eq!(via_link.journal, direct.journal, "both spellings must share one journal");
    assert_eq!(via_link.checkpoint, direct.checkpoint);
    assert_eq!(via_link.lock_path(), direct.lock_path());
    assert_eq!(via_link.log_path(None), suffix_path(&link, LOG_SUFFIX));

    let _ = std::fs::remove_dir_all(&dir);
}

/// 可写打开清掉落点目录里的陈旧 ckpt 暂存文件：只动本目标的、且只在过期之后；
/// 新鲜那份（可能属于正在写的另一个进程）与别的目标的残骸都不碰
#[test]
pub(crate) fn stale_ckpt_temps_are_swept_on_open() {
    let dir = std::env::temp_dir().join(format!("diskedit_sweep_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let img = dir.join("x.img");
    std::fs::write(&img, b"x").unwrap();
    // Linux 上身份取 realpath，落点目录未必是上面那个字面目录：以身份为准
    let id = TargetIdentity::resolve_image(&img);
    let ckpt = id.checkpoint_candidates(None).paths[0].clone();
    let dir = ckpt.parent().unwrap().to_path_buf();

    let prefix = ckpt_temp_prefix(&ckpt);
    let stale = dir.join(format!("{prefix}1.1"));
    let fresh = dir.join(format!("{prefix}2.1"));
    // 同目录下另一个目标的残骸：名字只差 token，清扫不得认领
    let foreign_ckpt = dir.join("other.img.diskedit.ckpt");
    let foreign = dir.join(format!("{}1.1", ckpt_temp_prefix(&foreign_ckpt)));
    let other = dir.join("not-ours.img");
    std::fs::File::create(&stale).unwrap()
        .set_modified(std::time::SystemTime::now() - CKPT_TMP_STALE_AFTER * 2).unwrap();
    std::fs::copy(&stale, &foreign).unwrap();
    std::fs::File::create(&fresh).unwrap();
    std::fs::write(&other, b"x").unwrap();

    sweep_stale_ckpt_temps(&id);
    assert!(!stale.exists(), "a stale temp of this target must be swept");
    assert!(fresh.exists(), "a fresh temp may belong to a running writer");
    assert!(foreign.exists(), "another target's stale temp is not ours to sweep");
    assert!(other.exists(), "only the checkpoint temp prefix is ours to sweep");

    let _ = std::fs::remove_file(&fresh);
    let _ = std::fs::remove_file(&foreign);
    let _ = std::fs::remove_file(&other);
    let _ = std::fs::remove_file(&img);
    let _ = std::fs::remove_dir_all(&dir);
}

/// fail-closed：sysfs 拓扑解析不出来 ⇒ 拒绝（错误信息带设备名），不退到
/// "devname-容量"的调用方身份——那个退化会让 /dev/sdb 与 /dev/sdb1 得到两把锁。
/// 非 Linux 平台没有 sysfs 概念，`resolve_block` 本就不走该判据
#[cfg(target_os = "linux")]
#[test]
pub(crate) fn block_identity_refuses_unresolvable_topology() {
    // 普通文件被当作 is_block 传入：rdev=0 ⇒ /sys/dev/block/0:0 不存在 ⇒ 拒绝
    let fake = std::env::temp_dir().join(format!("diskedit_notablock_{}", std::process::id()));
    std::fs::write(&fake, b"x").unwrap();
    let e = TargetIdentity::resolve_block(&fake).expect_err("an unresolvable topology must be refused");
    assert!(e.contains("fake") || e.contains("diskedit_notablock"), "the error must name the device: {e}");
    assert!(e.contains("topology"), "{e}");
    let _ = std::fs::remove_file(&fake);
}

/// [`read_sysfs_attr`] 的三态必须可分（上条测试的细粒度锁）："不存在 ⇒ None"
/// 是整设备的正常路径，"读失败 ⇒ Err" 则不得降级——两者曾被 `.ok()` 压成同一个
/// None，分区身份会在读不出来的那一刻静默变成整设备身份
#[cfg(target_os = "linux")]
#[test]
pub(crate) fn sysfs_attr_distinguishes_missing_from_read_failure() {
    assert_eq!(
        read_sysfs_attr(Path::new("/nonexistent/diskedit/attr")).unwrap(),
        None,
        "不存在 ⇒ None"
    );
    let tmp = std::env::temp_dir().join(format!("diskedit_attr_{}", std::process::id()));
    std::fs::write(&tmp, "7\n").unwrap();
    assert_eq!(read_sysfs_attr(&tmp).unwrap().as_deref(), Some("7"));
    // 存在但值空：与"不存在"分开（空的 partition 属性是分区侧的异常，不该走整设备分支）
    std::fs::write(&tmp, " \n").unwrap();
    assert_eq!(read_sysfs_attr(&tmp).unwrap().as_deref(), Some(""));
    let _ = std::fs::remove_file(&tmp);
    // 读目录：失败且 errno 非 ENOENT ⇒ 必须上抛，不得折进"不存在"
    let e = read_sysfs_attr(&std::env::temp_dir())
        .expect_err("reading a directory must not count as 'attribute missing'");
    assert_ne!(e.kind(), io::ErrorKind::NotFound, "{e}");
}

/// 锁按**盘**派生、现场按**目标**派生：两者在块设备上是同一个函数算出来的两个投影，
/// 因此锁落点必然落在 state_dir 下、与 journal 同目录（不引入第二套路径规则）。
/// 非 Linux 跑退化身份（无 sysfs 判据）；Linux 上身份只能来自真实拓扑
/// （见 `block_identity_refuses_unresolvable_topology`）
#[cfg(not(target_os = "linux"))]
#[test]
pub(crate) fn block_lock_and_journal_share_one_directory() {
    let id = TargetIdentity::resolve_block(Path::new("/dev/diskedit-nonexistent")).unwrap();
    assert_eq!(id.lock_path().parent(), Some(state_dir().as_path()));
    assert_eq!(id.journal_path().parent(), Some(state_dir().as_path()));
    assert!(id.lock_path().to_string_lossy().ends_with(LOCK_SUFFIX));
}
