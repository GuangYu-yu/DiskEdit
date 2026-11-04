use super::*;

/// 每扇区内容由扇区号与字节位共同决定：搬移错位、只搬首扇区或漏块都会与期望不等
fn marker_range(first_lba: u64, sectors: u64) -> Vec<u8> {
    let mut v = Vec::with_capacity((sectors * 512) as usize);
    for lba in first_lba..first_lba + sectors {
        let mut s = [0u8; 512];
        for (j, b) in s.iter_mut().enumerate() {
            *b = (lba as u8).wrapping_mul(37).wrapping_add(j as u8);
        }
        v.extend_from_slice(&s);
    }
    v
}

fn read_region(img: &std::path::Path, lba: u64, len: usize) -> Vec<u8> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(img).unwrap();
    f.seek(SeekFrom::Start(lba * 512)).unwrap();
    let mut v = vec![0u8; len];
    f.read_exact(&mut v).unwrap();
    v
}

fn write_region(img: &std::path::Path, lba: u64, bytes: &[u8]) {
    use std::io::{Seek, SeekFrom, Write};
    let mut f = std::fs::OpenOptions::new().write(true).open(img).unwrap();
    f.seek(SeekFrom::Start(lba * 512)).unwrap();
    f.write_all(bytes).unwrap();
}

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
    // #1 的整区载荷（8192 扇区，自重叠右移：源与目标区间相交）
    let one_payload = marker_range(2048, 8192);
    write_region(&img, 2048, &one_payload);
    // move：右移到 4096..12287（空闲区，自由落点）
    let (c, _, e) = run(&["move", &format!("{img_s}:1"), "--start", "4096", "--yes"]);
    assert_eq!(c, 0, "move must succeed: {e}");
    assert_eq!(
        read_region(&img, 4096, one_payload.len()),
        one_payload,
        "the moved partition must carry every source byte to its new range"
    );
    // 右移后源区头部 2048..4095 位于目标之外，工具不擦源：内容仍是搬移前的字节
    assert_eq!(
        read_region(&img, 2048, 2048 * 512),
        one_payload[..2048 * 512],
        "the vacated head must still hold the pre-move bytes (the tool does not scrub the source)"
    );
    // create --size：2MiB → #2 12288..16383
    let (c, _, e) = run(&["create", img_s, "--size", "2097152"]);
    assert_eq!(c, 0, "{e}");
    // #2 的整区载荷：扩容时它作为挡路者被搬到尾部，字节必须原样跟随
    let two_payload = marker_range(12288, 4096);
    write_region(&img, 12288, &two_payload);

    // resize grow：右侧被 #2 挡 → 无 --allow-move 直接拒绝（不打印计划）。
    // --yes 只让确认门让路：这里要测的是 --allow-move 这道搬移授权，不是确认门
    let img_before = std::fs::read(&img).unwrap();
    let entries_before = dir_entries(&dir);
    let (c, out, stderr) = run(&["resize", &format!("{img_s}:1"), "grow", "--yes"]);
    assert_eq!(c, 10, "blocked grow without --allow-move must refuse");
    assert!(!out.contains("move part 2"), "no plan without --allow-move: {out}");
    assert!(stderr.contains("--allow-move"), "{stderr}");
    assert_eq!(std::fs::read(&img).unwrap(), img_before, "a refusal must leave the image byte-identical");
    assert_eq!(dir_entries(&dir), entries_before, "a refusal must not add sidecars");
    // --allow-move 无 --yes：打印搬移计划后拒绝（这一条测的就是确认门本身）
    let img_before = std::fs::read(&img).unwrap();
    let entries_before = dir_entries(&dir);
    let (c, out, stderr) = run(&["resize", &format!("{img_s}:1"), "grow", "--allow-move"]);
    assert_eq!(c, 10, "plan printed but --yes missing must refuse");
    assert!(out.contains("move part 2"), "plan must be printed: {out}");
    assert!(stderr.contains("--yes"), "{stderr}");
    assert_eq!(std::fs::read(&img).unwrap(), img_before, "a refusal must leave the image byte-identical");
    assert_eq!(dir_entries(&dir), entries_before, "a refusal must not add sidecars");
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
    drop(gpt);
    drop(f);
    // 搬移落点逐字节等于搬移前；原区间未被清零（工具不擦源，仍是搬移前的字节）
    assert_eq!(
        read_region(&img, 28639, two_payload.len()),
        two_payload,
        "the auto-relocated partition must carry every source byte to the tail"
    );
    assert_eq!(
        read_region(&img, 12288, two_payload.len()),
        two_payload,
        "the vacated range must still hold the pre-move bytes"
    );
    // 扩容新增的空闲区（16384..28638）不得被搬移过程写入
    assert!(
        read_region(&img, 16384, (28639 - 16384) * 512).iter().all(|&b| b == 0),
        "the newly grown range must stay zero: relocation writes only the destination"
    );

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
    let (c, _, stderr) = run(&["resize", &format!("{img_s}:1"), "8388608", "--yes"]);
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