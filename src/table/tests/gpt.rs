use super::*;
use super::support::*;
use gptman::GPT;

/// 重叠条目：诊断层必须仍能读出（info 要能指出哪两条重叠、用户据此修表），
/// 而写入层必须拒绝——本工具的空间派生事实（右侧空闲、搬移打包、扩容终点）全部
/// 以"不重叠"为前提（UEFI 2.10 §5.3.1 GPT overview：Each defined partition must not
/// overlap with any other defined partition）。拒绝发生在任何写盘之前，故归 infra
#[test]
pub(crate) fn overlapping_entries_are_diagnosable_but_never_writable() {
    let ss = 512u64;
    let mut src = src_from("overlap", vec![0u8; (2048 * ss) as usize]);
    // 150..250 落在 100..200 内：解析层放行（诊断要看得到），写入层拒绝
    write_primary_entries(&mut src, ss, &[(100, 200), (150, 250), (400, 500)]);
    ensure_protective_mbr(&mut src, ss).unwrap();
    let g = load_gpt(&src)
        .unwrap()
        .expect("a table with overlapping entries must stay observable for diagnostics");
    assert_eq!(crate::geometry::find_overlap(&g.entries), Some((1, 2)));
    let e = crate::gpt_policy::resolve_geometry(&src)
        .err()
        .expect("an overlapping table must be refused by the write-path geometry");
    assert!(matches!(&e, Fail::Infra(m) if m.contains("overlap")), "{e:?}");

    // 紧邻但不重叠（200 / 201）：同一构造点必须放行，不得把合法表一并拒掉
    let mut ok = src_from("no_overlap", vec![0u8; (2048 * ss) as usize]);
    write_primary_entries(&mut ok, ss, &[(100, 200), (201, 250), (400, 500)]);
    ensure_protective_mbr(&mut ok, ss).unwrap();
    assert!(crate::gpt_policy::resolve_geometry(&ok).is_ok());
}

/// 原始条目末端越出可用区：内核遇到这种表会先告警、再按盘尾把长度截断，
/// 因此 sysfs 里的长度可能"看起来正常"而盘上的表项并没有。几何与实际写路径只认
/// **盘上的字节**，解析层必须拒绝，被截断的长度不得成为后续算术的基线
#[test]
pub(crate) fn entry_past_last_usable_is_refused() {
    let ss = 512u64;
    let mut src = src_from("entry_past_usable", vec![0u8; (2048 * ss) as usize]);
    // 头自述的可用区由容器算出（2048 扇区的 last_usable 远小于 5000）
    write_primary_entries(&mut src, ss, &[(100, 5000)]);
    ensure_protective_mbr(&mut src, ss).unwrap();
    let e = crate::gpt_policy::resolve_geometry(&src).err().expect("an entry past the usable range must be refused");
    assert!(matches!(&e, Fail::Infra(m) if m.contains("outside usable range")), "{e:?}");
}

/// 容器装不下条目数组时，commit_gpt 必须用 checked 运算返回 Err，
/// 而不能让 last_lba − span 这类减法下溢（debug 下 panic、release 下回绕）
#[test]
pub(crate) fn commit_gpt_refuses_container_too_small() {
    let mut src = src_from_gpt("tiny", 512);
    let g = load_gpt(&src).unwrap().unwrap();
    let e = commit_gpt(&mut src, &g, 0).unwrap_err();
    assert!(e.to_string().contains("too small to hold GPT entry array"), "{e}");
}

