#![allow(clippy::let_underscore_must_use)] // 清理临时文件有意忽略失败
use super::*;
use crate::testdir::test_dir;

/// `:N` 后缀的判定：分区号 1-based，`:0` 必须拒绝而不是折叠成"整盘"——
/// 折叠会把一次针对具体分区的操作放大成对整盘的表操作。
/// `:last` 与数字号并列，大小写不敏感
#[test]
pub(crate) fn parse_target_partition_suffix() {
    assert_eq!(parse_target("img").unwrap(), ("img".to_string(), None));
    assert_eq!(parse_target("img:1").unwrap(), ("img".to_string(), Some(PartSelector::Number(1))));
    assert_eq!(parse_target("img:128").unwrap(), ("img".to_string(), Some(PartSelector::Number(128))));
    // 解析值域 1..=128，超出即拒绝
    assert!(parse_target("img:129").is_err());
    assert!(parse_target("img:4294967295").is_err());
    assert!(parse_target("img:0").is_err());
    assert!(parse_target("img:4294967296").is_err());
    assert!(parse_target("img:").is_err());
    assert_eq!(parse_target("img:last").unwrap(), ("img".to_string(), Some(PartSelector::Last)));
    assert_eq!(parse_target("img:LAST").unwrap(), ("img".to_string(), Some(PartSelector::Last)));
    // 文件名的冒号不是分区后缀（其后不是纯数字，也不是 last）
    assert_eq!(parse_target("/a/b:c.img").unwrap(), ("/a/b:c.img".to_string(), None));
}

/// 分区节点不暴露 loop 属性：沿 `partition` 属性上溯到整设备节点再读。
/// 非 loop 的盘同样上溯后得到"没有 backing_file"，不是"读不出来"
#[cfg(target_os = "linux")]
#[test]
pub(crate) fn loop_backing_is_read_from_the_device_above_the_partition() {
    let dir = test_dir("sysfs");
    let root = dir.path();
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
}

/// 指纹的设备 ID 与容量都取**整设备节点**：以分区节点解析与以整盘节点解析得到同一份
/// 指纹，故整盘与它的分区共享同一份恢复现场。`dm/name` 不在身份证据里——映射名不是身份，
/// 只有它可读时按"无 ID"处理（容量仍作为辅助证据）
#[cfg(target_os = "linux")]
#[test]
pub(crate) fn disk_identity_is_shared_by_a_partition_and_its_whole_disk() {
    let dir = test_dir("diskid");
    let root = dir.path();
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
}

/// 字面路径与 canonical 指同一文件（相对路径 / `./` / `..` 这类非规范化成分）时，
/// 落点仍只有一个：两种写法都从 canonical 派生，不因写法不同多出第二份现场
#[test]
pub(crate) fn non_canonical_literal_yields_the_canonical_slot() {
    let dir = test_dir("iddup");
    let img = dir.join("x.img");
    std::fs::write(&img, b"x").unwrap();
    let canonical = std::fs::canonicalize(&img).unwrap();
    // 同一文件的另一条写法：`<tmp>/sub/../x.img` 文本上不同于 canonical，指向同一处
    let sub = dir.join("sub");
    std::fs::create_dir_all(&sub).unwrap();
    let literal = sub.join("..").join(canonical.file_name().unwrap());
    assert_ne!(literal, canonical, "the fixture must be textually different");
    let id = TargetIdentity::image_at(&canonical, &literal);
    assert_eq!(id.journal_path(), suffix_path(&canonical, JOURNAL_SUFFIX));
}

