use super::*;

#[test]
fn auto_commands_create_move_resize_set_delete() {
    let dir = std::env::temp_dir().join(format!("diskedit_auto_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let img = dir.join("auto.img");
    // 16 MiB（32768 扇区，可用 34..32734），坐标全部 1MiB 对齐
    std::fs::write(&img, vec![0u8; 16 * 1024 * 1024]).unwrap();
    let exe = env!("CARGO_BIN_EXE_DiskEdit");
    let img_s = img.to_str().unwrap();
    let run = |args: &[&str]| -> (i32, String, String) {
        let out = Command::new(exe).args(args).output().unwrap();
        (out.status.code().unwrap_or(-1),
         String::from_utf8_lossy(&out.stdout).into_owned(),
         String::from_utf8_lossy(&out.stderr).into_owned())
    };

    run(&["new", img_s, "--yes"]);

    // create --size：4MiB → #1 2048..10239
    let (c, out, e) = run(&["create", img_s, "--size", "4194304", "--name", "one"]);
    assert_eq!(c, 0, "{e}{out}");
    // move：右移到 4096..12287（空闲区，自由落点）
    let (c, _, e) = run(&["move", &format!("{img_s}:1"), "--start", "4096"]);
    assert_eq!(c, 0, "move must succeed: {e}");
    // create --size：2MiB → #2 12288..16383
    let (c, _, e) = run(&["create", img_s, "--size", "2097152"]);
    assert_eq!(c, 0, "{e}");

    // resize grow：右侧被 #2 挡 → 无 --allow-move 直接拒绝（不打印计划）
    let (c, out, stderr) = run(&["resize", &format!("{img_s}:1"), "grow"]);
    assert_eq!(c, 10, "blocked grow without --allow-move must refuse");
    assert!(!out.contains("move part 2"), "no plan without --allow-move: {out}");
    assert!(stderr.contains("--allow-move"), "{stderr}");
    // --allow-move 无 --yes：打印搬移计划后拒绝
    let (c, out, stderr) = run(&["resize", &format!("{img_s}:1"), "grow", "--allow-move"]);
    assert_eq!(c, 10, "plan printed but --yes missing must refuse");
    assert!(out.contains("move part 2"), "plan must be printed: {out}");
    assert!(stderr.contains("--yes"), "{stderr}");
    // 带 --allow-move --yes：自动搬移 #2 到尾部并扩 #1（apply 打包到 last_usable=32734）。
    // #1 里没有 FS，扩分区表这件事本身要显式声明——--no-fs 正是"只改表"的入口
    let (c, _, e) = run(&["resize", &format!("{img_s}:1"), "grow", "--allow-move", "--yes", "--no-fs"]);
    assert_eq!(c, 0, "auto-relocate grow must succeed: {e}");
    let mut f = std::fs::File::open(&img).unwrap();
    let gpt = gptman::GPT::find_from(&mut f).unwrap();
    assert_eq!(gpt[1].starting_lba, 4096);
    assert_eq!(gpt[1].ending_lba, 28638);
    assert_eq!(gpt[2].starting_lba, 28639);
    assert_eq!(gpt[2].ending_lba, 32734);
    drop(f);

    // set：改名 + ESP 标志（info 回读验证）
    let (c, _, e) = run(&["set", &format!("{img_s}:1"), "name", "auto"]);
    assert_eq!(c, 0, "{e}");
    let (c, _, e) = run(&["set", &format!("{img_s}:1"), "flag", "esp", "on"]);
    assert_eq!(c, 0, "{e}");
    let (c, out, _) = run(&["info", img_s]);
    assert_eq!(c, 0);
    let v: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(v["partitions"][0]["name"], "auto");
    assert_eq!(v["partitions"][0]["type"], "C12A7328-F81F-11D2-BA4B-00A0C93EC93B");

    // resize 位置 SIZE 收缩：unknown FS 拒绝（fail-fast）
    let (c, _, stderr) = run(&["resize", &format!("{img_s}:1"), "8388608"]);
    assert_eq!(c, 10, "shrink of unknown FS must refuse: {stderr}");
    assert!(stderr.contains("shrink"), "{stderr}");

    // delete + 位置 SIZE 扩容：删 #2 后 #1 扩到 12MiB（4096..28671）；#1 无 FS，同样走 --no-fs
    let (c, _, e) = run(&["delete", &format!("{img_s}:2"), "--yes"]);
    assert_eq!(c, 0, "{e}");
    let (c, _, e) = run(&["resize", &format!("{img_s}:1"), "12582912", "--no-fs"]);
    assert_eq!(c, 0, "grow by SIZE must succeed: {e}");
    let mut f = std::fs::File::open(&img).unwrap();
    let gpt = gptman::GPT::find_from(&mut f).unwrap();
    assert_eq!(gpt[1].starting_lba, 4096);
    assert_eq!(gpt[1].ending_lba, 28671);
    assert_eq!(gpt[2].ending_lba, 0, "partition 2 must be gone");
    drop(f);

    let _ = std::fs::remove_dir_all(&dir);
}