/// HeaderSize 是规范字段而非固定 92：读到的值必须随头带回并按原值重写，
/// CRC 覆盖 HeaderSize 字节（含 92 之后的保留区）。写入端固定 92 的实现会把
/// 合法的更大 HeaderSize 静默规范化掉——读改写不再是恒等变换
#[test]
pub(crate) fn header_size_is_preserved_across_rewrite() {
    for (hdr_size, reserved) in [(92u32, false), (96, true), (512, false)] {
        let mut src = patch_primary_header(src_from_gpt(&format!("hsz{hdr_size}"), 512), hdr_size, reserved);
        let g = load_gpt(&src).unwrap().unwrap();
        assert_eq!(g.header.header_size, hdr_size, "parse must carry HeaderSize");
        let last_lba = src.size / 512 - 1;
        commit_gpt(&mut src, &g, last_lba).unwrap();
        let g2 = load_gpt(&src).unwrap().unwrap();
        assert_eq!(g2.header.header_size, hdr_size, "rewrite must keep HeaderSize");
        // 盘上字节自证：字段等于原值，且 CRC 恰覆盖该长度
        let mut sec = vec![0u8; 512];
        src.read_at(512, &mut sec).unwrap();
        assert_eq!(u32::from_le_bytes(sec[12..16].try_into().unwrap()), hdr_size);
        let stored = u32::from_le_bytes(sec[16..20].try_into().unwrap());
        sec[16..20].fill(0);
        assert_eq!(crc32(&sec[..hdr_size as usize]), stored);
    }
}

/// 头部自述的 HeaderSize 越出容器/小于规范下限时必须返回错误，
/// 不得按该值切片（越界 panic）或写出一张 CRC 语义不明的头
#[test]
pub(crate) fn header_size_out_of_range_is_an_error() {
    let mut src = src_from_gpt("hszbad", 512);
    let g = load_gpt(&src).unwrap().unwrap();
    let last_lba = src.size / 512 - 1;
    for bad in [0u32, 91, 513, u32::MAX] {
        let mut g = g.clone();
        g.header.header_size = bad;
        let e = commit_gpt(&mut src, &g, last_lba).unwrap_err();
        assert!(e.to_string().contains("HeaderSize"), "{bad}: {e}");
    }
}

/// 主副本视角：条目数组必须夹在头部与可用区之间（[2, 2+span) ⊆ 可用区之前）。
/// 越界的数组会让"分区数据"与"分区表"共用同一段 LBA，读出来的条目是别的字节
#[test]
pub(crate) fn geometry_primary_requires_entry_array_before_usable_range() {
    let last = 999;
    let ok = validate_geometry(&geo_header(2 + SPAN_128_512, 900, 2), &geom128(512), last, GptCopyKind::Primary);
    assert!(ok.is_ok(), "spec-shaped table must pass: {ok:?}");
    // 数组起点压住 LBA0/LBA1
    assert!(matches!(
        validate_geometry(&geo_header(34, 900, 1), &geom128(512), last, GptCopyKind::Primary),
        Err(GptError::InvalidHeader(_))
    ));
    // 起点合规但数组上界越过 FirstUsableLBA（比"起点 < 2"隐蔽）
    assert!(matches!(
        validate_geometry(&geo_header(34, 900, 4), &geom128(512), last, GptCopyKind::Primary),
        Err(GptError::InvalidHeader(_))
    ));
    // FirstUsableLBA 装不下数组：first_usable < 2 + span
    assert!(matches!(
        validate_geometry(&geo_header(1, 900, 2), &geom128(512), last, GptCopyKind::Primary),
        Err(GptError::InvalidHeader(_))
    ));
    // 极端下界：拒绝而非回绕/越界
    assert!(matches!(
        validate_geometry(&geo_header(0, 900, 2), &geom128(512), last, GptCopyKind::Primary),
        Err(GptError::InvalidHeader(_))
    ));
}

