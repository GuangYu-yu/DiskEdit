use super::*;

/// 重复旗标：值型拒绝（两个取值的旗标语义冲突，last-wins 会让生效值与用户
/// 第一次意图相反且无回显），布尔幂等（`--yes --yes` 与 `--yes` 同一状态，
/// 包装脚本可以无条件追加）。`--help` 让流程止于参数层，不需要真实目标
#[test]
fn duplicate_flags_value_rejected_boolean_idempotent() {
    let exe = env!("CARGO_BIN_EXE_DiskEdit");
    let run = |args: &[&str]| -> (i32, String) {
        let out = Command::new(exe).args(args).output().unwrap();
        (out.status.code().unwrap_or(-1), String::from_utf8_lossy(&out.stderr).into_owned())
    };

    let (c, e) = run(&["delete", "--help", "--yes", "--yes"]);
    assert_eq!(c, 0, "a boolean flag is idempotent: {e}");
    assert!(e.is_empty(), "help goes to stdout: {e}");

    let (c, e) = run(&["resize", "--help", "--chunk", "4", "--chunk", "8"]);
    assert_eq!(c, 2, "a value option cannot be given twice: {e}");
    assert!(e.contains("more than once"), "the rejection must say why: {e}");

    // 布尔与值型的区别取自注册表，同一条规则也覆盖 `--flag=value` 写法
    let (c, e) = run(&["move", "--help", "--start", "2048", "--start=4096"]);
    assert_eq!(c, 2, "{e}");
}

/// `--version` 是主动请求，但不是参数校验的旁路：任何多余旗标或位置参数都拒绝，
/// 已知旗标（`--yes`、`--sector-size 4096`）也不例外
#[test]
fn version_rejects_extra_arguments() {
    let exe = env!("CARGO_BIN_EXE_DiskEdit");
    let run = |args: &[&str]| -> (i32, String, String) {
        let out = Command::new(exe).args(args).output().unwrap();
        (out.status.code().unwrap_or(-1),
         String::from_utf8_lossy(&out.stdout).into_owned(),
         String::from_utf8_lossy(&out.stderr).into_owned())
    };

    let (c, out, e) = run(&["--version"]);
    assert_eq!(c, 0, "{e}");
    assert!(out.contains(env!("CARGO_PKG_VERSION")), "{out}");

    for extra in [vec!["--sector-size", "4096"], vec!["x.bin"], vec!["--yes"]] {
        let mut args = vec!["--version"];
        args.extend(extra.iter().copied());
        let (c, _, e) = run(&args);
        assert_eq!(c, 2, "extra argument must be refused: {extra:?}: {e}");
        assert!(e.contains("`--version` takes no"), "{e}");
    }
}

/// 详助首行必须点名命令自己：首行写错命令名会让 `help <CMD>` 显示别的命令的
/// SYNOPSIS；反向旗标对账只扫首行，该命令自己的旗标因此不被核对
#[test]
fn each_help_names_its_own_command_first() {
    let exe = env!("CARGO_BIN_EXE_DiskEdit");
    let run = |args: &[&str]| -> (i32, String) {
        let out = Command::new(exe).args(args).output().unwrap();
        (out.status.code().unwrap_or(-1), String::from_utf8_lossy(&out.stdout).into_owned())
    };
    for cmd in ["plan", "info", "resize", "move", "copy"] {
        let (c, out) = run(&["help", cmd]);
        assert_eq!(c, 0, "help {cmd} must succeed");
        let first = out.lines().next().unwrap_or("");
        assert!(first.contains(&format!("diskedit {cmd}")), "help {cmd} first line is {first:?}");
    }
}

