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

/// 每条会改变目标的命令在非 Linux 上都必须停在统一能力门：退出 30、专属文案、
/// 目标字节不变、目录里没有新增伴随文件。每个用例独立目录，一条被旧实现写坏
/// 不会污染下一个用例的证据
fn assert_refused(cmd: &[&str], dir: &std::path::Path) {
    std::fs::create_dir_all(dir).unwrap();
    let img = dir.join("disk.img");
    writable_gpt(&img);
    let before = std::fs::read(&img).unwrap();
    let entries_before = dir_entries(dir);

    let (code, _out, err) = run(cmd);
    assert_eq!(code, 30, "`{cmd:?}` must stop at the platform gate: {err}");
    assert!(
        err.contains("does not support safe disk writes"),
        "the refusal must be the platform-capability message, not another failure: {err}"
    );
    assert!(err.contains("Nothing was written"), "the refusal must state the no-write promise: {err}");
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
        ("create", &["create"]),
        ("resize", &["resize"]),
        ("move", &["move"]),
        ("copy", &["copy"]),
        ("resize-part", &["resize-part"]),
        ("apply", &["apply"]),
    ];
    for (name, args) in cases {
        let dir = base.join(name.replace('-', "_"));
        let img = dir.join("disk.img");
        let img_s = img.to_string_lossy().into_owned();
        let p = format!("{img_s}:1");
        let mut full: Vec<&str> = args.to_vec();
        match *name {
            "new" => full.extend([img_s.as_str(), "--yes"]),
            "add" => full.extend([img_s.as_str(), "--start", "8192", "--end", "10239"]),
            "delete" => full.extend([p.as_str(), "--yes"]),
            "set" => full.extend([p.as_str(), "name", "renamed"]),
            "create" => full.extend([img_s.as_str(), "--size", "1M"]),
            "resize" => full.extend([p.as_str(), "+1M", "--no-fs"]),
            "move" => full.extend([p.as_str(), "--start", "4096"]),
            "copy" => full.extend([p.as_str(), "--start", "8192"]),
            "resize-part" => full.extend([p.as_str(), "--start", "4096", "--end", "8191", "--no-fs"]),
            "apply" => full.extend([img_s.as_str(), "--grow", "1", "--no-fs"]),
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
            "check" | "resizefs" => full.extend([p.as_str()]),
            "undo" => full.extend([img_s.as_str(), "--yes"]),
            _ => unreachable!(),
        }
        assert_refused(&full, &dir);
    }
    let _ = std::fs::remove_dir_all(&base);
}

/// 回归：busy 文案把 abandon 指为唯一出路，所以那条出路必须在本平台真的可用。
/// 放弃的只是回滚能力：目标字节不变、记录字节原样保留到 `.abandoned`；现场释放后
/// 写盘命令不再被"未完成作业"挡住——本平台上改由能力门拒绝，并说清缺的是平台保证
#[test]
fn abandon_is_admitted_and_releases_the_scene_on_this_platform() {
    let dir = std::env::temp_dir().join(format!("diskedit_gate_ab_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let img = dir.join("disk.img");
    writable_gpt(&img);
    let img_s = img.to_string_lossy().into_owned();
    let journal = dir.join("disk.img.diskedit.journal");
    let abandoned = dir.join("disk.img.diskedit.journal.abandoned");
    let record = b"an unreadable journal left behind";
    std::fs::write(&journal, record).unwrap();
    let before = std::fs::read(&img).unwrap();

    let (code, out, err) = run(&["abandon", &img_s, "--yes"]);
    assert_eq!(code, 0, "abandon must be admitted on this platform: {err}");
    assert!(out.contains("abandoned 1 recovery record(s)"), "{out}");
    assert!(!journal.exists(), "the active name must be released");
    assert_eq!(std::fs::read(&abandoned).unwrap(), record, "the record bytes must be kept");
    assert_eq!(std::fs::read(&img).unwrap(), before, "abandon must not touch target bytes");

    let (code, _, err) = run(&["add", &img_s, "--start", "8192", "--end", "10239"]);
    assert!(!err.contains("owns this target"), "the unfinished operation must be gone: {err}");
    assert_eq!(code, 30, "{err}");
    assert!(
        err.contains("does not support safe disk writes"),
        "what remains missing is the platform's write guarantee, not the recovery scene: {err}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// 只读命令在能力门上照常放行：`info` 与 `plan` 都不改目标、不建 ckpt/journal。
/// `plan` 走 open_target_ro，连锁也不取，所以目录在两次运行前后必须完全一致
#[test]
fn info_and_plan_are_allowed_and_read_only() {
    let dir = std::env::temp_dir().join(format!("diskedit_gate_ro_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let img = dir.join("disk.img");
    writable_gpt(&img);
    let before = std::fs::read(&img).unwrap();
    let entries_before = dir_entries(&dir);
    let img_s = img.to_str().unwrap();

    let (code, out, err) = run(&["info", img_s]);
    assert_eq!(code, 0, "info must remain available: {err}");
    let v: serde_json::Value = serde_json::from_str(out.trim()).expect("info must emit valid JSON");
    assert_eq!(v["label"], "gpt");

    let (code, out, err) = run(&["plan", img_s, "--grow", "1"]);
    assert_eq!(code, 0, "plan must remain available: {err}");
    assert!(!out.is_empty(), "plan must produce a plan: {err}");

    assert_eq!(std::fs::read(&img).unwrap(), before, "info/plan must not modify the image");
    assert_eq!(dir_entries(&dir), entries_before, "info/plan must leave no lock/journal/checkpoint");
    let _ = std::fs::remove_dir_all(&dir);
}