/// 备份副本视角：数组在可用区**之后**、备份头之前。主备两条约束不可混用——
/// 备份数组本来就不在可用区之前，套用主视角会误拒所有合法盘
#[test]
pub(crate) fn geometry_backup_requires_entry_array_after_usable_range() {
    let file_last = 999;
    // 规范形状：数组 [967, 999)，可用区上界 966
    let legal = geo_header(2 + SPAN_128_512, file_last - SPAN_128_512 - 1, file_last - SPAN_128_512);
    assert!(validate_geometry(&legal, &geom128(512), file_last, GptCopyKind::Backup).is_ok());
    // 同一份头按主副本视角必须被拒：证明两视角确实是两套约束
    assert!(validate_geometry(&legal, &geom128(512), file_last, GptCopyKind::Primary).is_err());
    // 数组落进可用区
    assert!(validate_geometry(&geo_header(34, 966, 966), &geom128(512), file_last, GptCopyKind::Backup).is_err());
    // 数组越过备份头（越过盘尾）
    assert!(validate_geometry(&geo_header(34, 966, 968), &geom128(512), file_last, GptCopyKind::Backup).is_err());
    // 起点仍须在 LBA0/LBA1 之后
    assert!(validate_geometry(&geo_header(34, 966, 1), &geom128(512), file_last, GptCopyKind::Backup).is_err());
    // FirstUsableLBA 装不下数组：表级界与主视角同判，备份视角不得漏
    assert!(matches!(
        validate_geometry(&geo_header(1, 966, file_last - SPAN_128_512), &geom128(512), file_last, GptCopyKind::Backup),
        Err(GptError::InvalidHeader(_))
    ));
    assert!(matches!(
        validate_geometry(&geo_header(0, 966, file_last - SPAN_128_512), &geom128(512), file_last, GptCopyKind::Backup),
        Err(GptError::InvalidHeader(_))
    ));
}

/// 表级不变量：FirstUsableLBA 的下界与"解析的是哪份副本"无关。
/// 同一份 first_usable_lba 过低的头，主/备两个视角必须同判，否则备份视角会成为
/// 绕过界面的入口（主副本损坏 → 回退备份副本 → 下游按备份自述的下界放行条目）
#[test]
pub(crate) fn geometry_first_usable_floor_is_view_independent() {
    let file_last = 999;
    // 备份数组落位合规（可用区之后、备份头之前），单独把 first_usable_lba 压到界下
    let h = geo_header(2 + SPAN_128_512 - 1, file_last - SPAN_128_512 - 1, file_last - SPAN_128_512);
    assert!(matches!(validate_geometry(&h, &geom128(512), file_last, GptCopyKind::Primary), Err(GptError::InvalidHeader(_))));
    assert!(matches!(validate_geometry(&h, &geom128(512), file_last, GptCopyKind::Backup), Err(GptError::InvalidHeader(_))));
}

/// 表级不变量：LastUsableLBA 的上界（须位于备份条目数组之前）同样与副本无关。
/// 越界值一旦判为 Valid，下游 add/grow 便以它为可信上界把分区划进 backup GPT
/// 元数据区，随后被 commit_table 覆盖
#[test]
pub(crate) fn geometry_last_usable_ceiling_is_view_independent() {
    let file_last = 999;
    let span = SPAN_128_512;
    // 备份数组落位合规，单独把 last_usable_lba 压进数组区（= file_last − span）
    let h = geo_header(2 + span, file_last - span, file_last - span);
    assert!(matches!(validate_geometry(&h, &geom128(512), file_last, GptCopyKind::Primary), Err(GptError::InvalidHeader(_))));
    assert!(matches!(validate_geometry(&h, &geom128(512), file_last, GptCopyKind::Backup), Err(GptError::InvalidHeader(_))));
    // 界外一扇区（file_last − span − 1）恰好合规
    let ok = geo_header(2 + span, file_last - span - 1, 2);
    assert!(validate_geometry(&ok, &geom128(512), file_last, GptCopyKind::Primary).is_ok());
}