/// 错误路径走真实二进制：bail_fail() 会退出进程，进程内测不到，故用 subprocess 断言退出码 + 文案
#[cfg(unix)]
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

    // 目标不存在 → 4（写盘前的环境失败），不是"无表"；失败也产出 JSON 信封，
    // 拿不到的字段为 null，原因进 error（调用方不必按故障阶段换一套解析）
    let missing = dir.join("nope.img");
    let (c, out, e) = run(&["info", missing.to_str().unwrap()]);
    assert_eq!(c, 4, "missing target must be a pre-write environment failure: {e}");
    assert!(e.contains("open failed"), "{e}");
    let v: serde_json::Value = serde_json::from_str(out.trim()).expect("open failure must still emit JSON");
    assert!(v["size_bytes"].is_null() && v["error"].is_string(), "{out}");

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
    let (c, _, e) = run(&["resize", &format!("{}:1", no_pmbr.display()), "10M", "--yes"]);
    assert_eq!(c, 3, "resize on damaged-GPT target must refuse: {e}");
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
    // 可恢复的表在只读入口上也应可读，而不是报"表非法"
    let (c, _, e) = run(&["plan", bad_arr.to_str().unwrap()]);
    assert_ne!(c, 4, "a recoverable table must not be reported as unreadable: {e}");

    // 两份副本的条目数组都坏 → 无可救回：表结构非法 = 前置条件不满足 = 3，
    // 且因为写盘前失败，不得附"盘可能已改变"的提示
    let bad2 = dir.join("badarr2.img");
    fixture_gpt_image(&bad2);
    {
        let mut raw = std::fs::read(&bad2).unwrap();
        raw[2 * 512 + 10] ^= 0xFF; // 主条目数组（LBA2）
        raw[67 * 512 + 10] ^= 0xFF; // 备条目数组（末块 99 − 跨度 32 = LBA 67）
        std::fs::write(&bad2, &raw).unwrap();
    }
    let (c, out, e) = run(&["info", bad2.to_str().unwrap()]);
    assert_eq!(c, 3, "both copies damaged must be reported: {e}");
    assert!(e.contains("parse failed"), "{e}");
    // 读表阶段失败：信封与成功路径同键集，未取得的布局字段为 null，原因进 error
    let v: serde_json::Value = serde_json::from_str(out.trim()).expect("info must emit valid JSON on failure too");
    assert!(v["label"].is_null() && v["damaged"].is_null(), "{out}");
    assert!(v["error"].as_str().unwrap_or_default().contains("parse failed"), "{out}");
    let (c, _, e) = run(&["resize", &format!("{}:1", bad2.display()), "10M", "--yes"]);
    assert_eq!(c, 3, "{e}");
    // 同一判据延伸到 plan/check：表结构非法在任何入口都是 3
    let (c, _, e) = run(&["plan", bad2.to_str().unwrap()]);
    assert_eq!(c, 3, "plan on a corrupt table must refuse: {e}");
    assert!(e.contains("GPT entry array"), "{e}");
    assert!(!e.contains("may have changed"), "a pre-write failure must not claim the disk may have changed: {e}");
    let (c, _, e) = run(&["check", &format!("{}:1", bad2.display())]);
    assert_eq!(c, 3, "check on a corrupt table must refuse: {e}");
    assert!(e.contains("GPT entry array"), "{e}");
    // 只读入口对无表目标给出报告；写命令仍按请求与现状不匹配落 3
    let blank = dir.join("blank.img");
    std::fs::write(&blank, vec![0u8; 100 * 512]).unwrap();
    let (c, out, e) = run(&["plan", blank.to_str().unwrap()]);
    assert_eq!(c, 0, "plan reports a table-less target like info: {e}");
    assert!(out.contains("\"label\":\"none\""), "{out}");
    // 顺序：先确认表、再解释选择器 —— 无表目标上写 `:last` 不该换一种错误
    let (c, _, e) = run(&["move", &format!("{}:last", blank.display()), "--start", "2048", "--chunk", "4", "--yes"]);
    assert_eq!(c, 3, "{e}");
    assert!(e.contains("move requires a GPT target"), "{e}");

    // SIZE 语法错误 / 缩到超过当前大小 / 重复给尺寸
    let img = dir.join("ok.img");
    fixture_gpt_image(&img);
    let img_s = img.to_str().unwrap();
    let (c, _, e) = run(&["resize", &format!("{img_s}:1"), "10XB"]);
    assert_eq!(c, 2, "{e}");
    assert!(e.contains("bad SIZE"), "{e}");
    let (c, _, e) = run(&["resize", &format!("{img_s}:1"), "-1T", "--yes"]);
    assert_eq!(c, 3, "{e}");
    assert!(e.contains("exceeds the current size"), "{e}");
    // SIZE 只有位置参数一种给法；`--size` 不是 resize 的选项
    let (c, _, e) = run(&["resize", &format!("{img_s}:1"), "10M", "--size", "4096"]); // cli-surface: refused
    assert_eq!(c, 2, "{e}");
    assert!(e.contains("unknown flag `--size`"), "{e}");
    // --sector-size 非法 → 参数解析阶段拒绝
    let (c, _, _) = run(&["info", img_s, "--sector-size", "abc"]);
    assert_eq!(c, 2);
    let (c, _, e) = run(&["info", img_s, "--sector-size", "513"]);
    assert_eq!(c, 2, "non-power-of-two sector size must be a usage error: {e}");
    assert!(e.contains("power of two"), "{e}");
    // 超过实现边界：文案必须说明这是本工具的实现限制，而 GPT 字段的 UINT32 宽度不是兼容性上限
    let (c, _, e) = run(&["info", img_s, "--sector-size", "131072"]);
    assert_eq!(c, 2, "a size above the implementation limit must be a usage error: {e}");
    assert!(e.contains("implementation limit"), "{e}");
    assert!(e.contains("not a format requirement"), "{e}");
    // 65536 恰是实现边界之内，解析层接受
    let (c, _, e) = run(&["info", img_s, "--sector-size", "65536"]);
    assert_eq!(c, 0, "65536 is inside the implementation limit: {e}");
    // 位置参数超限（CommandSpec.max_pos）：多余参数是用法错误，静默忽略等于吞掉用户的笔误
    let (c, _, e) = run(&["info", img_s, "extra"]);
    assert_eq!(c, 2, "excess positional argument must be a usage error: {e}");
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
/// 三处拒绝都发生在打开目标之前；用例随写命令落在 Linux（见 tests/cli/main.rs）
#[cfg(unix)]
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

    // 形态即模式：:N 是离线形式，尺寸给法只属于挂载点形式
    let (c, _, e) = run(&["resizefs", &format!("{img_s}:1"), "8M", "--yes"]);
    assert_eq!(c, 2, "offline resizefs must refuse a size: {e}");
    assert!(e.contains("mountpoint"), "{e}");
    // 扇区大小只有打开镜像的路径才消费：挂载点形式拒绝
    let (c, _, e) = run(&["resizefs", "some-mountpoint", "--sector-size", "4096", "--yes"]);
    assert_eq!(c, 2, "online resizefs must refuse --sector-size: {e}");
    assert!(e.contains("image targets"), "{e}");
    // set 的 --random 是 uuid 分支专属
    let (c, _, e) = run(&["set", &format!("{img_s}:1"), "name", "foo", "--random", "--yes"]);
    assert_eq!(c, 2, "set --random outside the uuid branch must refuse: {e}");
    assert!(e.contains("uuid"), "{e}");

    let _ = std::fs::remove_dir_all(&dir);
}