/// 相对写法不移动记录的落点：记录目录由 canonical 派生，`..` / 相对名不会在当前
/// 工作目录下多出一个 `<literal>.diskedit/`——那会让同一目标在不同目录下出现两份现场
#[test]
pub(crate) fn a_relative_literal_does_not_move_the_record() {
    let dir = test_dir("relrec");
    let img = dir.join("x.img");
    std::fs::write(&img, b"x").unwrap();
    let canonical = std::fs::canonicalize(&img).unwrap();
    // 一条相对写法：本进程的工作目录与临时目录无关，它解析不到任何文件——
    // 若落点跟着字面路径走，这里就会在当前目录旁造出一份现场
    let literal = Path::new("..").join(canonical.file_name().unwrap());

    let id = TargetIdentity::image_at(&canonical, &literal);
    let expected = suffix_path(&canonical, RECORD_SUFFIX).join(RECORD_NAME);
    assert_eq!(id.record_path(), Some(expected.as_path()));
    assert!(!suffix_path(&literal, RECORD_SUFFIX).exists(), "no record may be derived from the relative spelling");
}

/// 身份取自已打开的句柄：符号链接只是别名，身份主体落在真实路径的兄弟文件上；
/// 打开之后路径被改名/替换时必须拒绝——fd 仍指向旧 inode，而字面路径已属于别人，
/// 按字面路径派生伴随文件会把现场放到另一份数据旁
#[cfg(target_os = "linux")]
#[test]
pub(crate) fn image_identity_follows_the_opened_handle_not_the_path() {
    let dir = test_dir("idfd");
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

/// 落盘命名的契约锚点：命名空间的字面量只在这一处出现，三类伴随文件各自的后缀
/// 由它派生校验——改命名空间或改某一类后缀，这里该失败
#[test]
pub(crate) fn artifact_names_are_pinned_as_documented() {
    let ns = JOURNAL_SUFFIX.strip_suffix("journal").expect("journal suffix ends with its stem");
    assert_eq!(ns, ".diskedit.");
    assert_eq!(LOCK_SUFFIX, format!("{ns}lock"));
    assert_eq!(LOG_SUFFIX, format!("{ns}log"));

    // 镜像：journal 是目标路径的同目录兄弟；锁与记录同落在
    // `<image>.diskedit` 目录内
    let img = PathBuf::from("/tmp/x.img");
    let id = TargetIdentity::resolve_image(&img);
    assert_eq!(id.journal_path(), suffix_path(&img, JOURNAL_SUFFIX));
    assert_eq!(id.lock_path(), suffix_path(&img, RECORD_SUFFIX).join(RECORD_LOCK_NAME));
    assert_eq!(id.log_path(None), suffix_path(&img, LOG_SUFFIX));
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

/// 非 loop 块设备的身份是**盘级**的：恢复现场与锁按 disk_key 落点。
/// 断言用 state_dir() 自身拼期望值——测试不改环境变量，进程内并行测试共享环境
#[test]
pub(crate) fn block_identity_keys_on_the_whole_disk() {
    let id = TargetIdentity::from_block_keys(Path::new("/dev/sdb1"), "sdb-1234".to_string());
    let disk_tok = key_token("sdb-1234");
    assert_eq!(id.journal_path(), state_dir().join(format!("{disk_tok}{JOURNAL_SUFFIX}")));
    assert_eq!(id.lock_path(), state_dir().join(format!("{disk_tok}{LOCK_SUFFIX}")));
}

/// 锁键按**载体**收敛，记录键按目标分开：同一盘的两个分区各有自己的记录目录，
/// 但共用同一把锁；整盘节点与它的分区节点同样共用。锁落点在全盘唯一的 state_dir
/// 根下，与记录目录不同层
#[test]
pub(crate) fn block_lock_converges_to_the_carrier_while_records_stay_per_target() {
    let dir = state_dir();
    let with_record = |node: &str, hash: &str, disk_key: &str| {
        TargetIdentity::from_block_parts(
            Path::new(node),
            disk_key.to_string(),
            Some(dir.join(format!("recovery/{hash}/record"))),
        )
    };
    let part1 = with_record("/dev/sdb1", "hash-1", "sdb-1234");
    let part2 = with_record("/dev/sdb2", "hash-2", "sdb-1234");
    let whole = with_record("/dev/sdb", "hash-0", "sdb-1234");
    let expected = dir.join(format!("{}{LOCK_SUFFIX}", key_token("sdb-1234")));
    assert_eq!(part1.lock_path(), expected.as_path());
    assert_eq!(part2.lock_path(), expected.as_path());
    assert_eq!(whole.lock_path(), expected.as_path());
    assert_ne!(part1.record_path(), part2.record_path());
    assert_ne!(part1.record_path(), whole.record_path());

    let other = with_record("/dev/sdc1", "hash-3", "sdc-9999");
    assert_ne!(other.lock_path(), part1.lock_path());
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
    }
    impl Drop for Bound {
        fn drop(&mut self) {
            let _ = std::process::Command::new("losetup").arg("-d").arg(&self.dev).output();
        }
    }

    let dir = test_dir("loop_id");
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
    let _cleanup = Bound { dev: dev.clone() };

    let id = TargetIdentity::loop_backed(&dev, backing.clone()).expect("a bound backing file resolves");
    assert_eq!(id.journal_path(), suffix_path(&backing, JOURNAL_SUFFIX));
    assert_eq!(id.lock_path(), suffix_path(&backing, RECORD_SUFFIX).join(RECORD_LOCK_NAME));

    // backing 已删：身份不再可解析，不退到 sysfs 字符串
    std::fs::remove_file(&backing).unwrap();
    assert!(TargetIdentity::loop_backed(&dev, backing.clone()).is_err());
}

/// 同一 backing 上两个 loop 共用一把锁：v1 按载体整块独占，不重叠的区间也互斥，
/// 这是有意的加粗：占用域覆盖整盘及其全部分区。
/// Linux 专属用例；本机（Windows）不编译也不运行，Linux 上第二次绑定失败即跳过
#[cfg(target_os = "linux")]
#[test]
pub(crate) fn two_loops_over_one_backing_share_the_carrier_lock() {
    struct Bound(PathBuf);
    impl Drop for Bound {
        fn drop(&mut self) {
            let _ = std::process::Command::new("losetup").arg("-d").arg(&self.0).output();
        }
    }
    fn bind(backing: &Path) -> Option<Bound> {
        let out = std::process::Command::new("losetup")
            .args(["--find", "--show"])
            .arg(backing)
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        Some(Bound(PathBuf::from(String::from_utf8_lossy(&out.stdout).trim())))
    }

    let dir = test_dir("loop_lock");
    let backing = dir.join("backing.img");
    std::fs::write(&backing, vec![0u8; 1 << 20]).unwrap();
    // 两个映射都绑不上同一个文件时没有可造出的现场（无空闲 loop 或内核拒绝重复绑定）
    let (Some(first_dev), Some(second_dev)) = (bind(&backing), bind(&backing)) else { return };
    let first = TargetIdentity::loop_backed(&first_dev.0, backing.clone()).expect("a bound backing resolves");
    let second = TargetIdentity::loop_backed(&second_dev.0, backing.clone()).expect("a bound backing resolves");
    assert_eq!(first.lock_path(), second.lock_path());
}

/// 镜像身份以 realpath 为主体：经符号链接打开与直接打开收敛到同一现场；
/// 日志命名沿用字面路径，不因 canonicalize 打断连续性
#[cfg(target_os = "linux")]
#[test]
pub(crate) fn image_identity_prefers_the_realpath() {
    let dir = test_dir("img_id");
    let real = dir.join("real.img");
    std::fs::write(&real, b"x").unwrap();
    let link = dir.join("link.img");
    std::os::unix::fs::symlink(&real, &link).unwrap();

    let via_link = TargetIdentity::resolve_image(&link);
    let direct = TargetIdentity::resolve_image(&real);
    assert_eq!(via_link.journal, direct.journal, "both spellings must share one journal");
    assert_eq!(via_link.lock_path(), direct.lock_path());
    assert_eq!(via_link.log_path(None), suffix_path(&link, LOG_SUFFIX));
}

/// fail-closed：sysfs 拓扑解析不出来 ⇒ 拒绝（错误信息带设备名），不退到
/// "devname-容量"的调用方身份——那个退化会让 /dev/sdb 与 /dev/sdb1 得到两把锁。
/// 非 Linux 平台没有 sysfs 概念，`resolve_block` 本就不走该判据
#[cfg(target_os = "linux")]
#[test]
pub(crate) fn block_identity_refuses_unresolvable_topology() {
    // 普通文件被当作 is_block 传入：rdev=0 ⇒ /sys/dev/block/0:0 不存在 ⇒ 拒绝
    let dir = test_dir("notablock");
    let fake = dir.join("diskedit_notablock");
    std::fs::write(&fake, b"x").unwrap();
    let e = TargetIdentity::resolve_block(&fake).expect_err("an unresolvable topology must be refused");
    assert!(e.contains("fake") || e.contains("diskedit_notablock"), "the error must name the device: {e}");
    assert!(e.contains("topology"), "{e}");
}

/// [`read_sysfs_attr`] 的三态必须可分（上条测试的细粒度锁）："不存在 ⇒ None"
/// 是整设备的正常路径，"读失败 ⇒ Err" 则不得降级——压成同一个 None 会让分区身份
/// 在读不出来的那一刻静默变成整设备身份
#[cfg(target_os = "linux")]
#[test]
pub(crate) fn sysfs_attr_distinguishes_missing_from_read_failure() {
    assert_eq!(
        read_sysfs_attr(Path::new("/nonexistent/diskedit/attr")).unwrap(),
        None,
        "不存在 ⇒ None"
    );
    let dir = test_dir("attr");
    let tmp = dir.join("attr");
    std::fs::write(&tmp, "7\n").unwrap();
    assert_eq!(read_sysfs_attr(&tmp).unwrap().as_deref(), Some("7"));
    // 存在但值空：与"不存在"分开（空的 partition 属性是分区侧的异常，不该走整设备分支）
    std::fs::write(&tmp, " \n").unwrap();
    assert_eq!(read_sysfs_attr(&tmp).unwrap().as_deref(), Some(""));
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

/// 凭据比对的判据只有一条：两侧都是 `Present` 且不同才算不符。不可得与本次生成
/// 都不是"相符"，只是没有可比的值——它们不提供判据，也就不能成为拒绝或放行的理由
#[test]
pub(crate) fn disk_guid_mismatch_only_compares_two_present_values() {
    let a = DiskGuidState::Present([0x11; 16]);
    let b = DiskGuidState::Present([0x22; 16]);
    assert!(disk_identity_mismatch(a, a).is_none(), "equal GUIDs match");
    let why = disk_identity_mismatch(a, b).expect("different GUIDs must be refused");
    assert!(why.contains("disk GUID"), "the refusal must name the credential: {why}");
    assert!(
        disk_identity_mismatch(a, DiskGuidState::Unavailable).is_some(),
        "a recorded GUID against a target without a GPT is not a match"
    );
    assert!(
        disk_identity_mismatch(DiskGuidState::Unavailable, a).is_some(),
        "a target that gained a GPT is not the target the record was written for"
    );
    assert!(
        disk_identity_mismatch(DiskGuidState::GeneratedThisOperation, b).is_none(),
        "a generated credential is not evidence either way"
    );
    assert!(disk_identity_mismatch(DiskGuidState::Unavailable, DiskGuidState::Unavailable).is_none());
}

/// 凭据段的编解码是定长的：种类字节 + 16 字节 GUID，未知种类必须拒绝而不是猜成某个
/// 已知值
#[test]
pub(crate) fn disk_guid_roundtrip_and_unknown_kind() {
    for state in [
        DiskGuidState::Unavailable,
        DiskGuidState::Present([0x5A; 16]),
        DiskGuidState::GeneratedThisOperation,
    ] {
        assert_eq!(DiskGuidState::decode(&state.encode()).unwrap(), state);
    }
    assert!(DiskGuidState::decode(&[0xFF; CREDENTIAL_LEN]).is_err(), "an unknown kind is not decodable");
}

/// 恒等字段按「设备类别、稳定身份串、载体 file_id」顺序拼接，
/// 取 sha256 前 16 字节的小写 hex
#[test]
pub(crate) fn identity_hash_uses_the_contract_field_order() {
    assert_eq!(
        identity_hash_hex("block", "naa.5000c500aabbccdd", None),
        "0a36fee99502f14d6517b5acd3a02885"
    );
    assert_eq!(
        identity_hash_hex("partition", "naa.5000c500aabbccdd1", None),
        "17e70bce34779eb9442a972e0915213c"
    );
    assert_eq!(
        identity_hash_hex("block", "eui.0025385812345678", None),
        "7c29cbea42661faab241a51943da9950"
    );
    assert_eq!(identity_hash_hex("block", "naa.5000c500aabbccdd", None).len(), 32);

    let id = StableIdentity::Strong {
        device_class: "block",
        stable_key: "naa.5000c500aabbccdd".to_string(),
    };
    let hash = id.hash_hex().unwrap();
    assert_eq!(
        record_dir_under(Path::new("/state"), &hash),
        Path::new("/state/recovery/0a36fee99502f14d6517b5acd3a02885")
    );
    assert_eq!(
        id.record_dir().unwrap().file_name().unwrap().to_string_lossy(),
        hash,
        "the record directory is named by the identity-hash"
    );
    assert!(id.record_path().unwrap().ends_with("record"));
    assert!(StableIdentity::Unusable("no identity".to_string()).hash_hex().is_none());
}

/// 块设备身份取 WWN → EUI → serial 的第一个非空原文；分区串是
/// 载体串 + 分区号；dm / md / 没有身份属性都是 `Unusable`
#[cfg(target_os = "linux")]
#[test]
pub(crate) fn block_identity_is_wwn_eui_or_serial_and_rejects_dm_md() {
    let dir = test_dir("hostrepo");
    let root = dir.path().to_path_buf();
    let make = |name: &str| {
        let d = root.join(name);
        std::fs::create_dir_all(&d).unwrap();
        d
    };

    let sda = make("sda");
    std::fs::write(sda.join("wwid"), "naa.5000c500aabbccdd\n").unwrap();
    std::fs::write(sda.join("size"), "2048\n").unwrap();
    let sda1 = make("sda/sda1");
    std::fs::write(sda1.join("partition"), "1\n").unwrap();
    std::fs::write(sda1.join("size"), "1024\n").unwrap();
    assert_eq!(
        block_stable_identity(&sda),
        StableIdentity::Strong { device_class: "block", stable_key: "naa.5000c500aabbccdd".into() }
    );
    assert_eq!(
        block_stable_identity(&sda1),
        StableIdentity::Strong {
            device_class: "partition",
            stable_key: "naa.5000c500aabbccdd1".into()
        }
    );

    let nvme = make("nvme0n1");
    std::fs::write(nvme.join("wwid"), "eui.0025385812345678\n").unwrap();
    assert_eq!(
        block_stable_identity(&nvme),
        StableIdentity::Strong { device_class: "block", stable_key: "eui.0025385812345678".into() }
    );

    let serial_only = make("sdb");
    std::fs::create_dir_all(serial_only.join("device")).unwrap();
    std::fs::write(serial_only.join("device/serial"), "SN123456\n").unwrap();
    assert_eq!(
        block_stable_identity(&serial_only),
        StableIdentity::Strong { device_class: "block", stable_key: "SN123456".into() }
    );

    let dm = make("dm-0/dm");
    std::fs::write(dm.join("uuid"), "mpath-1234\n").unwrap();
    assert!(matches!(block_stable_identity(&root.join("dm-0")), StableIdentity::Unusable(_)));

    let md = make("md0/md");
    std::fs::write(md.join("uuid"), "abc:def\n").unwrap();
    assert!(matches!(block_stable_identity(&root.join("md0")), StableIdentity::Unusable(_)));

    let bare = make("sdc");
    std::fs::write(bare.join("size"), "2048\n").unwrap();
    assert!(matches!(block_stable_identity(&bare), StableIdentity::Unusable(_)));
}