/// 头字段判据的拒绝分支：primary_lba、区间倒挂、越容器、数组跨度溢出逐一覆盖
#[test]
pub(crate) fn geometry_rejects_malformed_header_fields() {
    let g = geom128(512);
    // primary_lba != 1：UEFI 2.10 §5.3.1 固定主头在 LBA1
    let mut h = geo_header(34, 900, 2);
    h.primary_lba = 2;
    assert!(matches!(validate_geometry(&h, &g, 999, GptCopyKind::Primary), Err(GptError::InvalidHeader(_))));
    // 区间倒挂
    let h = geo_header(900, 34, 2);
    assert!(matches!(validate_geometry(&h, &g, 999, GptCopyKind::Primary), Err(GptError::InvalidHeader(_))));
    // last_usable_lba 越容器
    let h = geo_header(34, 1000, 2);
    assert!(matches!(
        validate_geometry(&h, &g, 999, GptCopyKind::Primary),
        Err(GptError::BeyondContainer { field: "last_usable_lba", .. })
    ));
    // backup_lba 越容器
    let mut h = geo_header(34, 900, 2);
    h.backup_lba = 1000;
    assert!(matches!(
        validate_geometry(&h, &g, 999, GptCopyKind::Primary),
        Err(GptError::BeyondContainer { field: "backup_lba", .. })
    ));
    // 数组跨度溢出：checked_add 必须拒绝而非回绕（起点贴着 u64 上界）
    let h = geo_header(34, 900, u64::MAX - 30);
    assert!(matches!(validate_geometry(&h, &g, 999, GptCopyKind::Primary), Err(GptError::InvalidHeader(_))));
}

/// 端到端：主副本自述 first_usable_lba = 1（装不下条目数组）时必须被拒，
/// 且不得因"主副本坏"就静默交由别处掩盖
#[test]
pub(crate) fn geometry_invariant_reaches_load_gpt() {
    let ss = 512u64;
    let mut src = src_from_gpt("geo_e2e", ss);
    let file_last = src.size / ss - 1;
    // 抹掉盘尾备份头：本用例只考主副本的判定
    src.write_at(file_last * ss, &vec![0u8; ss as usize]).unwrap();
    let mut h = geo_header(1, 66, 2);
    h.backup_lba = file_last;
    write_raw_primary(&mut src, h, ss);
    assert!(matches!(load_gpt(&src), Err(GptError::InvalidHeader(_))));
}

/// 端到端：主副本读不出时回退备份副本，备份头若自述 first_usable_lba = 0 必须被拒。
/// 该字段随后被写入路径当作权威下界，漏判会放行 starting_lba=0 的条目——
/// 覆盖保护 MBR 与主头。这是"表级不变量必须主/备同判"的端到端锚点
#[test]
pub(crate) fn backup_first_usable_floor_reaches_load_gpt() {
    let ss = 512u64;
    let mut src = src_from_gpt("geo_bak", ss);
    let file_last = src.size / ss - 1;
    // 抹掉主头：本轮只考备份副本的判定
    src.write_at(ss, &vec![0u8; ss as usize]).unwrap();
    let entry_lba = file_last - SPAN_128_512;
    let mut h = geo_header(0, entry_lba - 1, entry_lba);
    h.primary_lba = file_last; // 备份头自述 MyLBA = 盘尾
    h.backup_lba = 1; // 备头 AltLBA = 1
    write_raw_backup(&mut src, h, ss);
    assert!(
        matches!(load_gpt(&src), Err(GptError::InvalidHeader(_))),
        "备份副本的 first_usable_lba 下界不得被漏判"
    );
}

#[test]
pub(crate) fn entry_size_rules() {
    // UEFI 2.10 §5.3.3 Table 5.6：SizeOfPartitionEntry = 128 × 2^n；192/136 等非法。
    // 判据的唯一实现在 geometry（数组几何的构造点），此处直接消费它
    assert!(entry_size_ok(128));
    assert!(entry_size_ok(256));
    assert!(entry_size_ok(1024));
    assert!(!entry_size_ok(192));
    assert!(!entry_size_ok(136));
    assert!(!entry_size_ok(64));
    // es=256 序列化：前 128 字节为条目，尾部保留区为零，CRC 覆盖整个 n×es
    let e = GPTPartitionEntry {
        partition_type_guid: ESP_TYPE_GUID,
        unique_partition_guid: [0x21; 16],
        starting_lba: 34,
        ending_lba: 100,
        attribute_bits: 0,
        partition_name: "p".into(),
    };
    let (b, crc) = serialize_array(&[e], &EntryArrayGeometry::new(512, 256, 1, MAX_ARRAY_BYTES).unwrap());
    assert_eq!(b.len(), 512);
    assert_eq!(b[128..256], [0u8; 128], "reserved tail must stay zero");
    assert_eq!(crc, crate::table::crc32(&b[..256]));
    // 192 违规：几何构造点即拒绝
    assert!(EntryArrayGeometry::new(512, 192, 1, MAX_ARRAY_BYTES).is_err());
}