/// fail-closed：命令不消费的旗标一律拒绝（而非静默忽略）
#[test]
fn flag_contract_fail_closed() {
    let dir = FixtureDir::new();
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
    let (c, _, e) = run(&["info", img_s, "--yes"]); // cli-surface: refused
    assert_eq!(c, 2, "unconsumed flag must be refused: {e}");
    assert!(e.contains("not a valid option for `info`"), "{e}");
}

/// 值槽里的旗标笔误必须拒绝：`set img:1 name --bogs` 里的 `--bogs` 若被当成位置参数，
/// 就会以 0 退出并把分区名改成一个以 `--` 开头的串（值路径不校验前缀）
#[cfg(unix)]
#[test]
fn a_flag_typo_in_a_value_slot_is_refused() {
    let dir = std::env::temp_dir().join(format!("diskedit_fc3_{}", std::process::id()));
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

    let (c, _, e) = run(&["new", img_s, "--table", "mbr", "--yes"]);
    assert_eq!(c, 0, "{e}");
    let (c, _, e) = run(&["add", img_s, "1M", "--align", "mib", "--yes"]);
    assert_eq!(c, 0, "{e}");
    let before = std::fs::read(&img).unwrap();

    let (c, _, e) = run(&["set", &format!("{img_s}:1"), "name", "--bogs"]); // cli-surface: refused
    assert_eq!(c, 2, "a mistyped flag in a value slot must be refused: {e}");
    assert!(e.contains("unknown flag `--bogs`"), "{e}");
    assert_eq!(std::fs::read(&img).unwrap(), before, "the refusal must not write");

    // 相对尺寸是唯一合法的 `-` 开头值：`resize sf -1M` 不该被当成旗标笔误
    let sf = dir.join("sf.img");
    std::fs::write(&sf, vec![0u8; 8 * 1024 * 1024]).unwrap();
    let (c, _, e) = run(&["resize", sf.to_str().unwrap(), "-1M", "--no-fs", "--yes"]);
    assert!(!e.contains("unknown flag"), "a relative size must not be read as a flag: {e}");
    assert_eq!(c, 3, "superfloppy --no-fs is refused for its own reason: {e}");

    let _ = std::fs::remove_dir_all(&dir);
}

