use super::*;

/// 错误路径走真实二进制：bail_fail() 会退出进程，进程内测不到，故用 subprocess 断言退出码 + 文案
#[test]
fn cli_negative_paths() {
    let dir = std::env::temp_dir().join(format!("diskedit_neg_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let exe = env!("CARGO_BIN_EXE_DiskEdit");
    let run = |args: &[&str]| -> (i32, String, String) {
        let out = Command::new(exe).args(args).output().unwrap();
        (out.status.code().unwrap_or(-1),
         String::from_utf8_lossy(&out.stdout).into_owned(),
         String::from_utf8_lossy(&out.stderr).into_owned())
    };

    // 目标不存在 → 30（基础设施失败），不是"无表"
    let missing = dir.join("nope.img");
    let (c, _, e) = run(&["info", missing.to_str().unwrap()]);
    assert_eq!(c, 30, "missing target must be infra failure: {e}");
    assert!(e.contains("open failed"), "{e}");

    // 有 GPT 结构但保护 MBR 缺失 → 盘型可辨、形状受损：报 "gpt (damaged)"，
    // 写命令对非 gpt/msdos 标签一律拒绝（不会被当 superfloppy 整盘扩）
    let no_pmbr = dir.join("nopmbr.img");
    {
        let mut cur = Cursor::new(vec![0u8; 100 * 512]);
        let mut gpt = gptman::GPT::new_from(&mut cur, 512, [0xAB; 16]).unwrap();
        gpt.write_into(&mut cur).unwrap();
        std::fs::write(&no_pmbr, cur.into_inner()).unwrap(); // 故意不补保护 MBR
    }
    let (c, out, _) = run(&["info", no_pmbr.to_str().unwrap()]);
    assert_eq!(c, 0);
    let v: serde_json::Value = serde_json::from_str(out.trim()).expect("info must emit valid JSON");
    assert_eq!(v["label"], "gpt", "{out}");
    assert_eq!(v["damaged"], true, "PMBR-less GPT must surface as damaged: {out}");
    let (c, _, e) = run(&["resize", &format!("{}:1", no_pmbr.display()), "10M"]);
    assert_eq!(c, 10, "resize on damaged-GPT target must refuse: {e}");
    assert!(e.contains("resize requires"), "{e}");

    // 主头 CRC 损坏 → 回退盘尾备份头读取（主备互备，UEFI 2.10 §5.3.2）
    let bad_hdr = dir.join("badhdr.img");
    fixture_gpt_image(&bad_hdr);
    {
        let mut raw = std::fs::read(&bad_hdr).unwrap();
        raw[512 + 16] ^= 0xFF; // HeaderCRC32 字段翻转
        std::fs::write(&bad_hdr, &raw).unwrap();
    }
    let (c, out, _) = run(&["info", bad_hdr.to_str().unwrap()]);
    assert_eq!(c, 0);
    assert!(out.contains("\"label\":\"gpt\""), "must fall back to backup header: {out}");
    assert!(out.contains("smoke"), "partition must be read from backup: {out}");

    // 条目数组 CRC 损坏（仅主副本）→ 用备份副本读回，而非整体失败；
    // 两份数组是分别写入的，这正是 UEFI §5.3.2 主备互备的意义
    let bad_arr = dir.join("badarr.img");
    fixture_gpt_image(&bad_arr);
    {
        let mut raw = std::fs::read(&bad_arr).unwrap();
        raw[2 * 512 + 10] ^= 0xFF; // LBA2 = 主条目数组
        std::fs::write(&bad_arr, &raw).unwrap();
    }
    let (c, out, e) = run(&["info", bad_arr.to_str().unwrap()]);
    assert_eq!(c, 0, "primary array damage must fall back to the backup copy: {e}");
    assert!(out.contains("\"label\":\"gpt\""), "{out}");
    assert!(out.contains("smoke"), "partition must be read from the backup array: {out}");
    assert!(e.contains("recovered from the backup"), "the recovery must be reported: {e}");
    // --grow 1 在同一备份副本上也应可规划（表可读），而不是报"表非法"
    let (c, _, e) = run(&["plan", bad_arr.to_str().unwrap(), "--grow", "1"]);
    assert_ne!(c, 30, "a recoverable table must not be reported as corrupt: {e}");

    // 两份副本的条目数组都坏 → 无可救回：表非法 = 盘内容故障 = 30，
    // 且因为写盘前失败，不得附"盘可能已改变"的提示
    let bad2 = dir.join("badarr2.img");
    fixture_gpt_image(&bad2);
    {
        let mut raw = std::fs::read(&bad2).unwrap();
        raw[2 * 512 + 10] ^= 0xFF; // 主条目数组（LBA2）
        raw[67 * 512 + 10] ^= 0xFF; // 备条目数组（末块 99 − 跨度 32 = LBA 67）
        std::fs::write(&bad2, &raw).unwrap();
    }
    let (c, _, e) = run(&["info", bad2.to_str().unwrap()]);
    assert_eq!(c, 30, "both copies damaged must be reported: {e}");
    assert!(e.contains("parse failed"), "{e}");
    let (c, _, e) = run(&["resize", &format!("{}:1", bad2.display()), "10M"]);
    assert_eq!(c, 30, "{e}");
    // 同一判据延伸到 plan/apply/check：表非法在任何入口都是 30
    let (c, _, e) = run(&["plan", bad2.to_str().unwrap(), "--grow", "1"]);
    assert_eq!(c, 30, "plan on a corrupt table must be infra: {e}");
    assert!(e.contains("parse failed"), "{e}");
    // apply 无 --yes（apply 在锁下按盘上现状重新推导计划后执行，没有第二个确认层）
    let (c, _, e) = run(&["apply", bad2.to_str().unwrap(), "--grow", "1"]);
    assert_eq!(c, 30, "apply on a corrupt table must be infra: {e}");
    assert!(!e.contains("may have changed"), "a pre-write failure must not claim the disk may have changed: {e}");
    let (c, _, e) = run(&["check", &format!("{}:1", bad2.display())]);
    assert_eq!(c, 30, "check on a corrupt table must be infra: {e}");
    assert!(e.contains("parse failed"), "{e}");
    // 无表 = 请求与现状不匹配 = 10（与上面的 30 必须分开）
    let blank = dir.join("blank.img");
    std::fs::write(&blank, vec![0u8; 100 * 512]).unwrap();
    let (c, _, e) = run(&["plan", blank.to_str().unwrap(), "--grow", "1"]);
    assert_eq!(c, 10, "a target without a table must be refused, not infra: {e}");
    // 缺表的拒绝文案出自唯一那处（gpt_policy::require_gpt_geometry），带命令名
    assert!(e.contains("plan requires a GPT target"), "{e}");
    // 顺序：先确认表、再解释选择器 —— 无表目标上写 `:last` 不该换一种错误
    let (c, _, e) = run(&["move", &format!("{}:last", blank.display()), "--start", "2048"]);
    assert_eq!(c, 10, "{e}");
    assert!(e.contains("move requires a GPT target"), "{e}");

    // SIZE 语法错误 / 缩到超过当前大小 / 与 --size 互斥
    let img = dir.join("ok.img");
    fixture_gpt_image(&img);
    let img_s = img.to_str().unwrap();
    let (c, _, e) = run(&["resize", &format!("{img_s}:1"), "10XB"]);
    assert_eq!(c, 10, "{e}");
    assert!(e.contains("bad SIZE"), "{e}");
    let (c, _, e) = run(&["resize", &format!("{img_s}:1"), "-1T"]);
    assert_eq!(c, 10, "{e}");
    assert!(e.contains("exceeds current size"), "{e}");
    let (c, _, e) = run(&["resize", &format!("{img_s}:1"), "10M", "--size", "4096"]);
    assert_eq!(c, 10, "{e}");
    assert!(e.contains("mutually exclusive"), "{e}");
    // --sector-size 非法 → 参数解析阶段拒绝
    let (c, _, _) = run(&["info", img_s, "--sector-size", "abc"]);
    assert_eq!(c, 10);
    let (c, _, e) = run(&["info", img_s, "--sector-size", "300"]);
    assert_eq!(c, 10, "non-power-of-two sector size must be a usage refusal, not Infra: {e}");
    assert!(e.contains("power of two"), "{e}");
    // 位置参数超限（CommandSpec.max_pos）：多余参数是用法错误，静默忽略等于吞掉用户的笔误
    let (c, _, e) = run(&["info", img_s, "extra"]);
    assert_eq!(c, 10, "excess positional argument must be a usage refusal: {e}");
    assert!(e.contains("unexpected extra argument") && e.contains("help"), "refusal must name the excess argument and point at help: {e}");

    // fixed VHD（footer 只在 EOF）→ 提示容器格式与 qemu-nbd 出路，但不解析其内容
    let vhd = dir.join("fixed.vhd");
    {
        let mut data = vec![0u8; 8192];
        data[8192 - 512..].copy_from_slice(&[0u8; 512]);
        data[8192 - 512..8192 - 504].copy_from_slice(b"conectix");
        data[8192 - 512 + 60..8192 - 512 + 64].copy_from_slice(&2u32.to_be_bytes()); // VHD_FIXED
        std::fs::write(&vhd, &data).unwrap();
    }
    let (c, out, e) = run(&["info", vhd.to_str().unwrap()]);
    assert_eq!(c, 0, "{e}");
    assert!(out.contains("\"label\":\"none\""), "{out}");
    assert!(e.contains("VHD"), "container hint must mention VHD: {e}");
    assert!(e.contains("qemu-nbd"), "{e}");

    let _ = std::fs::remove_dir_all(&dir);
}

/// fs 族分支级旗标守卫：白名单是命令级的，分支差异必须显式拒绝而非静默忽略。
/// 三处拒绝都发生在打开目标之前，任何平台上都可验证退出码与文案
#[test]
fn fs_command_branch_level_flag_guards() {
    let dir = std::env::temp_dir().join(format!("diskedit_fsflags_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let img = dir.join("t.img");
    std::fs::write(&img, vec![0u8; 8 * 1024 * 1024]).unwrap();
    let exe = env!("CARGO_BIN_EXE_DiskEdit");
    let img_s = img.to_str().unwrap();
    let run = |args: &[&str]| -> (i32, String, String) {
        let out = Command::new(exe).args(args).output().unwrap();
        (out.status.code().unwrap_or(-1),
         String::from_utf8_lossy(&out.stdout).into_owned(),
         String::from_utf8_lossy(&out.stderr).into_owned())
    };

    // 在线 resizefs 拿 <target>:N ⇒ 它只收挂载点
    let (c, _, e) = run(&["resizefs", &format!("{img_s}:1"), "--online"]);
    assert_eq!(c, 10, "online resizefs must refuse a :N target: {e}");
    assert!(e.contains("mountpoint"), "{e}");
    // 在线 resizefs 携 --sector-size ⇒ 该旗标的消费点都在打开路径上
    let (c, _, e) = run(&["resizefs", "some-mountpoint", "--online", "--sector-size", "4096"]);
    assert_eq!(c, 10, "online resizefs must refuse --sector-size: {e}");
    assert!(e.contains("mountpoint"), "{e}");
    // set 的 --random 是 uuid 分支专属
    let (c, _, e) = run(&["set", &format!("{img_s}:1"), "name", "foo", "--random"]);
    assert_eq!(c, 10, "set --random outside the uuid branch must refuse: {e}");
    assert!(e.contains("uuid"), "{e}");

    let _ = std::fs::remove_dir_all(&dir);
}

/// fail-closed 参数契约：命令不消费的旗标一律拒绝（而非静默忽略）
#[test]
fn flag_contract_fail_closed() {
    let dir = std::env::temp_dir().join(format!("diskedit_fc_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let exe = env!("CARGO_BIN_EXE_DiskEdit");
    let run = |args: &[&str]| -> (i32, String, String) {
        let out = Command::new(exe).args(args).output().unwrap();
        (out.status.code().unwrap_or(-1),
         String::from_utf8_lossy(&out.stdout).into_owned(),
         String::from_utf8_lossy(&out.stderr).into_owned())
    };

    // info 不消费 --yes：必须拒绝而非忽略
    let img = dir.join("a.img");
    std::fs::write(&img, vec![0u8; 8 * 1024 * 1024]).unwrap();
    let img_s = img.to_str().unwrap();
    let (c, _, e) = run(&["info", img_s, "--yes"]);
    assert_eq!(c, 10, "unconsumed flag must be refused: {e}");
    assert!(e.contains("not a valid option for `info`"), "{e}");
}

/// 旗标契约的其余几条 fail-closed：MBR 不接受 --allow-move、superfloppy 上 --no-fs
/// 无事可做、离线 resizefs 没有"目标尺寸"语义、MBR --type 接受大写 0X 前缀
#[test]
fn flag_contract_mbr_resizefs_and_type() {
    let dir = std::env::temp_dir().join(format!("diskedit_fc2_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let exe = env!("CARGO_BIN_EXE_DiskEdit");
    let img = dir.join("a.img");
    std::fs::write(&img, vec![0u8; 8 * 1024 * 1024]).unwrap();
    let img_s = img.to_str().unwrap();
    let run = |args: &[&str]| -> (i32, String, String) {
        let out = Command::new(exe).args(args).output().unwrap();
        (out.status.code().unwrap_or(-1),
         String::from_utf8_lossy(&out.stdout).into_owned(),
         String::from_utf8_lossy(&out.stderr).into_owned())
    };

    // MBR resize 不支持 --allow-move：显式拒绝，而非"空间不足"误导
    let (c, _, e) = run(&["new", img_s, "--table", "msdos", "--yes"]);
    assert_eq!(c, 0, "{e}");
    let (c, _, e) = run(&["add", img_s, "--start", "2048", "--end", "4095"]);
    assert_eq!(c, 0, "{e}");
    let (c, _, e) = run(&["resize", &format!("{img_s}:1"), "10M", "--allow-move"]);
    assert_eq!(c, 10, "MBR resize with --allow-move must be refused: {e}");
    assert!(e.contains("--allow-move is not supported for MBR"), "{e}");

    // superfloppy 无分区可改：--no-fs 让命令无事可做，拒绝
    let sf = dir.join("sf.img");
    std::fs::write(&sf, vec![0u8; 8 * 1024 * 1024]).unwrap();
    let (c, _, e) = run(&["resize", sf.to_str().unwrap(), "--no-fs"]);
    assert_eq!(c, 10, "superfloppy --no-fs must be refused: {e}");
    assert!(e.contains("--no-fs leaves nothing to do on a superfloppy"), "{e}");

    // 离线 resizefs 没有"目标尺寸"语义：--size 拒绝
    let g = dir.join("g.img");
    std::fs::write(&g, vec![0u8; 8 * 1024 * 1024]).unwrap();
    let g_s = g.to_str().unwrap();
    let (c, _, e) = run(&["new", g_s, "--yes"]);
    assert_eq!(c, 0, "{e}");
    let (c, _, e) = run(&["add", g_s, "--start", "2048", "--end", "6143"]);
    assert_eq!(c, 0, "{e}");
    let (c, _, e) = run(&["resizefs", &format!("{g_s}:1"), "--size", "5G"]);
    assert_eq!(c, 10, "offline resizefs with --size must be refused: {e}");
    assert!(e.contains("--size only applies to the online form"), "{e}");
    // 位置参数形式的尺寸同属在线专属：离线分支消费不到它，静默忽略会让
    // `resizefs img:1 10G` 报成功却仍只把 FS 扩进现分区
    let raw_before = std::fs::read(&g).unwrap();
    let (c, _, e) = run(&["resizefs", &format!("{g_s}:1"), "5G"]);
    assert_eq!(c, 10, "offline resizefs with a positional size must be refused: {e}");
    assert!(e.contains("only applies to the online form"), "{e}");
    assert_eq!(std::fs::read(&g).unwrap(), raw_before, "the refusal must not write");

    // MBR --type 的 0X 大写前缀
    let m = dir.join("m.img");
    std::fs::write(&m, vec![0u8; 8 * 1024 * 1024]).unwrap();
    let m_s = m.to_str().unwrap();
    let (c, _, e) = run(&["new", m_s, "--table", "msdos", "--yes"]);
    assert_eq!(c, 0, "{e}");
    let (c, _, e) = run(&["add", m_s, "--start", "2048", "--end", "4095", "--type", "0X83"]);
    assert_eq!(c, 0, "uppercase 0X prefix must be accepted: {e}");
    let (c, out, _) = run(&["info", m_s]);
    assert_eq!(c, 0);
    let v: serde_json::Value = serde_json::from_str(out.trim()).expect("info must emit valid JSON");
    let types: Vec<&str> = v["partitions"].as_array().unwrap().iter().filter_map(|p| p["type"].as_str()).collect();
    assert_eq!(types, ["0x83"], "{out}");

    let _ = std::fs::remove_dir_all(&dir);
}

/// 参数层与只读输出层的三处缺陷：`--help` 的主题、`:0` 分区号、MBR 条目末端算术域
#[test]
fn help_topic_zero_partition_and_mbr_end_overflow() {
    let dir = std::env::temp_dir().join(format!("diskedit_parse_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let exe = env!("CARGO_BIN_EXE_DiskEdit");
    let run = |args: &[&str]| -> (i32, String, String) {
        let out = Command::new(exe).args(args).output().unwrap();
        (out.status.code().unwrap_or(-1),
         String::from_utf8_lossy(&out.stdout).into_owned(),
         String::from_utf8_lossy(&out.stderr).into_owned())
    };

    // <CMD> --help 的主题是命令名；位置参数是目标，取它只会退化成顶层 usage
    let (c, out, _) = run(&["resize", "whatever.img:1", "--help"]);
    assert_eq!(c, 0, "per-command help must be selected by the command name: {out}");
    assert!(out.contains("diskedit resize <TARGET>:N"), "{out}");
    // --help <CMD> 这条写法仍按位置参数取主题
    let (c, out, _) = run(&["--help", "resize"]);
    assert_eq!(c, 0);
    assert!(out.contains("diskedit resize <TARGET>:N"), "{out}");
    // copy 没有 checkpoint，中断后重跑从头抄：帮助文本必须如实声明，
    // 否则用户会以为它能像 move 一样续传
    let (c, out, _) = run(&["copy", "whatever.img:1", "--help"]);
    assert_eq!(c, 0);
    assert!(out.contains("No resume"), "{out}");

    // `:0` 不是合法分区号：拒绝而不是静默当整盘
    let blank = dir.join("blank.img");
    std::fs::write(&blank, vec![0u8; 2 * 1024 * 1024]).unwrap();
    let blank_s = blank.to_str().unwrap();
    let (c, _, err) = run(&["info", &format!("{blank_s}:0")]);
    assert_eq!(c, 10, "{err}");
    assert!(err.contains("1-based"), "{err}");

    // MBR 条目末端 = start + size − 1：两个 u32 相加必须在 u64 域算，
    // 否则 start 接近 u32::MAX 时 debug 下 panic、release 下回绕成小于起点的 last_lba。
    // 这类越盘条目属"表已损坏"：观察路径（info）照常可看并标注 damaged，
    // 写路径（parse_mbr 的校验侧）则必须拒绝
    let mut data = vec![0u8; 2 * 1024 * 1024];
    data[446 + 4] = 0x83;
    data[446 + 8..446 + 12].copy_from_slice(&0xFFFF_FFFEu32.to_le_bytes());
    data[446 + 12..446 + 16].copy_from_slice(&4u32.to_le_bytes());
    data[510] = 0x55;
    data[511] = 0xAA;
    let wrap = dir.join("wrap.img");
    std::fs::write(&wrap, &data).unwrap();
    let (c, out, err) = run(&["info", wrap.to_str().unwrap()]);
    // 条目越盘使 identify 短读：JSON 照常输出（可观察），但退出码如实升级为 30
    assert_eq!(c, 30, "identify failure must be reported honestly: {err}");
    assert!(err.contains("identify failed"), "{err}");
    let v: serde_json::Value = serde_json::from_str(out.trim()).expect("info must emit valid JSON");
    assert_eq!(v["label"], "mbr", "{out}");
    assert_eq!(v["damaged"], true, "the out-of-range entry must be reported as damage: {out}");
    assert_eq!(v["partitions"][0]["first_lba"], 4294967294u64);
    assert_eq!(v["partitions"][0]["last_lba"], 4294967297u64, "must not wrap in u32: {out}");
    // 可观察 ≠ 可操作：同一条目在写路径上必须 fail-closed
    let (c, _, err) = run(&["del", &format!("{}:1", wrap.display()), "--yes"]);
    assert_ne!(c, 0, "a damaged table must not be written: {err}");
    assert!(err.contains("extends past the end"), "{err}");

    let _ = std::fs::remove_dir_all(&dir);
}

/// 三条"改参数即可解决"的拒绝必须落在 10，且提示要指向正确的旗标：
/// 未知 FS 名（列出支持的类型）、`resize --start`（指向 move/resize-part）、
/// `resize-part --start end`（指向 --grow-to-end）
#[test]
fn targeted_refusals_are_reported_as_ten() {
    let dir = std::env::temp_dir().join(format!("diskedit_refuse_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let exe = env!("CARGO_BIN_EXE_DiskEdit");
    let run = |args: &[&str]| -> (i32, String, String) {
        let out = Command::new(exe).args(args).output().unwrap();
        (out.status.code().unwrap_or(-1),
         String::from_utf8_lossy(&out.stdout).into_owned(),
         String::from_utf8_lossy(&out.stderr).into_owned())
    };

    let img = dir.join("t.img");
    fixture_gpt_image(&img);
    let img_s = img.to_str().unwrap();
    let part = format!("{img_s}:1");

    // 不认得的 FS 名是"参数写错了"：必须 10，且把支持的类型列出来供改正
    let (c, _, e) = run(&["mkfs", &part, "et4", "--yes"]);
    assert_eq!(c, 10, "unknown fstype must be refused, not reported as infra: {e}");
    assert!(e.contains("unsupported fstype et4"), "{e}");
    assert!(e.contains("supported: ext2/3/4"), "{e}");

    // resize 只改大小、不搬移：--start 进白名单后由本命令给出有指向性的拒绝
    let (c, _, e) = run(&["resize", &part, "--start", "5"]);
    assert_eq!(c, 10, "{e}");
    assert!(e.contains("does not relocate"), "{e}");

    // resize-part 的 `--start end` 是 move/copy 的尾部打包语法：提示改用 --grow-to-end
    let (c, _, e) = run(&["resize-part", &part, "--start", "end"]);
    assert_eq!(c, 10, "{e}");
    assert!(e.contains("--grow-to-end"), "{e}");

    let _ = std::fs::remove_dir_all(&dir);
}