#[test]
pub(crate) fn gpt_roundtrip_512() {
    let src = src_from_gpt("g512", 512);
    let g = load_gpt(&src).unwrap().expect("gpt should parse");
    assert_eq!(g.header.number_of_partition_entries, 128);
    assert_eq!(g.header.size_of_partition_entry, 128);
    let used: Vec<_> = g.entries.iter().filter(|e| e.starting_lba != 0 || e.ending_lba != 0).collect();
    assert_eq!(used.len(), 1);
    assert_eq!(used[0].starting_lba, 34);
    assert_eq!(used[0].partition_name.as_str(), "test");
}

#[test]
pub(crate) fn gpt_roundtrip_4kn() {
    let src = src_from_gpt("g4kn", 4096);
    let g = load_gpt(&src).unwrap().expect("gpt should parse at 4096");
    // 4Kn 盘上 128 条目 × 128B = 16 KiB = 4 扇区
    let geom = EntryArrayGeometry::new(4096, g.header.size_of_partition_entry, g.header.number_of_partition_entries, MAX_ARRAY_BYTES).unwrap();
    assert_eq!(geom.lba_span(), 4);
}

#[test]
pub(crate) fn corrupt_metadata_refused() {
    // 两例都先补保护 MBR：否则 load_gpt 的前置检查直接返回 None，测不到损坏分支
    // 主头损坏 → 回退盘尾备份头（主备互备），state 标记 Stale 交写入路径重建主头
    let mut data = fixture_gpt(512);
    data[600] ^= 0xFF; // LBA1 头部区域内翻转（破坏 CRC）
    let mut src = src_from("cbad1", data);
    let ss = src.sector_size;
    ensure_protective_mbr(&mut src, ss).unwrap();
    let g = load_gpt(&src).unwrap().expect("must fall back to backup header");
    assert!(
        matches!(g.state, GptState::NeedsRepair { cause: HeaderIssue::PrimaryUnreadable }),
        "fallback must be marked for repair"
    );
    assert!(g.entries.iter().any(|e| e.ending_lba != 0), "entries must come from backup array");
    // 主副本的**条目数组**损伤：同样用备份副本救回（UEFI 2.10 §5.3.2 的主备互备），
    // 而不是整体失败。两份数组是分别写入的，这正是回退存在的意义
    let mut data2 = fixture_gpt(512);
    data2[2 * 512 + 10] ^= 0xFF; // LBA2 = 主条目数组
    let mut src2 = src_from("cbad2", data2);
    let ss = src2.sector_size;
    ensure_protective_mbr(&mut src2, ss).unwrap();
    let g2 = load_gpt(&src2)
        .expect("primary array damage must fall back to the backup")
        .expect("backup copy is intact");
    assert!(
        matches!(g2.state, GptState::NeedsRepair { cause: HeaderIssue::PrimaryUnreadable }),
        "recovered-from-backup must be marked for repair"
    );
    assert!(g2.entries.iter().any(|e| e.ending_lba != 0), "entries must come from backup array");
    // 两份数组都坏 → 最终失败，且报的是"数组损伤"，
    // 绝不能因为"试过备份"就静默接受损坏的主副本
    let mut data3 = fixture_gpt(512);
    data3[2 * 512 + 10] ^= 0xFF; // 主条目数组
    data3[(67 * 512) + 10] ^= 0xFF; // 备条目数组：末块 99 − 跨度 32 = LBA 67
    let mut src3 = src_from("cbad3", data3);
    let ss = src3.sector_size;
    ensure_protective_mbr(&mut src3, ss).unwrap();
    assert!(matches!(
        load_gpt(&src3),
        Err(GptError::EntryArrayCorrupt { copy: GptCopyKind::Primary })
    ));
}