/// `--flag=value`、`--` 终止符与 `--version` 的解析：等号形式与空格形式等价，
/// 布尔旗标带值是笔误，`--` 之后一律位置参数，版本请求退 0
#[cfg(unix)]
#[test]
fn option_syntax_equals_terminator_and_version() {
    let dir = std::env::temp_dir().join(format!("diskedit_fc4_{}", std::process::id()));
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

    let (c, o, e) = run(&["--version"]);
    assert_eq!(c, 0, "{e}");
    assert_eq!(o.trim(), format!("diskedit {}", env!("CARGO_PKG_VERSION")));

    // 两种写法必须走同一条判定，结果（含拒绝理由）逐字相同
    let space = run(&["resize", img_s, "10G", "--chunk", "4"]);
    let equals = run(&["resize", img_s, "10G", "--chunk=4"]);
    assert_eq!(space, equals, "--flag=value must be the same syntax as --flag value");

    let (c, _, e) = run(&["delete", &format!("{img_s}:1"), "--yes=1"]);
    assert_eq!(c, 2, "a value on a bare flag must be refused: {e}");
    assert!(e.contains("takes no value"), "{e}");

    // `--` 之后的值不再按旗标解析：`-1M` 是尺寸而不是笔误
    let (c, _, e) = run(&["resize", img_s, "--no-fs", "--yes", "--", "-1M"]);
    assert_eq!(c, 3, "{e}");
    assert!(!e.contains("unknown flag"), "a token after `--` is positional: {e}");

    let _ = std::fs::remove_dir_all(&dir);
}

