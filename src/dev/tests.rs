#![allow(clippy::let_underscore_must_use)] // 清理临时文件有意忽略失败
use super::*;
use crate::testdir::test_dir;

/// 隔离目录列不出来时不能当成"没有既有编号"：编号会从 0 重来，盖掉已有证据
#[test]
fn a_quarantine_directory_that_cannot_be_listed_is_an_error() {
    let root = test_dir("quarantine_unreadable");
    let blocker = root.join("not-a-directory");
    std::fs::write(&blocker, b"x").unwrap();
    let from = blocker.join(RECORD_NAME);
    let e = quarantine_move(&from)
        .expect_err("listing the quarantine directory must not be taken as empty");
    assert!(e.to_string().contains("cannot enumerate the quarantine directory"), "{e}");
}

/// 副本已建、原名删不掉时报告 `CopiedOnly`：本次运行确实新增了一个隔离条目，
/// 不能按"什么都没发生"处理；原件与副本都要留在原地供重跑按 inode 收敛
#[test]
fn a_copy_whose_original_cannot_be_removed_is_reported_as_copied_only() {
    let root = test_dir("quarantine_copied_only");
    let from = root.join(RECORD_NAME);
    std::fs::write(&from, b"record").unwrap();
    let _fault = RemoveFaultGuard::armed();
    let outcome = quarantine_move(&from).expect("building the copy succeeds; only the removal is refused");
    let Quarantine::CopiedOnly { to, cause } = outcome else {
        panic!("a refused removal must not be reported as Moved");
    };
    assert!(from.exists(), "the original stays while the copy exists");
    assert!(to.exists(), "the copy must be in place");
    assert_eq!(to.parent(), Some(root.path()), "the copy lands in the original's directory");
    assert!(cause.to_string().contains("injected remove fault"), "{cause}");
}

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
    assert_eq!(
        id.lock_path(),
        image_record_dir(&canonical).expect("the per-user state root is available in tests").join(RECORD_LOCK_NAME)
    );
}

/// 相对写法不移动记录的落点：记录目录由 canonical 的文件身份派生，`..` / 相对名
/// 不会在当前工作目录下多出一个 `<literal>.diskedit/`——那会让同一目标在不同目录下
/// 出现两份现场
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
    let expected = image_record_dir(&canonical)
        .expect("the per-user state root is available in tests")
        .join(RECORD_NAME);
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
    let id = FileSource::sample_image(&link, Some((&f, &meta))).unwrap();
    let canonical = std::fs::canonicalize(&a).unwrap();
    assert_eq!(
        id.lock_path(),
        image_record_dir(&canonical).expect("the per-user state root is available in tests").join(RECORD_LOCK_NAME),
        "the identity subject must be the opened file, not the spelling used to open it"
    );
    drop(f);

    // 打开后把文件改名，并在原路径放一个新文件：fd 仍指向旧 inode，字面路径已换人
    let f = std::fs::OpenOptions::new().read(true).write(true).open(&a).unwrap();
    let meta = f.metadata().unwrap();
    std::fs::rename(&a, dir.join("moved.img")).unwrap();
    std::fs::write(&a, vec![0u8; 4096]).unwrap();
    let e = FileSource::sample_image(&a, Some((&f, &meta))).unwrap_err();
    assert!(e.to_string().contains("no longer names the file that was opened"), "{e}");
}