/// 主头签名在而 CRC 坏、且盘尾没有备份：必须报"副本损伤"，不能静默当成"无分区表"。
/// 后者会让 new 覆盖掉或许还能救回的表——load_gpt 末尾那条设计原则要挡的正是它
#[test]
pub(crate) fn damaged_primary_without_backup_is_reported() {
    // 对照组：只抹掉盘尾备份头、头本身完好 → 仍是可用的表。
    // 有它才能证明下面那个 Err 来自头损伤，而不是"没有备份"本身
    let mut intact = fixture_gpt(512);
    let last = intact.len() - 512;
    intact[last..].fill(0); // 备份头所在扇区清零
    let mut ctl = src_from("hdmg_ctl", intact);
    let ss = ctl.sector_size;
    ensure_protective_mbr(&mut ctl, ss).unwrap();
    assert!(load_gpt(&ctl).unwrap().is_some(), "对照组：头完好时缺备份不影响解析");

    let mut data = fixture_gpt(512);
    let last = data.len() - 512;
    data[last..].fill(0);
    data[512 + 16] ^= 0xFF; // LBA1 头部 CRC 字段内翻转：签名完好，CRC 对不上
    let mut src = src_from("hdmg", data);
    let ss = src.sector_size;
    ensure_protective_mbr(&mut src, ss).unwrap();
    assert!(
        matches!(load_gpt(&src), Err(GptError::HeaderCorrupt { copy: GptCopyKind::Primary, .. })),
        "签名在而头 CRC 坏且无备份 ⇒ 必须报 HeaderCorrupt(primary)"
    );
}

/// 读不出来只是"这一处"的事：主头那一段读失败时，盘尾备份照旧可用。
/// 让局部读取失败否掉整块盘，等于一次坏扇区就丢掉唯一能救回的表
#[test]
pub(crate) fn unreadable_primary_sector_falls_back_to_backup() {
    let src = src_from_gpt("iofb", 512);
    {
        let _fault = crate::dev::ReadFaultGuard::at(512); // LBA1；盘尾备份在另一处偏移
        let g = load_gpt(&src).expect("a bad sector must not fail the whole load").expect("the backup copy is intact");
        assert!(
            matches!(g.state, GptState::NeedsRepair { cause: HeaderIssue::PrimaryUnreadable }),
            "recovered-from-backup must be marked for repair: {:?}",
            g.state
        );
        assert!(g.entries.iter().any(|e| e.ending_lba != 0), "entries must come from the backup array");
    }
    // 守卫析构即撤掉注入：同一处再读正常，认的是主副本
    assert!(load_gpt(&src).unwrap().is_some(), "the fault must not outlive its guard");
}

/// 两份副本的**头**都不可用 ⇒ 报错，不能静默成"无表"——后者会让 new 覆盖掉或许
/// 还能救回的表。与 damaged_primary_without_backup_is_reported 成对：
/// 那条测的是单份损伤走回退，这条测的是两份都损伤必须浮出
#[test]
pub(crate) fn both_headers_unusable_is_an_error_not_absence() {
    let mut data = fixture_gpt(512);
    let last = data.len() - 512;
    data[512 + 16] ^= 0xFF; // 主头 CRC 字段：签名在，CRC 对不上
    data[last + 16] ^= 0xFF; // 备头同样处理
    let mut src = src_from("bothhdr", data);
    let ss = src.sector_size;
    ensure_protective_mbr(&mut src, ss).unwrap();
    assert!(
        matches!(load_gpt(&src), Err(GptError::HeaderCorrupt { copy: GptCopyKind::Primary, .. })),
        "两份都坏必须报错，且报首个损伤"
    );
}