/// 其余几条 fail-closed：MBR 不接受 --allow-move、superfloppy 上 --no-fs
/// 无事可做、离线 resizefs 没有"目标尺寸"语义、MBR --type 接受大写 0X 前缀
#[cfg(unix)]
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
    let (c, _, e) = run(&["new", img_s, "--table", "mbr", "--yes"]);
    assert_eq!(c, 0, "{e}");
    let (c, _, e) = run(&["add", img_s, "1M", "--align", "mib", "--yes"]);
    assert_eq!(c, 0, "{e}");
    let (c, _, e) = run(&["resize", &format!("{img_s}:1"), "10M", "--allow-move", "--chunk", "4", "--yes"]);
    assert_eq!(c, 3, "MBR resize with --allow-move must be refused: {e}");
    assert!(e.contains("--allow-move is not supported for MBR"), "{e}");

    // superfloppy 无分区可改：--no-fs 让命令无事可做，拒绝
    let sf = dir.join("sf.img");
    std::fs::write(&sf, vec![0u8; 8 * 1024 * 1024]).unwrap();
    let (c, _, e) = run(&["resize", sf.to_str().unwrap(), "grow", "--no-fs", "--yes"]);
    assert_eq!(c, 3, "superfloppy --no-fs must be refused: {e}");
    assert!(e.contains("--no-fs leaves nothing to do on a superfloppy"), "{e}");

    // 离线 resizefs 没有"目标尺寸"语义：尺寸参数是挂载点形式专属，离线分支消费不到它，
    // 静默忽略会让 `resizefs img:1 10G` 报成功却仍只把 FS 扩进现分区
    let g = dir.join("g.img");
    std::fs::write(&g, vec![0u8; 8 * 1024 * 1024]).unwrap();
    let g_s = g.to_str().unwrap();
    let (c, _, e) = run(&["new", g_s, "--yes"]);
    assert_eq!(c, 0, "{e}");
    let (c, _, e) = run(&["add", g_s, "2M", "--align", "mib", "--type-guid", "0fc63daf-8483-4772-8e79-3d69d8477de4", "--yes"]);
    assert_eq!(c, 0, "{e}");
    let raw_before = std::fs::read(&g).unwrap();
    let (c, _, e) = run(&["resizefs", &format!("{g_s}:1"), "5G", "--yes"]);
    assert_eq!(c, 2, "offline resizefs with a positional size must be refused: {e}");
    assert!(e.contains("only applies to the mountpoint form"), "{e}");
    assert_eq!(std::fs::read(&g).unwrap(), raw_before, "the refusal must not write");

    // MBR --type 的 0X 大写前缀
    let m = dir.join("m.img");
    std::fs::write(&m, vec![0u8; 8 * 1024 * 1024]).unwrap();
    let m_s = m.to_str().unwrap();
    let (c, _, e) = run(&["new", m_s, "--table", "mbr", "--yes"]);
    assert_eq!(c, 0, "{e}");
    // MBR 分区类型字节的大小写前缀由 `set type` 统一解析
    let (c, _, e) = run(&["add", m_s, "1M", "--align", "mib", "--yes"]);
    assert_eq!(c, 0, "{e}");
    let (c, _, e) = run(&["set", &format!("{m_s}:1"), "type", "0X83", "--yes"]);
    assert_eq!(c, 0, "uppercase 0X prefix must be accepted: {e}");
    let (c, out, _) = run(&["info", m_s]);
    assert_eq!(c, 0);
    let v: serde_json::Value = serde_json::from_str(out.trim()).expect("info must emit valid JSON");
    let types: Vec<&str> = v["partitions"].as_array().unwrap().iter().filter_map(|p| p["type"].as_str()).collect();
    assert_eq!(types, ["0x83"], "{out}");

    let _ = std::fs::remove_dir_all(&dir);
}

/// 参数层与只读输出层的三处缺陷：`--help` 的主题、`:0` 分区号、MBR 条目末端算术域
#[cfg(unix)]
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
    assert!(out.contains("diskedit resize <TARGET>[:N]"), "{out}");
    // --help <CMD> 这条写法仍按位置参数取主题
    let (c, out, _) = run(&["--help", "resize"]);
    assert_eq!(c, 0);
    assert!(out.contains("diskedit resize <TARGET>[:N]"), "{out}");
    // 命令位本身是 help 的两种写法同样过旗标白名单：帮助不是参数校验的旁路
    let (c, _, e) = run(&["--help", "info", "--yes"]);
    assert_eq!(c, 2, "help must not accept a flag the command rejects: {e}");
    let (c, _, e) = run(&["help", "info", "--yes"]); // cli-surface: refused
    assert_eq!(c, 2, "help must not accept a flag the command rejects: {e}");
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
    assert_eq!(c, 2, "{err}");
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
    // 条目越盘使 identify 短读：JSON 照常输出（可观察），但退出码如实升级为 4
    assert_eq!(c, 4, "identify failure must be reported honestly: {err}");
    assert!(err.contains("identify failed"), "{err}");
    let v: serde_json::Value = serde_json::from_str(out.trim()).expect("info must emit valid JSON");
    assert_eq!(v["label"], "mbr", "{out}");
    assert_eq!(v["damaged"], true, "the out-of-range entry must be reported as damage: {out}");
    assert_eq!(v["partitions"][0]["first_lba"], 4294967294u64);
    assert_eq!(v["partitions"][0]["last_lba"], 4294967297u64, "must not wrap in u32: {out}");
    // 可观察 ≠ 可操作：同一条目在写路径上必须 fail-closed
    let (c, _, err) = run(&["delete", &format!("{}:1", wrap.display()), "--yes"]);
    assert_ne!(c, 0, "a damaged table must not be written: {err}");
    assert!(err.contains("extends past the end"), "{err}");

    let _ = std::fs::remove_dir_all(&dir);
}