/// 打开后路径被替换：后续写入仍落在已打开的那个 inode 上，原路径的新文件一个字节都不动。
/// 这是 fd 交棒的宿主侧前提——QEMU 继承的正是这个句柄（`exec::qemu::command` 传
/// `src.raw_fd()`），因此"路径换了人"与"写入落点"在类型上无关
#[cfg(target_os = "linux")]
#[test]
pub(crate) fn a_swapped_path_does_not_redirect_writes() {
    let dir = test_dir("swapwrite");
    let path = dir.join("disk.img");
    std::fs::write(&path, vec![0u8; 8192]).unwrap();

    let mut src = WriteSource::unowned(FileSource::open_image(&path, None, true).unwrap());
    src.write_fixture(0, &[1u8; 512]).unwrap();

    std::fs::rename(&path, dir.join("moved.img")).unwrap();
    std::fs::write(&path, vec![0u8; 8192]).unwrap();

    src.write_fixture(512, &[2u8; 512]).unwrap();

    let original = std::fs::read(dir.join("moved.img")).unwrap();
    assert_eq!(&original[0..512], &[1u8; 512], "the first write must be in the opened file");
    assert_eq!(&original[512..1024], &[2u8; 512], "the write after the swap must still land there");
    assert_eq!(
        std::fs::read(&path).unwrap(),
        vec![0u8; 8192],
        "the file now sitting at the old path must stay untouched"
    );
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

/// 落盘命名：命名空间的字面量只在这一处出现，两类伴随文件各自的后缀由它派生校验
#[test]
pub(crate) fn artifact_names_are_pinned_as_documented() {
    let ns = LOCK_SUFFIX.strip_suffix("lock").expect("lock suffix ends with its stem");
    assert_eq!(ns, ".diskedit.");
    assert_eq!(LOG_SUFFIX, format!("{ns}log"));

    // 镜像：锁与记录落在每用户状态根下的键目录内，日志是目标路径的同目录兄弟
    let img = PathBuf::from("/tmp/x.img");
    let id = TargetIdentity::resolve_image(&img);
    let record_dir = image_record_dir(&img).expect("the per-user state root is available in tests");
    assert_eq!(id.lock_path(), record_dir.join(RECORD_LOCK_NAME));
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

/// 非 loop 块设备的锁是**盘级**的：锁按 disk_key 落点，不带分区号。
/// 断言用 host_state_root() 自身拼期望值——测试不改环境变量，进程内并行测试共享环境
#[test]
pub(crate) fn block_identity_keys_on_the_whole_disk() {
    let id = TargetIdentity::from_block_keys(Path::new("/dev/sdb1"), "sdb-1234".to_string());
    let disk_tok = key_token("sdb-1234");
    assert_eq!(id.lock_path(), host_state_root().join(format!("{disk_tok}{LOCK_SUFFIX}")));
}

/// 锁键按**载体**收敛，记录键按目标分开：同一盘的两个分区各有自己的记录目录，
/// 但共用同一把锁；整盘节点与它的分区节点同样共用。锁落点在全盘唯一的 state_dir
/// 根下，与记录目录不同层
#[test]
pub(crate) fn block_lock_converges_to_the_carrier_while_records_stay_per_target() {
    let with_record = |node: &str, partition: Option<u32>, disk_key: &str| {
        TargetIdentity::from_block_parts(
            Path::new(node),
            disk_key.to_string(),
            crate::dev::LockKey::new(disk_key.to_string()),
            Some(key_for_test("sdb-carrier", partition)),
            partition,
        )
        .expect("a test identity with a short key encodes")
    };
    let part1 = with_record("/dev/sdb1", Some(1), "sdb-1234");
    let part2 = with_record("/dev/sdb2", Some(2), "sdb-1234");
    let whole = with_record("/dev/sdb", None, "sdb-1234");
    let expected = host_state_root().join(format!("{}{LOCK_SUFFIX}", key_token("sdb-1234")));
    assert_eq!(part1.lock_path(), expected.as_path());
    assert_eq!(part2.lock_path(), expected.as_path());
    assert_eq!(whole.lock_path(), expected.as_path());
    assert_ne!(part1.record_path(), part2.record_path());
    assert_ne!(part1.record_path(), whole.record_path());

    let other = with_record("/dev/sdc1", Some(1), "sdc-9999");
    assert_ne!(other.lock_path(), part1.lock_path());
}

/// 锁键不取 `dm/name`：映射名是用户可改的标签，`dmsetup rename` 不能把同一台设备拆成
/// 两把锁。有 `dm/uuid` 用它；没有就退到内核给的 `dm-N` 与容量——改名前后锁落点一致
#[cfg(target_os = "linux")]
#[test]
pub(crate) fn a_dm_rename_does_not_change_the_lock_slot() {
    let dir = test_dir("dmkey");
    let root = dir.path().to_path_buf();
    let node = root.join("dm-0");
    std::fs::create_dir_all(node.join("dm")).unwrap();
    std::fs::write(node.join("size"), "4096\n").unwrap();
    std::fs::write(node.join("dm/name"), "vg-data\n").unwrap();

    let before = TargetIdentity::resolve_block_at(&node, Path::new("/dev/dm-0")).unwrap();
    assert_eq!(block_lock_key(&node).unwrap().as_str(), format!("dm-0-{}", 4096 * 512));
    std::fs::write(node.join("dm/name"), "renamed\n").unwrap();
    let after = TargetIdentity::resolve_block_at(&node, Path::new("/dev/dm-0")).unwrap();
    assert_eq!(after.lock_path(), before.lock_path());

    std::fs::write(node.join("dm/uuid"), "mpath-1234\n").unwrap();
    assert_eq!(block_lock_key(&node).unwrap().as_str(), "mpath-1234");
}

/// 续跑只从当前目标的键派生出的唯一落点开始：同一个键得到同一落点（允许续跑），
/// 键不同则落点不同（读不到另一目标的记录）
#[test]
pub(crate) fn the_same_identity_key_is_the_same_resume_slot() {
    let slot = |node: &str, target: &str, partition: Option<u32>| {
        TargetIdentity::from_block_parts(
            Path::new(node),
            "sdb-1234".to_string(),
            crate::dev::LockKey::new("sdb-1234".to_string()),
            Some(key_for_test(target, partition)),
            partition,
        )
        .expect("a test identity with a short key encodes")
        .record_path()
        .map(Path::to_path_buf)
    };
    assert_eq!(slot("/dev/sdb1", "carrier", Some(1)), slot("/dev/sdb1", "carrier", Some(1)));
    assert_ne!(slot("/dev/sdb1", "carrier", Some(1)), slot("/dev/sdb2", "carrier", Some(2)));
    assert_ne!(slot("/dev/sdb1", "carrier", Some(1)), slot("/dev/sdb", "carrier", None));
    assert_ne!(slot("/dev/sdb1", "carrier1", Some(2)), slot("/dev/sdb1", "carrier", Some(12)));
}

/// 声明身份派生的键：记录按（名字, 分区号）分开，锁与占据声明按整盘名字收敛。
/// 声明名带 `declared:` 前缀，与设备声明的原文不共用命名空间
#[test]
pub(crate) fn a_declared_identity_derives_its_own_records_and_one_lock_per_name() {
    let carrier = |node: &str, partition| {
        TargetIdentity::from_block_parts(
            Path::new(node),
            "nbd-carrier".to_string(),
            crate::dev::LockKey::new(node.to_string()),
            None,
            partition,
        )
        .expect("a test identity with no record key has no encoding width to overflow")
    };
    let declare = |id: TargetIdentity, name: &str| {
        id.with_declared(&DeclaredIdentity::parse(name).unwrap()).expect("an unidentified block device accepts a name")
    };
    let whole = declare(carrier("/dev/nbd0", None), "export-a");
    let part3 = declare(carrier("/dev/nbd0p3", Some(3)), "export-a");
    assert_eq!(
        whole.record_path().map(Path::to_path_buf),
        Some(
            key_for_test("declared:export-a", None)
                .record_path()
                .expect("a short test key encodes")
        )
    );
    assert_eq!(
        part3.record_path().map(Path::to_path_buf),
        Some(
            key_for_test("declared:export-a", Some(3))
                .record_path()
                .expect("a short test key encodes")
        )
    );
    // 锁落在整盘名字上：分区节点与整盘节点共用一把
    let expected = host_state_root().join(format!("{}{LOCK_SUFFIX}", key_token("declared:export-a")));
    assert_eq!(whole.lock_path(), expected.as_path());
    assert_eq!(part3.lock_path(), expected.as_path());
    // 占据声明按载体落点：与名字无关，两个名字看到的是同一份声明
    let other_name = declare(carrier("/dev/nbd0", None), "export-b");
    assert_eq!(whole.claim_path(), other_name.claim_path());
    assert_ne!(whole.claim_path(), declare(carrier("/dev/nbd1", None), "export-a").claim_path());
    // 自由文本名字不冒充同名设备声明
    assert_ne!(
        whole.record_path().map(Path::to_path_buf),
        Some(
            key_for_test("naa.5000c500aabbccdd", None)
                .record_path()
                .expect("a short test key encodes")
        )
    );
}

/// 声明只补身份不可得的块设备：镜像与自带 WWN/serial 的设备按自己的身份定位
#[test]
pub(crate) fn only_an_unidentified_block_device_accepts_a_name() {
    let name = DeclaredIdentity::parse("export-a").unwrap();
    assert!(TargetIdentity::resolve_image(Path::new("/tmp/x.img")).with_declared(&name).is_err());
    let identified = TargetIdentity::from_block_parts(
        Path::new("/dev/sdb"),
        "naa.5000c500aabbccdd".to_string(),
        crate::dev::LockKey::new("naa.5000c500aabbccdd".to_string()),
        Some(key_for_test("naa.5000c500aabbccdd", None)),
        None,
    )
    .expect("a test identity with a short key encodes");
    assert!(identified.with_declared(&name).is_err());
}

/// loop 设备的身份绑定 backing **对象**：重新打开 backing、用已打开句柄解析名字，现场落
/// 该名字的兄弟文件与其 state_dir 落点。
/// backing 消失后身份不再可解析（名字已经不是对象），此处拒绝而非把现场挂到同名路径旁
///
/// 比对的一侧来自内核给这个 loop 记下的 backing 对象，所以这里必须有一个真绑定：
/// 绑不上（无空闲设备或没有权限）就没有可造出的现场，跳过
#[cfg(target_os = "linux")]
#[test]
pub(crate) fn loop_identity_converges_to_the_backing_file() {
    struct Bound {
        dev: PathBuf,
    }
    impl Drop for Bound {
        fn drop(&mut self) {
            // audit-allow: D-14
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
    assert_eq!(
        id.lock_path(),
        image_record_dir(&backing)
            .expect("the per-user state root is available in tests")
            .join(RECORD_LOCK_NAME)
    );

    // backing 已删：身份不再可解析，不退到 sysfs 字符串
    std::fs::remove_file(&backing).unwrap();
    assert!(TargetIdentity::loop_backed(&dev, backing.clone()).is_err());
}

/// 同一 backing 上两个 loop 共用一把锁：锁按载体独占，占用域覆盖整盘及其全部分区，
/// 不重叠的区间也互斥。
/// Linux 专属用例；第二次绑定失败即跳过
#[cfg(target_os = "linux")]
#[test]
pub(crate) fn two_loops_over_one_backing_share_the_carrier_lock() {
    struct Bound(PathBuf);
    impl Drop for Bound {
        fn drop(&mut self) {
            // audit-allow: D-15
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
/// 日志命名取字面路径，不因 canonicalize 打断连续性
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
    assert_eq!(
        via_link.record_path(),
        direct.record_path(),
        "both spellings must share one record slot"
    );
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

/// [`read_sysfs_attr`] 的三态必须可分："不存在 ⇒ None"
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

/// 锁按**盘**派生、记录按**目标**派生：两者在块设备上是同一个函数算出来的两个投影，
/// 因此锁落点必然落在 state_dir 下（不引入第二套路径规则）。
/// 非 Linux 跑退化身份（无 sysfs 判据）；Linux 上身份只能来自真实拓扑
/// （见 `block_identity_refuses_unresolvable_topology`）
#[cfg(not(target_os = "linux"))]
#[test]
pub(crate) fn block_lock_lives_in_the_state_dir() {
    let id = TargetIdentity::resolve_block(Path::new("/dev/diskedit-nonexistent")).unwrap();
    assert_eq!(id.lock_path().parent(), Some(state_dir().as_path()));
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

/// 块设备身份取 WWN → EUI → serial 的第一个非空原文；分区号是独立分量；
/// dm / md / 没有身份属性都没有键
#[cfg(target_os = "linux")]
#[test]
pub(crate) fn block_identity_is_wwn_eui_or_serial_and_has_no_key_for_dm_md() {
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
    let (whole, whole_partition) = block_record_context(&sda).unwrap();
    assert_eq!(whole, Some(key_for_test("naa.5000c500aabbccdd", None)));
    assert_eq!(whole_partition, None);
    let (partition, partition_number) = block_record_context(&sda1).unwrap();
    assert_eq!(partition, Some(key_for_test("naa.5000c500aabbccdd", Some(1))));
    assert_eq!(partition_number, Some(1));
    assert_ne!(whole, partition);

    let nvme = make("nvme0n1");
    std::fs::write(nvme.join("wwid"), "eui.0025385812345678\n").unwrap();
    assert_eq!(
        block_record_context(&nvme).unwrap().0,
        Some(key_for_test("eui.0025385812345678", None))
    );

    let serial_only = make("sdb");
    std::fs::create_dir_all(serial_only.join("device")).unwrap();
    std::fs::write(serial_only.join("device/serial"), "SN123456\n").unwrap();
    assert_eq!(
        block_record_context(&serial_only).unwrap().0,
        Some(key_for_test("SN123456", None))
    );

    let dm = make("dm-0/dm");
    std::fs::write(dm.join("uuid"), "mpath-1234\n").unwrap();
    assert!(block_record_context(&root.join("dm-0")).unwrap().0.is_none());

    let md = make("md0/md");
    std::fs::write(md.join("uuid"), "abc:def\n").unwrap();
    assert!(block_record_context(&root.join("md0")).unwrap().0.is_none());

    let bare = make("sdc");
    std::fs::write(bare.join("size"), "2048\n").unwrap();
    assert!(block_record_context(&bare).unwrap().0.is_none());
}

/// 只读位读不出不是"可写"：准入必须在记录落盘之前按 I/O 故障（4）拒绝，
/// 否则只读状态要等第一次 pwrite 才暴露，那时记录已在盘上
#[test]
fn an_unreadable_read_only_bit_refuses_before_any_write() {
    let refused = read_only_verdict(crate::domain::Fact::Undecidable("BLKROGET failed: EIO".into()))
        .expect_err("an unreadable read-only bit must not be taken as writable");
    assert_eq!(refused.exit_code(), crate::outcome::EXIT_NO_MUTATION);
    assert!(refused.to_string().contains("BLKROGET"), "{refused}");

    let read_only = read_only_verdict(crate::domain::Fact::Known(true))
        .expect_err("a read-only block device must be refused");
    assert_eq!(read_only.exit_code(), crate::outcome::EXIT_NO_MUTATION);

    read_only_verdict(crate::domain::Fact::Known(false)).expect("a writable block device passes");
    read_only_verdict(crate::domain::Fact::Absent).expect("a non-block target has no read-only bit");
}