/// 两份副本都没有 GPT 签名（空盘）⇒ Ok(None)。与上一条成对：损伤与"没有表"必须区分开
#[test]
pub(crate) fn no_gpt_signature_is_absence() {
    let mut src = src_from("nogpt", vec![0u8; 100 * 512]);
    let ss = src.sector_size;
    ensure_protective_mbr(&mut src, ss).unwrap();
    assert!(pmbr_shape_valid(&src).unwrap(), "前提：形状成立才会走到副本解析");
    assert!(load_gpt(&src).unwrap().is_none(), "无签名 ⇒ 无表，不是损伤");
}

/// PMBR 形状成立而两份 GPT 头的签名都丢了 ⇒ GptDamaged，不得落成 None——
/// None 会让 new 把还能救回的盘当无表盘覆盖、resize 走 superfloppy 整盘扩。
/// 与上一条成对：load_gpt 只回答表能否解析（这里确实解析不出），盘型判定
/// 在它之上补回"这是一块 GPT 盘"的事实
#[test]
pub(crate) fn pmbr_intact_without_gpt_headers_is_damaged_not_absence() {
    let mut src = src_from_gpt("pmbrdead", 512);
    // 两份头都毁到 EFI PART 不在：主头 LBA1、备头镜像尾 LBA
    src.write_at(512, &[0u8; 512]).unwrap();
    let last = (src.size / 512 - 1) * 512;
    src.write_at(last, &[0u8; 512]).unwrap();
    assert!(pmbr_shape_valid(&src).unwrap(), "前提：保护记录未动");
    assert!(load_gpt(&src).unwrap().is_none(), "前提：表解析不出，缺口只在盘型判定层");
    assert_eq!(table_label(&src).unwrap(), TableLabel::GptDamaged);
}

/// 条目数组：128 × 2^n 合规，其余拒绝；字节数上限是策略参数（非规范），
/// 超限即拒绝而不是按字段做巨型分配
#[test]
pub(crate) fn entry_array_geometry_bounds() {
    assert!(EntryArrayGeometry::new(512, 128, 128, MAX_ARRAY_BYTES).is_ok());
    assert!(EntryArrayGeometry::new(4096, 128, 128, MAX_ARRAY_BYTES).is_ok());
    assert!(EntryArrayGeometry::new(512, 256, 128, MAX_ARRAY_BYTES).is_ok());
    // 条目数为 0 / 单条目不合规 / 扇区大小为 0
    assert!(EntryArrayGeometry::new(512, 128, 0, MAX_ARRAY_BYTES).is_err());
    assert!(EntryArrayGeometry::new(512, 192, 128, MAX_ARRAY_BYTES).is_err());
    assert!(EntryArrayGeometry::new(0, 128, 128, MAX_ARRAY_BYTES).is_err());
    // 16 MiB 上限：128B 条目 ⇒ 上界 131072 条；再大一档即拒绝。
    // 上限是调用方的策略输入：同一个"越限"几何换个宽松上限就合法
    assert!(EntryArrayGeometry::new(512, 128, 131_072, MAX_ARRAY_BYTES).is_ok());
    assert!(EntryArrayGeometry::new(512, 128, 262_144, MAX_ARRAY_BYTES).is_err());
    assert!(EntryArrayGeometry::new(512, 128, 262_144, u64::MAX).is_ok());
}

/// 跨度按表自身的扇区大小算：128 × 128B 在 512B 下 32 扇区、在 4Kn 下 4 扇区
#[test]
pub(crate) fn lba_span_follows_table_sector_size() {
    assert_eq!(EntryArrayGeometry::new(512, 128, 128, MAX_ARRAY_BYTES).unwrap().lba_span(), 32);
    assert_eq!(EntryArrayGeometry::new(4096, 128, 128, MAX_ARRAY_BYTES).unwrap().lba_span(), 4);
    // 256 条目 × 256B = 64 KiB ⇒ 4Kn 下 16 扇区
    assert_eq!(EntryArrayGeometry::new(4096, 256, 256, MAX_ARRAY_BYTES).unwrap().lba_span(), 16);
}