/// 三条"改参数即可解决"的拒绝必须落在 2 或 3，且提示要指向正确的旗标：
/// 未知 FS 名（列出支持的类型）、`resize --start`（未登记 → 2）、
/// `resize-part --start`（未登记 → 2）
#[cfg(unix)]
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

    // 不认得的 FS 名 = 不支持的类型（6），且把支持的类型列出来供改正
    let (c, _, e) = run(&["mkfs", &part, "et4", "--yes"]);
    assert_eq!(c, 6, "unknown fstype must be unsupported: {e}");
    assert!(e.contains("unsupported fstype et4"), "{e}");
    assert!(e.contains("supported: ext2/3/4"), "{e}");

    // resize 只改大小、不搬移：`--start` 未登记，收到即 2
    let (c, _, e) = run(&["resize", &part, "1G", "--start", "5", "--yes"]); // cli-surface: refused
    assert_eq!(c, 2, "{e}");
    assert!(e.contains("--start is not a valid option for `resize`"), "{e}");

    // resize-part 的起点由当前表决定：`--start` 未登记，收到即 2
    let (c, _, e) = run(&["resize-part", &part, "--start", "end", "--yes"]); // cli-surface: refused
    assert_eq!(c, 2, "{e}");
    assert!(e.contains("--start is not a valid option for `resize-part`"), "{e}");

    let _ = std::fs::remove_dir_all(&dir);
}

