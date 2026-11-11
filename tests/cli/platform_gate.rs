use super::*;

/// 16 MiB GPT 镜像，分区 1 = 2048..4095（其后到 last_usable 全是空闲）
fn writable_gpt(path: &std::path::Path) {
    let ss = 512u64;
    let mut cur = Cursor::new(vec![0u8; 32768 * ss as usize]);
    let mut gpt = gptman::GPT::new_from(&mut cur, ss, [0xAB; 16]).unwrap();
    gpt[1] = gptman::GPTPartitionEntry {
        partition_type_guid: [0x11; 16],
        unique_partition_guid: [0x02; 16],
        starting_lba: 2048,
        ending_lba: 4095,
        attribute_bits: 0,
        partition_name: "p1".into(),
    };
    gpt.write_into(&mut cur).unwrap();
    std::fs::write(path, with_protective_mbr(cur.into_inner())).unwrap();
}

fn dir_entries(dir: &std::path::Path) -> Vec<std::ffi::OsString> {
    let mut names: Vec<_> = std::fs::read_dir(dir).unwrap().map(|e| e.unwrap().file_name()).collect();
    names.sort();
    names
}

fn run(args: &[&str]) -> (i32, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_DiskEdit")).args(args).output().unwrap();
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

const CAPABILITY_GAP: &str = "needs the";

/// 需要写侧/证据侧原语的命令在非 Linux 上必须停在能力门：退出 6、专属文案、
/// 目标字节不变、目录里没有新增伴随文件。每个用例独立目录，一条用例写坏
/// 不会污染下一个用例的证据
fn assert_refused(cmd: &[&str], dir: &std::path::Path) {
    std::fs::create_dir_all(dir).unwrap();
    let img = dir.join("disk.img");
    writable_gpt(&img);
    let before = std::fs::read(&img).unwrap();
    let entries_before = dir_entries(dir);

    let (code, _out, err) = run(cmd);
    assert_eq!(code, 6, "`{cmd:?}` must stop at the capability gate: {err}");
    assert!(
        err.contains(CAPABILITY_GAP),
        "the refusal must name the missing primitive, not another failure: {err}"
    );
    assert!(
        err.contains(&format!("command `{}`", cmd[0])),
        "the refusal must name the command it refused: {err}"
    );
    assert_eq!(std::fs::read(&img).unwrap(), before, "`{cmd:?}` must not change target bytes");
    assert_eq!(dir_entries(dir), entries_before, "`{cmd:?}` must leave no lock/journal/checkpoint/log");
}

#[test]
fn table_and_data_writers_are_refused_on_this_platform() {
    let base = std::env::temp_dir().join(format!("diskedit_gate_w_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let cases: &[(&str, &[&str])] = &[
        ("new", &["new"]),
        ("add", &["add"]),
        ("delete", &["delete"]),
        ("set", &["set"]),
        ("resize", &["resize"]),
        ("move", &["move"]),
        ("copy", &["copy"]),
        ("resize-part", &["resize-part"]),
    ];
    for (name, args) in cases {
        let dir = base.join(name.replace('-', "_"));
        let img = dir.join("disk.img");
        let img_s = img.to_string_lossy().into_owned();
        let p = format!("{img_s}:1");
        let mut full: Vec<&str> = args.to_vec();
        match *name {
            "new" => full.extend([img_s.as_str(), "--yes"]),
            "add" => full.extend([img_s.as_str(), "1M", "--yes"]),
            "delete" => full.extend([p.as_str(), "--yes"]),
            "set" => full.extend([p.as_str(), "name", "renamed", "--yes"]),
            "resize" => full.extend([p.as_str(), "+1M", "--no-fs", "--yes"]),
            "move" => full.extend([p.as_str(), "--start", "4096", "--chunk", "4", "--yes"]),
            "copy" => full.extend([p.as_str(), "--start", "8192", "--chunk", "4", "--yes"]),
            "resize-part" => full.extend([p.as_str(), "--end", "8191", "--yes"]),
            _ => unreachable!(),
        }
        assert_refused(&full, &dir);
    }
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn external_tool_and_recovery_writers_are_refused_on_this_platform() {
    let base = std::env::temp_dir().join(format!("diskedit_gate_x_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let cases: &[(&str, &[&str])] = &[
        ("mkfs", &["mkfs"]),
        ("check", &["check"]),
        ("resizefs", &["resizefs"]),
        ("undo", &["undo"]),
    ];
    for (name, args) in cases {
        let dir = base.join(name);
        let img = dir.join("disk.img");
        let img_s = img.to_string_lossy().into_owned();
        let p = format!("{img_s}:1");
        let mut full: Vec<&str> = args.to_vec();
        match *name {
            "mkfs" => full.extend([p.as_str(), "ext4", "--yes"]),
            "check" => full.extend([p.as_str()]),
            "resizefs" => full.extend([p.as_str(), "--yes"]),
            "undo" => full.extend([img_s.as_str(), "--yes"]),
            _ => unreachable!(),
        }
        assert_refused(&full, &dir);
    }
    let _ = std::fs::remove_dir_all(&base);
}

/// `abandon` 只改名伴随文件、盘面一个字节不写，但它需要 `CompanionFile`
/// （`AtomicReplace` 非 Linux 不支持），故退 6。记录与目标都必须原样保留
#[test]
fn abandon_is_refused_like_every_other_command_on_this_platform() {
    let dir = std::env::temp_dir().join(format!("diskedit_gate_ab_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let img = dir.join("disk.img");
    writable_gpt(&img);
    let img_s = img.to_string_lossy().into_owned();
    let journal = dir.join("disk.img.diskedit.journal");
    let record = b"an unreadable journal left behind";
    std::fs::write(&journal, record).unwrap();
    let before = std::fs::read(&img).unwrap();

    let (code, _out, err) = run(&["abandon", &img_s]);
    assert_eq!(code, 6, "abandon must stop at the same gate: {err}");
    assert!(err.contains(CAPABILITY_GAP), "{err}");
    assert_eq!(std::fs::read(&journal).unwrap(), record, "the active record must keep its name");
    assert_eq!(std::fs::read(&img).unwrap(), before, "abandon must not touch target bytes");
    let _ = std::fs::remove_dir_all(&dir);
}

/// `info` / `plan` 只声明 `ReadOnly`，
/// 矩阵在「非 Linux × 镜像」这一格放行它们。它们不写目标、不建任何伴随文件，
/// 也不承诺恢复——所谓「看似可恢复的假象」只适用于写/证据命令，那些仍被门拒
#[test]
fn info_and_plan_run_on_images_and_write_nothing() {
    let dir = std::env::temp_dir().join(format!("diskedit_gate_ro_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let img = dir.join("disk.img");
    writable_gpt(&img);
    let before = std::fs::read(&img).unwrap();
    let entries_before = dir_entries(&dir);
    let img_s = img.to_str().unwrap();

    for cmd in [vec!["info", img_s], vec!["plan", img_s]] {
        let (code, out, err) = run(&cmd);
        assert_eq!(code, 0, "`{cmd:?}` is read-only and must be admitted: {err}");
        assert!(!out.is_empty(), "`{cmd:?}` must report the layout: {err}");
    }

    assert_eq!(std::fs::read(&img).unwrap(), before, "info/plan must not modify the image");
    assert_eq!(dir_entries(&dir), entries_before, "info/plan must leave no lock/journal/checkpoint");
    let _ = std::fs::remove_dir_all(&dir);
}

/// 帮助是元命令：`--help` 与 `help` 在参数层直接退出，不经过能力门，
/// 因此在任何平台上都按成功返回；未知主题仍是用法错误
#[test]
fn help_is_not_gated_by_the_platform() {
    for args in [
        vec!["--help"],
        vec!["help"],
        vec!["help", "info"],
        vec!["--help", "info"],
        vec!["info", "--help"],
    ] {
        let (code, out, err) = run(&args);
        assert_eq!(code, 0, "`{args:?}` must succeed on every platform: {err}");
        assert!(!out.is_empty(), "`{args:?}` must print help: {err}");
    }
    let (code, _out, err) = run(&["help", "nosuchcmd"]);
    assert_eq!(code, 2, "an unknown help topic stays a usage error: {err}");
}