/// 槽位换算：分区号上界来自本表的条目数，128 不是硬上限
#[test]
pub(crate) fn slot_is_derived_from_entry_count() {
    let g = EntryArrayGeometry::new(512, 128, 256, MAX_ARRAY_BYTES).unwrap();
    assert_eq!(g.slot(1), Some(0));
    assert_eq!(g.slot(256), Some(255));
    assert_eq!(g.slot(257), None);
    assert_eq!(g.slot(0), None);
}

/// 条目级结构校验（UEFI 2.10 §5.3.1）：倒挂 / 越出 usable range 在提交前即拒绝——
/// 写入侧与解析侧同一判据，盘上不会出现"字节写对了但自己读不回来"的表；
/// 两个 LBA 字段同时为零按未使用放行（本工具的规范化分类）
#[test]
pub(crate) fn entry_geometry_validation() {
    let mut src = src_from_gpt("eval", 512);
    let mut g = load_gpt(&src).unwrap().unwrap();
    let first = g.header.first_usable_lba;
    let last_usable = g.header.last_usable_lba;
    let last = src.size / 512 - 1;
    assert!(last_usable < last, "fixture must leave space beyond usable range");
    let set = |g: &mut RawGpt, s: u64, e: u64| {
        g.entries[0] = GPTPartitionEntry {
            partition_type_guid: LINUX_FS_TYPE_GUID,
            unique_partition_guid: [0x77; 16],
            starting_lba: s,
            ending_lba: e,
            attribute_bits: 0,
            partition_name: "bad".into(),
        };
    };
    // 倒挂：start > end
    set(&mut g, 100, 50);
    let why = commit_gpt(&mut src, &g, last).unwrap_err().to_string();
    assert!(why.contains("partition 1") && why.contains("starting_lba 100 > ending_lba 50"), "{why}");
    // end == 0 且 start != 0：同属倒挂，不是"未使用"
    set(&mut g, 100, 0);
    assert!(commit_gpt(&mut src, &g, last).is_err());
    // 越出 usable 上界（实体仍在容器内，故不是越界条目）
    set(&mut g, first, last_usable + 1);
    let why = commit_gpt(&mut src, &g, last).unwrap_err().to_string();
    assert!(why.contains("outside usable range"), "{why}");
    // 低于 usable 下界
    set(&mut g, first - 1, first + 5);
    assert!(commit_gpt(&mut src, &g, last).is_err());
    // 两字段同时为零 → 未使用（类型 GUID 残留也宽容）
    set(&mut g, 0, 0);
    commit_gpt(&mut src, &g, last).unwrap();
    let g2 = load_gpt(&src).unwrap().unwrap();
    assert_eq!(g2.entries[0].starting_lba, 0);
    // 规范化分类的直接推论：存活条目里 end == 0 ⟺ start == 0
    assert!(g2.entries.iter().all(|e| (e.ending_lba == 0) == (e.starting_lba == 0)));
}

#[test]
pub(crate) fn commit_roundtrip_via_gptman() {
    // 写入后用 gptman 读回，验证序列化与 CRC 符合规范
    let mut src = src_from_gpt("gcommit", 512);
    let mut g = load_gpt(&src).unwrap().unwrap();
    g.entries[2] = GPTPartitionEntry {
        partition_type_guid: [0x03; 16],
        unique_partition_guid: [0x04; 16],
        starting_lba: 40,
        ending_lba: 46,
        attribute_bits: 0,
        partition_name: "second".into(),
    };
    let last_lba = src.size / 512 - 1;
    commit_gpt(&mut src, &g, last_lba).unwrap();
    // gptman 读回即规范侧验收（gptman 索引 1 起：g.entries[2] 为 partition #3）
    let mut f = src.file.try_clone().unwrap();
    let gpt = GPT::find_from(&mut f).expect("gptman must accept our serialization");
    let p = &gpt[3];
    assert_eq!(p.starting_lba, 40);
    assert_eq!(p.partition_name.as_str(), "second");
    // 保护 MBR 幂等
    let ss = src.sector_size;
    ensure_protective_mbr(&mut src, ss).unwrap();
    ensure_protective_mbr(&mut src, ss).unwrap();
}