/// 带参旗标不能把下一个选项吞成自己的值：`--name --align=none` 是缺值，
/// 不是分区名 `--align=none`
#[cfg(unix)]
#[test]
fn a_value_flag_does_not_swallow_the_next_option() {
    let dir = std::env::temp_dir().join(format!("diskedit_swallow_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let exe = env!("CARGO_BIN_EXE_DiskEdit");
    let run = |args: &[&str]| -> (i32, String) {
        let out = Command::new(exe).args(args).output().unwrap();
        (out.status.code().unwrap_or(-1), String::from_utf8_lossy(&out.stderr).into_owned())
    };

    // 目标不存在：参数层在打开目标之前拒绝（2），不会走到 open（4）
    let missing = dir.join("nope.img");
    let (c, e) = run(&["copy", &format!("{}:1", missing.display()), "--start", "1000", "--name", "--align=none"]);
    assert_eq!(c, 2, "a value flag followed by an option is a missing value: {e}");
    assert!(e.contains("--name S requires a value"), "{e}");
    assert!(!e.contains("open failed"), "the refusal must happen before the target is opened: {e}");
    assert!(!missing.exists(), "the refused run must not create the target");

    // 同一形态在能成功执行的目标上：解析层拒绝之后，镜像与伴随文件都不该变化
    let img = dir.join("a.img");
    let ss = 512u64;
    let mut cur = Cursor::new(vec![0u8; 8192 * ss as usize]);
    let mut gpt = gptman::GPT::new_from(&mut cur, ss, [0xAB; 16]).unwrap();
    gpt[1] = gptman::GPTPartitionEntry {
        partition_type_guid: [
            0x6D, 0xFD, 0x57, 0x06, 0xAB, 0xA4, 0xC4, 0x43, 0x84, 0xE5, 0x09, 0x33, 0xC8, 0x4B, 0x4F, 0x4F,
        ],
        unique_partition_guid: [0x02; 16],
        starting_lba: 2048,
        ending_lba: 4095,
        attribute_bits: 0,
        partition_name: "smoke".into(),
    };
    gpt.write_into(&mut cur).unwrap();
    std::fs::write(&img, with_protective_mbr(cur.into_inner())).unwrap();
    let img_s = img.to_str().unwrap();
    let before = std::fs::read(&img).unwrap();
    let entries_before = dir_entries(&dir);

    let (c, e) = run(&["copy", &format!("{img_s}:1"), "--start", "4096", "--name", "--align=none"]);
    assert_eq!(c, 2, "the repro must be refused: {e}");
    assert!(e.contains("--name S requires a value"), "{e}");
    assert_eq!(std::fs::read(&img).unwrap(), before, "the refusal must not write the image");
    assert_eq!(dir_entries(&dir), entries_before, "the refusal must leave no journal/lock/checkpoint");

    // 下一个 token 是布尔旗标时同样是缺值：以 `-` 开头的裸 token 也是选项
    let (c, e) = run(&["add", img_s, "--name", "--yes"]);
    assert_eq!(c, 2, "{e}");
    assert!(e.contains("--name S requires a value"), "{e}");

    let _ = std::fs::remove_dir_all(&dir);
}

#[cfg(unix)]
fn dir_entries(dir: &std::path::Path) -> Vec<std::ffi::OsString> {
    let mut names: Vec<_> = std::fs::read_dir(dir).unwrap().map(|e| e.unwrap().file_name()).collect();
    names.sort();
    names
}

#[path = "flag_probes.rs"]
mod flag_probes;

/// 每条注册旗标的外部行为：两种取值写法都接受，值型重复拒绝（混用写法也拒），
/// 值型缺值拒绝，布尔带值拒绝、重复幂等。
///
/// 探测通道是 `<CMD> ... --help`：帮助在整条命令行解析完、白名单对账之后才输出，
/// 目标参数在它之后才取，所以退出 0 证明的是"参数层接受了这些形状"，与目标是否
/// 存在无关。命令名写错或旗标不在该命令白名单里，也会以 2 退出——探针点名的
/// 命令因此是被验证过的，不是自说自话
#[test]
fn every_registered_flag_parses_as_documented() {
    use flag_probes::{Kind, PROBES};

    let exe = env!("CARGO_BIN_EXE_DiskEdit");
    let run = |args: &[&str]| -> (i32, String) {
        let out = Command::new(exe).args(args).output().unwrap();
        (out.status.code().unwrap_or(-1), String::from_utf8_lossy(&out.stderr).into_owned())
    };

    // 每条旗标独立，按条并行：进程启动是本机最慢的一环，串行跑会把一条本应
    // 秒级完成的用例拖成一分钟
    std::thread::scope(|s| {
        for p in PROBES {
            let run = &run;
            s.spawn(move || {
                let cmd = p.command;
                match p.kind {
                    Kind::Value => {
                        let sample = p.sample.expect("a value probe must carry a sample");
                        let equal = format!("{}={}", p.name, sample);

                        let (c, e) = run(&[cmd, p.name, sample, "--help"]);
                        assert_eq!(c, 0, "`{cmd} {} {sample}` must parse: {e}", p.name);
                        let (c, e) = run(&[cmd, equal.as_str(), "--help"]);
                        assert_eq!(c, 0, "`{cmd} {equal}` must parse: {e}");

                        // 重复：两种写法各试一次，等号写法不能绕过判重
                        let twice_space = [cmd, p.name, sample, p.name, sample, "--help"];
                        let (c, e) = run(&twice_space);
                        assert_eq!(c, 2, "`{}` twice must be refused: {e}", p.name);
                        assert!(e.contains("more than once"), "the rejection must say why: {e}");

                        let twice_mixed = [cmd, p.name, sample, equal.as_str(), "--help"];
                        let (c, e) = run(&twice_mixed);
                        assert_eq!(c, 2, "`{equal}` after `{} {sample}` must be refused: {e}", p.name);
                        assert!(e.contains("more than once"), "the rejection must say why: {e}");

                        let (c, e) = run(&[cmd, p.name]);
                        assert_eq!(c, 2, "`{}` without a value must be refused: {e}", p.name);
                        assert!(e.contains("requires a value"), "the rejection must say why: {e}");
                    }
                    Kind::Bool => {
                        let (c, e) = run(&[cmd, p.name, "--help"]);
                        assert_eq!(c, 0, "`{cmd} {}` must parse: {e}", p.name);
                        let (c, e) = run(&[cmd, p.name, p.name, "--help"]);
                        assert_eq!(c, 0, "`{}` is idempotent: {e}", p.name);

                        let with_value = format!("{}=1", p.name);
                        let (c, e) = run(&[cmd, with_value.as_str(), "--help"]);
                        assert_eq!(c, 2, "`{with_value}` must be refused: {e}");
                        assert!(e.contains("takes no value"), "the rejection must say why: {e}");
                    }
                }
            });
        }
    });
}

/// 缺参数是定向拒绝，不是整份用法：每种缺失都要点名缺的是哪一项，
/// 并指向该命令自己的帮助。退出码仍是 2
#[cfg(unix)]
#[test]
fn missing_arguments_are_named() {
    let dir = FixtureDir::new();
    let img = dir.join("ok.img");
    fixture_gpt_image(&img);
    let img_s = img.to_str().unwrap();
    let exe = env!("CARGO_BIN_EXE_DiskEdit");
    let run = |args: &[&str]| -> (i32, String) {
        let out = Command::new(exe).args(args).output().unwrap();
        (out.status.code().unwrap_or(-1), String::from_utf8_lossy(&out.stderr).into_owned())
    };

    let p = format!("{img_s}:1");
    let cases: [(&[&str], &str); 6] = [
        (&["delete"], "`delete` needs a target"),
        (&["move", p.as_str()], "`move` needs --start"),
        (&["check", img_s], "`check` needs a partition"),
        (&["mkfs", p.as_str()], "`mkfs` needs a filesystem type"),
        (&["set", p.as_str()], "`set` needs a property"),
        (&["resize", p.as_str()], "specify a SIZE"),
    ];
    for (args, needle) in cases {
        let (c, e) = run(args);
        assert_eq!(c, 2, "{args:?} must be a usage error: {e}");
        assert!(e.contains(needle), "{args:?} must say {needle:?}: {e}");
        assert!(!e.contains("usage:"), "{args:?} must not dump the whole usage text: {e}");
    }
}

/// 以 `-` 开头的相对尺寸只在命令声明的 SIZE 槽位被接受：
/// `info -10` 是旗标笔误（2），不是一次打开失败（4）
#[cfg(unix)]
#[test]
fn relative_size_only_in_the_size_slot() {
    let dir = FixtureDir::new();
    let img = dir.join("ok.img");
    fixture_gpt_image(&img);
    let img_s = img.to_str().unwrap();
    let exe = env!("CARGO_BIN_EXE_DiskEdit");
    let run = |args: &[&str]| -> (i32, String) {
        let out = Command::new(exe).args(args).output().unwrap();
        (out.status.code().unwrap_or(-1), String::from_utf8_lossy(&out.stderr).into_owned())
    };

    let (c, e) = run(&["info", "-10"]);
    assert_eq!(c, 2, "a hyphen token outside the SIZE slot must be an unknown flag: {e}");
    assert!(e.contains("unknown flag `-10`"), "{e}");

    // SIZE 槽位照旧接受：`-10` 被当成相对尺寸并按盘上几何换算，报的是尺寸相关的
    // 拒绝，而不是"未知旗标"
    let (c, e) = run(&["resize", &format!("{img_s}:1"), "-10", "--yes"]);
    assert_ne!(c, 2, "the size must have reached the SIZE parse: {e}");
    assert!(!e.contains("unknown flag"), "the size must have reached the SIZE parse: {e}");
    assert!(e.contains("size"), "the rejection must be about the size: {e}");

    // `--` 之后仍是普通位置参数：这是给以 `-` 开头的路径留的出口
    let (c, e) = run(&["info", "--", "-10"]);
    assert_eq!(c, 4, "after `--`, `-10` is a target path: {e}");
}