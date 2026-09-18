#![allow(clippy::let_underscore_must_use)] // 测试的清理步骤有意忽略失败（临时目录/文件）
//! 端到端：gptman 造 GPT 镜像 → 真实二进制 `info` 读回 → JSON 含预期字段。

use std::io::Cursor;
use std::process::Command;

fn fixture_gpt_image(path: &std::path::Path) {
    let ss = 512u64;
    let data = vec![0u8; 100 * ss as usize];
    let mut cur = Cursor::new(data);
    let mut gpt = gptman::GPT::new_from(&mut cur, ss, [0xAB; 16]).unwrap();
    gpt[1] = gptman::GPTPartitionEntry {
        partition_type_guid: [
            0x6D, 0xFD, 0x57, 0x06, 0xAB, 0xA4, 0xC4, 0x43, 0x84, 0xE5, 0x09, 0x33, 0xC8, 0x4B, 0x4F, 0x4F,
        ], // linux swap（混合端磁盘序）
        unique_partition_guid: [0x02; 16],
        starting_lba: 34,
        ending_lba: 60,
        attribute_bits: 0,
        partition_name: "smoke".into(),
    };
    gpt.write_into(&mut cur).unwrap();
    std::fs::write(path, with_protective_mbr(cur.into_inner())).unwrap();
}

/// 补保护 MBR（LBA0 槽位 1 = 0xEE、StartLBA = 1，签名 0x55AA）：
/// gptman 只写 GPT 结构，而 load_gpt 以保护 MBR 为判定前置（UEFI 2.10 §5.2.3）
fn with_protective_mbr(mut data: Vec<u8>) -> Vec<u8> {
    let total = data.len() as u64 / 512;
    let size: u32 = if total > u32::MAX as u64 { u32::MAX } else { (total - 1) as u32 };
    data[446 + 4] = 0xEE;
    data[446 + 8..446 + 12].copy_from_slice(&1u32.to_le_bytes());
    data[446 + 12..446 + 16].copy_from_slice(&size.to_le_bytes());
    data[510] = 0x55;
    data[511] = 0xAA;
    data
}

#[test]
fn new_add_del_roundtrip() {
    let dir = std::env::temp_dir().join(format!("diskedit_nad_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let img = dir.join("blank.img");
    // 8 MiB 镜像（16384 扇区，可用区 34..16350），坐标全部取 1MiB 对齐值
    let data = vec![0u8; 8 * 1024 * 1024];
    std::fs::write(&img, &data).unwrap();
    let exe = env!("CARGO_BIN_EXE_DiskEdit");
    let img_s = img.to_str().unwrap();
    let run = |args: &[&str]| -> (i32, String, String) {
        let out = Command::new(exe).args(args).output().unwrap();
        (out.status.code().unwrap_or(-1),
         String::from_utf8_lossy(&out.stdout).into_owned(),
         String::from_utf8_lossy(&out.stderr).into_owned())
    };

    // new：无 --yes 拒绝；带 --yes 成功
    let (c, _, _) = run(&["new", img_s]);
    assert_eq!(c, 10, "new without --yes must refuse");
    let (c, _, _) = run(&["new", img_s, "--yes"]);
    assert_eq!(c, 0, "new with --yes must succeed");

    // add：加两个 1MiB 对齐的分区
    let (c, out, _) = run(&["add", img_s, "--start", "2048", "--end", "4095", "--name", "root"]);
    assert_eq!(c, 0, "add #1: {out}");
    let (c, out, _) = run(&["add", img_s, "--start", "4096", "--end", "6143", "--name", "home"]);
    assert_eq!(c, 0, "add #2: {out}");

    // add：重叠拒绝（未对齐坐标 2049..6143 对齐后 4096..6143，与 home 撞）
    let (c, _, stderr) = run(&["add", img_s, "--start", "2049", "--end", "6143"]);
    assert_eq!(c, 10, "overlapping add must refuse");
    assert!(stderr.contains("overlaps"), "{stderr}");

    // add：越界拒绝（14336 已对齐；对齐后 end 16383 > last_usable 16350）
    let (c, _, stderr) = run(&["add", img_s, "--start", "14336", "--end", "16400"]);
    assert_eq!(c, 10, "out-of-usable-range add must refuse");
    assert!(stderr.contains("outside usable"), "{stderr}");

    // del：删除 #1，info 不再含 root（JSON 结构化断言，非子串匹配）
    let (c, _, _) = run(&["del", &format!("{img_s}:1"), "--yes"]);
    assert_eq!(c, 0, "del must succeed");
    let (c, out, _) = run(&["info", img_s]);
    assert_eq!(c, 0);
    let v: serde_json::Value = serde_json::from_str(out.trim()).expect("info must emit valid JSON");
    assert_eq!(v["label"], "gpt");
    let names: Vec<&str> = v["partitions"].as_array().unwrap().iter().filter_map(|p| p["name"].as_str()).collect();
    assert!(!names.contains(&"root"), "root must be gone: {out}");
    assert!(names.contains(&"home"), "home must remain: {out}");

    // gptman 终验：磁盘字节序状态可被规范工具读取
    let mut f = std::fs::File::open(&img).unwrap();
    let gpt = gptman::GPT::find_from(&mut f).expect("gptman must accept final state");
    let p2 = &gpt[2];
    assert_eq!(p2.starting_lba, 4096);
    assert_eq!(p2.partition_name.as_str(), "home");

    let _ = std::fs::remove_dir_all(&dir);
}

/// 主头撕裂（torn write）时回退盘尾备份头，并在写入时重建主头
#[test]
fn backup_header_fallback_and_repair() {
    let dir = std::env::temp_dir().join(format!("diskedit_bk_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let img = dir.join("b.img");
    std::fs::write(&img, vec![0u8; 16 * 1024 * 1024]).unwrap();
    let exe = env!("CARGO_BIN_EXE_DiskEdit");
    let img_s = img.to_str().unwrap();
    let run = |args: &[&str]| -> (i32, String, String) {
        let out = Command::new(exe).args(args).output().unwrap();
        (out.status.code().unwrap_or(-1),
         String::from_utf8_lossy(&out.stdout).into_owned(),
         String::from_utf8_lossy(&out.stderr).into_owned())
    };
    // 覆写某个扇区（模拟撕裂/清零）
    let wipe = |lba: u64| {
        use std::io::{Seek, SeekFrom, Write};
        let mut f = std::fs::OpenOptions::new().write(true).open(&img).unwrap();
        f.seek(SeekFrom::Start(lba * 512)).unwrap();
        f.write_all(&[0u8; 512]).unwrap();
    };
    let part_count = || -> usize {
        let out = Command::new(exe).args(["info", img_s]).output().unwrap();
        let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        v["partitions"].as_array().map(|a| a.len()).unwrap_or(0)
    };

    let (c, _, e) = run(&["new", img_s, "--yes"]);
    assert_eq!(c, 0, "{e}");
    let (c, _, e) = run(&["add", img_s, "--start", "2048", "--end", "6143", "--name", "a"]);
    assert_eq!(c, 0, "{e}");
    let (c, _, e) = run(&["add", img_s, "--start", "8192", "--end", "12287", "--name", "b"]);
    assert_eq!(c, 0, "{e}");

    // 主头（LBA1）清零 → 必须回退盘尾备份头读到表
    wipe(1);
    let (c, out, e) = run(&["info", img_s]);
    assert_eq!(c, 0, "{e}");
    let v: serde_json::Value = serde_json::from_str(out.trim()).expect("info must emit JSON");
    assert_eq!(v["label"], "gpt", "must fall back to backup header: {out}");
    assert_eq!(part_count(), 2, "partitions must be read from backup: {out}");

    // 写入路径应借修复重建主头
    let (c, _, e) = run(&["set", &format!("{img_s}:1"), "name", "renamed"]);
    assert_eq!(c, 0, "write must succeed and repair primary: {e}");
    let mut f = std::fs::File::open(&img).unwrap();
    let gpt = gptman::GPT::find_from(&mut f).expect("primary header must be rebuilt");
    assert_eq!(gpt[1].partition_name.as_str(), "renamed");

    // 两份都清零 → 视为无表（不误判）
    wipe(1);
    let last = std::fs::metadata(&img).unwrap().len() / 512 - 1;
    wipe(last);
    let (c, out, _) = run(&["info", img_s]);
    assert_eq!(c, 0);
    let v: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(v["label"], "none", "both copies gone must read as none: {out}");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn journal_lifecycle_and_table_undo() {
    let dir = std::env::temp_dir().join(format!("diskedit_jl_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let img = dir.join("j.img");
    std::fs::write(&img, vec![0u8; 8 * 1024 * 1024]).unwrap();
    let exe = env!("CARGO_BIN_EXE_DiskEdit");
    let img_s = img.to_str().unwrap();
    let journal = dir.join("j.img.diskedit.journal");
    let run = |args: &[&str]| -> (i32, String) {
        let out = Command::new(exe).args(args).output().unwrap();
        (out.status.code().unwrap_or(-1), String::from_utf8_lossy(&out.stderr).into_owned())
    };
    let part_count = || -> usize {
        let out = Command::new(exe).args(["info", img_s]).output().unwrap();
        let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        v["partitions"].as_array().unwrap().len()
    };

    // 成功完成的破坏性命令 ⇒ journal 删除（撤销窗口关闭，不留残留）
    let (c, e) = run(&["new", img_s, "--yes"]);
    assert_eq!(c, 0, "{e}");
    assert!(!journal.exists(), "journal must be dropped after a successful new");
    let (c, e) = run(&["add", img_s, "--start", "2048", "--end", "4095", "--name", "x"]);
    assert_eq!(c, 0, "{e}");
    assert!(!journal.exists(), "journal must be dropped after a successful add");
    let (c, e) = run(&["set", &format!("{img_s}:1"), "flag", "esp", "on"]);
    assert_eq!(c, 0, "{e}");
    assert!(!journal.exists(), "journal must be dropped after a successful set");

    // 只读命令不得触碰（也不得误删）journal
    let (c, _) = run(&["info", img_s]);
    assert_eq!(c, 0);
    assert!(!journal.exists(), "read-only commands must not create a journal");

    // 部分失败（mkfs 工具不存在）⇒ journal 保留，供 undo 撤销半成品
    let before = part_count();
    let (c, e) = run(&["create", img_s, "--size", "1M", "--fs", "no-such-fs-type"]);
    assert_eq!(c, 20, "partition created but mkfs failed must be EXIT_PARTIAL: {e}");
    assert!(journal.exists(), "journal must be kept after a partial failure");
    assert_eq!(part_count(), before + 1, "partition should exist before undo");

    // undo 回滚表操作：分区消失，journal 清除
    let (c, e) = run(&["undo", img_s, "--yes"]);
    assert_eq!(c, 0, "undo of a table-only journal must succeed: {e}");
    assert!(!journal.exists(), "journal must be removed after undo");
    assert_eq!(part_count(), before, "undone partition must be gone");

    // journal 尾部未完成（截断 / 未写完的记录头）⇒ 那是"未完成的事务"而非损坏：
    // 记录先于写入落盘，故那条记录对应的写入根本没发生，丢弃它安全；完整前缀照常回放
    let (c, e) = run(&["create", img_s, "--size", "1M", "--fs", "no-such-fs-type"]);
    assert_eq!(c, 20, "{e}");
    let jb = std::fs::read(&journal).unwrap();
    std::fs::write(&journal, &jb[..jb.len() - 2]).unwrap();
    let (c, e) = run(&["undo", img_s, "--yes"]);
    assert_eq!(c, 0, "a truncated tail must be treated as an unfinished append: {e}");
    assert_eq!(part_count(), before, "the complete prefix must still be replayed");
    assert!(!journal.exists(), "journal must be removed after a successful undo");

    // 尾部垃圾 = 未写完的记录头，同样按未完成处理
    let (c, e) = run(&["create", img_s, "--size", "1M", "--fs", "no-such-fs-type"]);
    assert_eq!(c, 20, "{e}");
    let jb = std::fs::read(&journal).unwrap();
    let mut tail = jb.clone();
    tail.extend_from_slice(&[0xFF; 5]);
    std::fs::write(&journal, &tail).unwrap();
    let (c, e) = run(&["undo", img_s, "--yes"]);
    assert_eq!(c, 0, "an unterminated trailing header is an unfinished append: {e}");
    assert_eq!(part_count(), before, "the complete prefix must still be replayed");

    // 中途损坏（第 1 条记录的数据）⇒ 整体拒绝，不碰镜像——"不做部分回放"针对的是这种情形
    let (c, e) = run(&["create", img_s, "--size", "1M", "--fs", "no-such-fs-type"]);
    assert_eq!(c, 20, "{e}");
    let jb = std::fs::read(&journal).unwrap();
    let mut mid = jb.clone();
    mid[5 + 16 + 1] ^= 0xFF; // magic 5 字节 + 记录头 16 字节之后即第 1 条的数据
    std::fs::write(&journal, &mid).unwrap();
    let (c, e) = run(&["undo", img_s, "--yes"]);
    assert_ne!(c, 0, "mid-file corruption must refuse undo: {e}");
    assert_eq!(part_count(), before + 1, "a refused undo must not touch the image");

    // 恢复完整 journal → undo 正常
    std::fs::write(&journal, &jb).unwrap();
    let (c, e) = run(&["undo", img_s, "--yes"]);
    assert_eq!(c, 0, "intact journal must undo cleanly: {e}");
    assert_eq!(part_count(), before, "undone partition must be gone");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn resize_part_move_copy_flag_name() {
    let dir = std::env::temp_dir().join(format!("diskedit_rp_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let img = dir.join("rp.img");
    // 16 MiB 镜像（32768 扇区），坐标全部取 1MiB 对齐值
    let data = vec![0u8; 16 * 1024 * 1024];
    std::fs::write(&img, &data).unwrap();
    let exe = env!("CARGO_BIN_EXE_DiskEdit");
    let img_s = img.to_str().unwrap();
    let run = |args: &[&str]| -> (i32, String, String) {
        let out = Command::new(exe).args(args).output().unwrap();
        (out.status.code().unwrap_or(-1),
         String::from_utf8_lossy(&out.stdout).into_owned(),
         String::from_utf8_lossy(&out.stderr).into_owned())
    };

    run(&["new", img_s, "--yes"]);
    let (c, _, _) = run(&["add", img_s, "--start", "2048", "--end", "6143", "--name", "a"]);
    assert_eq!(c, 0);
    let (c, _, _) = run(&["add", img_s, "--start", "14336", "--end", "16383", "--name", "b"]);
    assert_eq!(c, 0);

    // 数据指纹：写入可辨识字节到分区 a 首尾
    let mut f = std::fs::OpenOptions::new().write(true).open(&img).unwrap();
    use std::io::{Read, Seek, SeekFrom, Write};
    f.seek(SeekFrom::Start(2048 * 512)).unwrap();
    f.write_all(&[0xAA; 512]).unwrap();
    f.seek(SeekFrom::Start(6143 * 512)).unwrap();
    f.write_all(&[0xBB; 512]).unwrap();
    drop(f);

    // 向已占用区移动必须被拒绝（本工具策略：不做隐式避让，须先挪开挡路的分区）
    // 12000..17407 对齐后 12288..16383，与 b（14336..16383）重叠
    let (c, _, stderr) = run(&["resize-part", &format!("{img_s}:1"), "--start", "12000", "--end", "17407"]);
    assert_eq!(c, 10, "overlapping move must refuse");
    assert!(stderr.contains("overlaps"), "{stderr}");

    // move：分区 a 右移 8100..12400 → 对齐 8192..12287（落点空闲，无 FS → 跳过 FS 步骤）
    let (c, _, stderr) = run(&["resize-part", &format!("{img_s}:1"), "--start", "8100", "--end", "12400"]);
    assert_eq!(c, 0, "move must succeed: {stderr}");
    let mut f = std::fs::File::open(&img).unwrap();
    let gpt = gptman::GPT::find_from(&mut f).unwrap();
    assert_eq!(gpt[1].starting_lba, 8192);
    assert_eq!(gpt[1].ending_lba, 12287);
    // 指纹随数据一起搬移：新首尾应含 0xAA/0xBB
    let mut buf = [0u8; 512];
    f.seek(SeekFrom::Start(8192 * 512)).unwrap();
    f.read_exact(&mut buf).unwrap();
    assert!(buf.iter().all(|&b| b == 0xAA), "head fingerprint must move with partition");
    f.seek(SeekFrom::Start(12287 * 512)).unwrap();
    f.read_exact(&mut buf).unwrap();
    assert!(buf.iter().all(|&b| b == 0xBB), "tail fingerprint must move with partition");
    drop(f);

    // shrink：unknown FS 的分区拒绝缩容（无法先缩 FS，缩表项会写坏数据——fail-fast 不落盘）
    let (c, _, stderr) = run(&["resize-part", &format!("{img_s}:1"), "--start", "8192", "--end", "11000"]);
    assert_eq!(c, 10, "shrink of unknown-FS partition must refuse: {stderr}");
    assert!(stderr.contains("shrink"), "{stderr}");
    // 拒绝发生在任何写入前：表项保持 move 后的 8192..12287
    let mut f = std::fs::File::open(&img).unwrap();
    let gpt = gptman::GPT::find_from(&mut f).unwrap();
    assert_eq!(gpt[1].starting_lba, 8192);
    assert_eq!(gpt[1].ending_lba, 12287);
    drop(f);

    // copy：分区 b 复制到 20000 → 对齐 20480（b 长 2048 扇区 → 20480..22527）
    // --chunk-size 1（1MiB = 2048 扇区 = 整块单 chunk）：参数化 chunk 全程走通
    let (c, out, stderr) = run(&["copy", &format!("{img_s}:2"), "--start", "20000", "--name", "bcopy", "--chunk-size", "1"]);
    assert_eq!(c, 0, "copy must succeed: {out}{stderr}");
    // 非法 chunk 拒绝（0 越界）
    let (c, _, _) = run(&["copy", &format!("{img_s}:2"), "--start", "24000", "--chunk-size", "0"]);
    assert_eq!(c, 10, "invalid --chunk-size must refuse");

    // flag/name
    let (c, _, _) = run(&["set", &format!("{img_s}:2"), "flag", "esp", "on"]);
    assert_eq!(c, 0);
    let (c, _, _) = run(&["set", &format!("{img_s}:2"), "name", "renamed"]);
    assert_eq!(c, 0);

    // 终验：info + gptman
    let (c, out, _) = run(&["info", img_s]);
    assert_eq!(c, 0);
    assert!(out.contains("\"name\":\"renamed\""), "{out}");
    assert!(out.contains("\"name\":\"bcopy\""), "{out}");
    let mut f = std::fs::File::open(&img).unwrap();
    let gpt = gptman::GPT::find_from(&mut f).unwrap();
    assert_eq!(gpt[1].starting_lba, 8192);
    assert_eq!(gpt[1].ending_lba, 12287);
    // esp 标志 = 类型 GUID 换成 EFI System Partition GUID（属性位 bit60 是 Microsoft
    // read-only，与 ESP 无关）；磁盘字节序 = 前 3 字段小端
    assert_eq!(
        gpt[2].partition_type_guid,
        [0x28, 0x73, 0x2A, 0xC1, 0x1F, 0xF8, 0xD2, 0x11, 0xBA, 0x4B, 0x00, 0xA0, 0xC9, 0x3E, 0xC9, 0x3B],
        "esp flag must switch type GUID to EFI System Partition"
    );
    assert_eq!(gpt[3].starting_lba, 20480);
    assert_eq!(gpt[3].ending_lba, 22527);

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn msdos_table_flow() {
    let dir = std::env::temp_dir().join(format!("diskedit_mdos_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let img = dir.join("m.img");
    std::fs::write(&img, vec![0u8; 16 * 1024 * 1024]).unwrap();
    let exe = env!("CARGO_BIN_EXE_DiskEdit");
    let img_s = img.to_str().unwrap();
    let run = |args: &[&str]| -> (i32, String) {
        let out = Command::new(exe).args(args).output().unwrap();
        (out.status.code().unwrap_or(-1), String::from_utf8_lossy(&out.stdout).into_owned())
    };

    let (c, _) = run(&["new", img_s, "--yes", "--table", "msdos"]);
    assert_eq!(c, 0);
    let (c, out) = run(&["info", img_s]);
    assert_eq!(c, 0);
    let v: serde_json::Value = serde_json::from_str(out.trim()).expect("info must emit valid JSON");
    assert_eq!(v["label"], "mbr");

    let (c, _) = run(&["add", img_s, "--start", "2048", "--end", "4095", "--type", "0x83"]);
    assert_eq!(c, 0);
    let (c, _) = run(&["add", img_s, "--start", "4096", "--end", "6143", "--type", "0x0C"]);
    assert_eq!(c, 0);
    let (c, _) = run(&["add", img_s, "--start", "2049", "--end", "6143"]);
    assert_eq!(c, 10, "overlapping msdos add must refuse");

    let (c, _) = run(&["set", &format!("{img_s}:1"), "flag", "boot", "on"]);
    assert_eq!(c, 0);
    let (c, _) = run(&["info", img_s]);
    assert_eq!(c, 0);
    // boot 标志在 info 的 type 字段不可见，但 MBR 字节可验证：
    let raw = std::fs::read(&img).unwrap();
    assert_eq!(raw[446], 0x80, "boot flag must be on slot 1");

    // hidden：0x0C ↔ 0x1C（util-linux pt-mbr-partnames.h 标准对）；0x83 无对应码拒绝
    let (c, _) = run(&["set", &format!("{img_s}:2"), "flag", "hidden", "on"]);
    assert_eq!(c, 0);
    let raw = std::fs::read(&img).unwrap();
    assert_eq!(raw[446 + 16 + 4], 0x1C, "hidden must swap 0x0C -> 0x1C");
    let (c, _) = run(&["set", &format!("{img_s}:2"), "flag", "hidden", "off"]);
    assert_eq!(c, 0);
    let raw = std::fs::read(&img).unwrap();
    assert_eq!(raw[446 + 16 + 4], 0x0C, "unhidden must restore 0x0C");
    let (c, _) = run(&["set", &format!("{img_s}:1"), "flag", "hidden", "on"]);
    assert_eq!(c, 10, "0x83 has no hidden counterpart, must refuse");

    let (c, _) = run(&["del", &format!("{img_s}:2"), "--yes"]);
    assert_eq!(c, 0);
    let raw = std::fs::read(&img).unwrap();
    assert_eq!(&raw[446 + 16..446 + 32], &[0u8; 16], "slot 2 must be zeroed");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn msdos_user_resize() {
    let dir = std::env::temp_dir().join(format!("diskedit_mres_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let img = dir.join("r.img");
    std::fs::write(&img, vec![0u8; 16 * 1024 * 1024]).unwrap();
    let exe = env!("CARGO_BIN_EXE_DiskEdit");
    let img_s = img.to_str().unwrap();
    let run = |args: &[&str]| -> (i32, String) {
        let out = Command::new(exe).args(args).output().unwrap();
        (out.status.code().unwrap_or(-1), format!("{}\n{}",
            String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr)))
    };
    let slot = |raw: &[u8], n: usize| -> (u32, u32) {
        let off = 446 + (n - 1) * 16;
        let rd = |o: usize| u32::from_le_bytes(raw[o..o + 4].try_into().unwrap());
        (rd(off + 8), rd(off + 12))
    };

    let (c, o) = run(&["new", img_s, "--yes", "--table", "msdos"]);
    assert_eq!(c, 0, "{o}");
    // 两个主分区：1: 2048..4095（2048 扇区），2: 6144..8191——分区 1 右侧空闲 2048 扇区
    let (c, o) = run(&["add", img_s, "--start", "2048", "--end", "4095", "--type", "0x83"]);
    assert_eq!(c, 0, "{o}");
    let (c, o) = run(&["add", img_s, "--start", "6144", "--end", "8191", "--type", "0x83"]);
    assert_eq!(c, 0, "{o}");

    // +delta：分区 1 → 4096 扇区（恰好吃掉右侧空闲）
    let (c, o) = run(&["resize", &format!("{img_s}:1"), "+1M"]);
    assert_eq!(c, 0, "{o}");
    let raw = std::fs::read(&img).unwrap();
    assert_eq!(slot(&raw, 1), (2048, 4096), "relative grow");
    assert_eq!(slot(&raw, 2), (6144, 2048), "partition 2 untouched");

    // grow 吃满右侧：已到分区 2 起点，表不再变化
    let (c, o) = run(&["resize", &format!("{img_s}:1"), "grow"]);
    assert_eq!(c, 0, "{o}");
    let raw = std::fs::read(&img).unwrap();
    assert_eq!(slot(&raw, 1), (2048, 4096), "grow stops at partition 2");

    // 右侧空闲不足：绝对值超界必须拒绝且未写盘
    let raw_before = std::fs::read(&img).unwrap();
    let (c, o) = run(&["resize", &format!("{img_s}:1"), "3M"]);
    assert_eq!(c, 10, "grow beyond blocking partition must refuse: {o}");
    assert_eq!(std::fs::read(&img).unwrap(), raw_before, "refused resize must not write");

    // 收缩：unknown FS（0x83 空数据）拒绝——FS 先缩守卫链，任何写入前终止
    let (c, o) = run(&["resize", &format!("{img_s}:1"), "-1M"]);
    assert_eq!(c, 10, "shrinking unknown FS must refuse: {o}");
    assert_eq!(std::fs::read(&img).unwrap(), raw_before);

    // 不存在的分区 / 扩展容器场景的入口拒绝
    let (c, _) = run(&["resize", &format!("{img_s}:3"), "+1M"]);
    assert_eq!(c, 10);

    // --no-fs 与缩容不可共存：分区末端会切进未缩的 FS 元数据（与 GPT 路径同判据）
    let raw_before = std::fs::read(&img).unwrap();
    let (c, o) = run(&["resize", &format!("{img_s}:1"), "-1M", "--no-fs"]);
    assert_eq!(c, 10, "--no-fs + shrink must refuse: {o}");
    assert!(o.contains("--no-fs"), "{o}");
    assert_eq!(std::fs::read(&img).unwrap(), raw_before, "refused resize must not write");

    // :N 命中核验按表类型分派：MBR 分区能被解析出来（此前一律按"无 GPT"拒绝）
    let (c, o) = run(&["check", &format!("{img_s}:1")]);
    assert_eq!(c, 30, "empty MBR partition must resolve, then fail on FS tooling: {o}");
    assert!(!o.contains("no GPT"), "{o}");
    let (c, o) = run(&["check", &format!("{img_s}:9")]);
    assert_eq!(c, 10, "out-of-range MBR slot must refuse: {o}");
    assert!(o.contains("MBR covers primary slots"), "{o}");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn cyl_align_and_gpt_hidden_required() {
    let dir = std::env::temp_dir().join(format!("diskedit_cyl_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let img = dir.join("c.img");
    // 16 MiB（32768 扇区）：cyl 柱面 = 16065 扇区（LBA-assist 255 头 × 63 扇区）
    std::fs::write(&img, vec![0u8; 16 * 1024 * 1024]).unwrap();
    let exe = env!("CARGO_BIN_EXE_DiskEdit");
    let img_s = img.to_str().unwrap();
    let run = |args: &[&str]| -> (i32, String) {
        let out = Command::new(exe).args(args).output().unwrap();
        (out.status.code().unwrap_or(-1), String::from_utf8_lossy(&out.stderr).into_owned())
    };

    let (c, e) = run(&["new", img_s, "--yes"]);
    assert_eq!(c, 0, "{e}");

    // add --align cyl：8100..32700 → 16065..32129（start 上取整、end 下取整到柱面边界）
    let (c, e) = run(&["add", img_s, "--start", "8100", "--end", "32700", "--align", "cyl"]);
    assert_eq!(c, 0, "{e}");
    let mut f = std::fs::File::open(&img).unwrap();
    let gpt = gptman::GPT::find_from(&mut f).unwrap();
    assert_eq!(gpt[1].starting_lba, 16065);
    assert_eq!(gpt[1].ending_lba, 32129);
    drop(f);
    // 独立交叉验证：不经 gptman，直接按 UEFI §5.3.2 条目布局读原始字节
    // （条目 0 = LBA2 起始：starting_lba@+32、ending_lba@+40，LE u64）
    let raw = std::fs::read(&img).unwrap();
    let ent = &raw[2 * 512..2 * 512 + 128];
    let get = |o: usize| u64::from_le_bytes(ent[o..o + 8].try_into().unwrap());
    assert_eq!(get(32), 16065, "raw starting_lba must match");
    assert_eq!(get(40), 32129, "raw ending_lba must match");

    // 柱面对齐后区间为空 → 拒绝
    let (c, _) = run(&["add", img_s, "--start", "32500", "--end", "32510", "--align", "cyl"]);
    assert_eq!(c, 10, "empty-after-cyl-align must refuse");

    // 非法 --align 值拒绝
    let (c, _) = run(&["add", img_s, "--start", "4096", "--end", "4097", "--align", "sector"]);
    assert_eq!(c, 10, "invalid --align must refuse");

    // GPT hidden（bit1）/ required（bit0）经 CLI 置位与清除，互不干扰 legacy（bit2）
    let (c, e) = run(&["set", &format!("{img_s}:1"), "flag", "legacy", "on"]);
    assert_eq!(c, 0, "{e}");
    let (c, e) = run(&["set", &format!("{img_s}:1"), "flag", "hidden", "on"]);
    assert_eq!(c, 0, "{e}");
    let (c, e) = run(&["set", &format!("{img_s}:1"), "flag", "required", "on"]);
    assert_eq!(c, 0, "{e}");
    let mut f = std::fs::File::open(&img).unwrap();
    let gpt = gptman::GPT::find_from(&mut f).unwrap();
    assert_eq!(gpt[1].attribute_bits, (1 << 0) | (1 << 1) | (1 << 2));
    drop(f);
    let (c, e) = run(&["set", &format!("{img_s}:1"), "flag", "hidden", "off"]);
    assert_eq!(c, 0, "{e}");
    let (c, e) = run(&["set", &format!("{img_s}:1"), "flag", "required", "off"]);
    assert_eq!(c, 0, "{e}");
    let mut f = std::fs::File::open(&img).unwrap();
    let gpt = gptman::GPT::find_from(&mut f).unwrap();
    assert_eq!(gpt[1].attribute_bits, 1 << 2);

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn grow_to_end() {
    let dir = std::env::temp_dir().join(format!("diskedit_gte_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let img = dir.join("g.img");
    // 8 MiB（16384 扇区）：last_usable = 16350
    std::fs::write(&img, vec![0u8; 8 * 1024 * 1024]).unwrap();
    let exe = env!("CARGO_BIN_EXE_DiskEdit");
    let img_s = img.to_str().unwrap();
    let run = |args: &[&str]| -> (i32, String) {
        let out = Command::new(exe).args(args).output().unwrap();
        (out.status.code().unwrap_or(-1), String::from_utf8_lossy(&out.stderr).into_owned())
    };

    let (c, e) = run(&["new", img_s, "--yes"]);
    assert_eq!(c, 0, "{e}");
    let (c, e) = run(&["add", img_s, "--start", "2048", "--end", "4095", "--name", "a"]);
    assert_eq!(c, 0, "{e}");

    // --end 与 --grow-to-end 互斥
    let (c, _) = run(&["resize-part", &format!("{img_s}:1"), "--start", "2048", "--end", "6000", "--grow-to-end"]);
    assert_eq!(c, 10, "--end + --grow-to-end must refuse");

    // grow-to-end：扩到 last_usable_lba 16350（无 FS → 跳过 FS 步骤）
    let (c, e) = run(&["resize-part", &format!("{img_s}:1"), "--start", "2048", "--grow-to-end"]);
    assert_eq!(c, 0, "{e}");
    let mut f = std::fs::File::open(&img).unwrap();
    let gpt = gptman::GPT::find_from(&mut f).unwrap();
    assert_eq!(gpt[1].starting_lba, 2048);
    assert_eq!(gpt[1].ending_lba, 16350);

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn apply_swap_recreate() {
    let dir = std::env::temp_dir().join(format!("diskedit_swr_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let img = dir.join("s.img");
    // 16 MiB（32768 扇区，last_usable 32734）：root 2048..6143 + swap 6144..8191
    // apply 后：swap 尾部打包 30687..32734（零数据搬移），root 扩到 32734
    // swap 分区内预置 v1 swap 头（见下），使重建时 -U/-L 都有值可传
    let ss = 512u64;
    let data = vec![0u8; 16 * 1024 * 1024];
    let mut cur = Cursor::new(data);
    let mut gpt = gptman::GPT::new_from(&mut cur, ss, [0xCD; 16]).unwrap();
    gpt[1] = gptman::GPTPartitionEntry {
        partition_type_guid: [0x11; 16],
        unique_partition_guid: [0x12; 16],
        starting_lba: 2048,
        ending_lba: 6143,
        attribute_bits: 0,
        partition_name: "root".into(),
    };
    gpt[2] = gptman::GPTPartitionEntry {
        partition_type_guid: [
            0x6D, 0xFD, 0x57, 0x06, 0xAB, 0xA4, 0xC4, 0x43, 0x84, 0xE5, 0x09, 0x33, 0xC8, 0x4B, 0x4F, 0x4F,
        ],
        unique_partition_guid: [0x42; 16],
        starting_lba: 6144,
        ending_lba: 8191,
        attribute_bits: 0,
        partition_name: "swap".into(),
    };
    gpt.write_into(&mut cur).unwrap();
    // v1 swap 头（内核 include/linux/swap.h union swap_header）：magic 在首页尾，
    // sws_uuid @1036、sws_volume @1052。全零分区只会走"无 swap 头 → 随机 UUID"那条路，
    // mkswap 的 -U/-L 参数构造就永远进不了自动化
    const SWAP_UUID: [u8; 16] = [0xA1; 16];
    const SWAP_LABEL: [u8; 3] = [b'A', 0xFF, b'B']; // 含非法 UTF-8 字节：按字节往返的判据
    {
        let ps = 4096usize; // 探测候选里本机页优先，4096 先命中
        let base = 6144usize * 512; // swap 分区起始（LBA 6144）
        let raw = cur.get_mut();
        raw[base + ps - 10..base + ps].copy_from_slice(b"SWAPSPACE2");
        raw[base + 1036..base + 1052].copy_from_slice(&SWAP_UUID);
        raw[base + 1052..base + 1055].copy_from_slice(&SWAP_LABEL);
    }
    std::fs::write(&img, with_protective_mbr(cur.into_inner())).unwrap();
    let exe = env!("CARGO_BIN_EXE_DiskEdit");
    let img_s = img.to_str().unwrap();

    let out = Command::new(exe).args(["apply", img_s, "--grow", "1"]).output().unwrap();
    let code = out.status.code().unwrap_or(-1);
    let stderr = String::from_utf8_lossy(&out.stderr);
    // swap 重建依赖 mkswap/losetup（仅 Linux）：工具可用 → 后置条件全满足（OK）；
    // 不可用 → 分区表已更新但 swap 未重建，属部分完成（PARTIAL），且必须给出补救命令。
    // 这里不再把"FS/swap 步骤失败"当作成功——那正是让脚本误判空间可用的根源
    #[cfg(target_os = "linux")]
    {
        // effective UID 取自 /proc/self/status 的 Uid: 行（proc(5)：四列依次为
        // real/effective/saved/filesystem，取第 2 列）；测试层不引入 libc 依赖
        let is_root = std::fs::read_to_string("/proc/self/status")
            .ok()
            .and_then(|s| s.lines().find(|l| l.starts_with("Uid:")).map(str::to_string))
            .and_then(|l| l.split_whitespace().nth(2).map(|f| f == "0"))
            .unwrap_or(false);
        if is_root {
            assert_eq!(code, 0, "apply must fully succeed as root: {stderr}");
            // 重建后的 swap 落在新位置（LBA 30687）：-U/-L 传下去的值必须原样出现在新 swap 头里
            use std::io::{Read, Seek, SeekFrom};
            let mut f = std::fs::File::open(&img).unwrap();
            let mut uuid = [0u8; 16];
            let mut label = [0u8; 16];
            f.seek(SeekFrom::Start(30687 * 512 + 1036)).unwrap();
            f.read_exact(&mut uuid).unwrap();
            f.seek(SeekFrom::Start(30687 * 512 + 1052)).unwrap();
            f.read_exact(&mut label).unwrap();
            assert_eq!(uuid, SWAP_UUID, "mkswap -U must restore the swap UUID");
            assert_eq!(&label[..SWAP_LABEL.len()], &SWAP_LABEL[..], "mkswap -L must carry the label byte-for-byte");
            assert!(label[SWAP_LABEL.len()..].iter().all(|&b| b == 0), "the rest of the label field must be NUL padding");
        } else {
            // CI runner 等非 root 环境：mkswap 无法执行 → 表已更新、swap 待重建
            assert_eq!(code, 20, "non-root: swap step pending → PARTIAL: {stderr}");
            assert!(stderr.contains("swap rebuild") && stderr.contains("mkswap"), "remedy must be printed: {stderr}");
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        assert_eq!(code, 20, "without mkswap the swap step is pending → PARTIAL: {stderr}");
        assert!(stderr.contains("swap rebuild") && stderr.contains("mkswap"), "remedy must be printed: {stderr}");
    }

    let mut f = std::fs::File::open(&img).unwrap();
    let gpt = gptman::GPT::find_from(&mut f).unwrap();
    assert_eq!(gpt[1].starting_lba, 2048, "root start unchanged");
    // root 扩到 swap 新区域之前（30687-1=30686），不得重叠
    assert_eq!(gpt[1].ending_lba, 30686, "root must end before relocated swap");
    assert_eq!(gpt[2].starting_lba, 30687, "swap packed at tail");
    assert_eq!(gpt[2].ending_lba, 32734);
    assert_eq!(gpt[2].unique_partition_guid, [0x42; 16], "PARTUUID must be preserved");

    let _ = std::fs::remove_dir_all(&dir);
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
    // 带 --allow-move --yes：自动搬移 #2 到尾部并扩 #1（apply 打包到 last_usable=32734）
    let (c, _, e) = run(&["resize", &format!("{img_s}:1"), "grow", "--allow-move", "--yes"]);
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

    // resize --size 收缩：unknown FS 拒绝（fail-fast）
    let (c, _, stderr) = run(&["resize", &format!("{img_s}:1"), "--size", "8388608"]);
    assert_eq!(c, 10, "shrink of unknown FS must refuse: {stderr}");
    assert!(stderr.contains("shrink"), "{stderr}");

    // delete + --size 扩容：删 #2 后 #1 扩到 12MiB（4096..28671）
    let (c, _, e) = run(&["delete", &format!("{img_s}:2"), "--yes"]);
    assert_eq!(c, 0, "{e}");
    let (c, _, e) = run(&["resize", &format!("{img_s}:1"), "--size", "12582912"]);
    assert_eq!(c, 0, "grow by --size must succeed: {e}");
    let mut f = std::fs::File::open(&img).unwrap();
    let gpt = gptman::GPT::find_from(&mut f).unwrap();
    assert_eq!(gpt[1].starting_lba, 4096);
    assert_eq!(gpt[1].ending_lba, 28671);
    assert_eq!(gpt[2].ending_lba, 0, "partition 2 must be gone");
    drop(f);

    let _ = std::fs::remove_dir_all(&dir);
}

/// 最小位移扩容（resize SIZE + --allow-move）：挡路分区按需让位，
/// 目标精确落在请求的新末端（不吞并目标与挡路者之间的间隙）
#[test]
fn resize_shift_relocates_blockers() {
    let dir = std::env::temp_dir().join(format!("diskedit_shift_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let img = dir.join("sh.img");
    // 16 MiB（32768 扇区，last_usable 32734），坐标全部 1MiB 对齐
    std::fs::write(&img, vec![0u8; 16 * 1024 * 1024]).unwrap();
    let exe = env!("CARGO_BIN_EXE_DiskEdit");
    let img_s = img.to_str().unwrap();
    let run = |args: &[&str]| -> (i32, String, String) {
        let out = Command::new(exe).args(args).output().unwrap();
        (out.status.code().unwrap_or(-1),
         String::from_utf8_lossy(&out.stdout).into_owned(),
         String::from_utf8_lossy(&out.stderr).into_owned())
    };

    let (c, _, e) = run(&["new", img_s, "--yes"]);
    assert_eq!(c, 0, "{e}");
    let (c, _, e) = run(&["add", img_s, "--start", "2048", "--end", "4095", "--name", "a"]);
    assert_eq!(c, 0, "{e}");
    let (c, _, e) = run(&["add", img_s, "--start", "4096", "--end", "6143", "--name", "b"]);
    assert_eq!(c, 0, "{e}");
    let (c, _, e) = run(&["add", img_s, "--start", "12288", "--end", "14335", "--name", "c"]);
    assert_eq!(c, 0, "{e}");

    // 数据指纹：b 首尾、c 首部各写可辨识字节
    {
        let mut f = std::fs::OpenOptions::new().write(true).open(&img).unwrap();
        use std::io::{Seek, SeekFrom, Write};
        f.seek(SeekFrom::Start(4096 * 512)).unwrap();
        f.write_all(&[0xAA; 512]).unwrap();
        f.seek(SeekFrom::Start(6143 * 512)).unwrap();
        f.write_all(&[0xBB; 512]).unwrap();
        f.seek(SeekFrom::Start(12288 * 512)).unwrap();
        f.write_all(&[0xCC; 512]).unwrap();
    }

    // 无 --allow-move：右侧被 b 挡 → 拒绝
    let (c, _, stderr) = run(&["resize", &format!("{img_s}:1"), "+3M"]);
    assert_eq!(c, 10, "blocked resize without --allow-move must refuse");
    assert!(stderr.contains("--allow-move"), "{stderr}");

    // --allow-move 无 --yes：打印位移计划后拒绝
    let (c, out, stderr) = run(&["resize", &format!("{img_s}:1"), "+3M", "--allow-move"]);
    assert_eq!(c, 10, "plan printed but --yes missing must refuse");
    assert!(out.contains("move part 2"), "plan must be printed: {out}");
    assert!(out.contains("move part 3"), "plan must include the far blocker: {out}");
    assert!(stderr.contains("--yes"), "{stderr}");

    // +3M 相对当前 1M 大小 → 绝对 4M：b/c 让位，a 精确扩到 10239（2048+8192-1）
    let (c, _, e) = run(&["resize", &format!("{img_s}:1"), "+3M", "--allow-move", "--yes"]);
    assert_eq!(c, 0, "shift resize must succeed: {e}");
    let mut f = std::fs::File::open(&img).unwrap();
    let gpt = gptman::GPT::find_from(&mut f).unwrap();
    assert_eq!(gpt[1].starting_lba, 2048);
    assert_eq!(gpt[1].ending_lba, 10239, "target must land exactly at the requested end");
    assert_eq!(gpt[2].starting_lba, 10240, "adjacent blocker shifts by the growth delta");
    assert_eq!(gpt[2].ending_lba, 12287);
    assert_eq!(gpt[3].starting_lba, 18432, "far blocker keeps its leading gap");
    assert_eq!(gpt[3].ending_lba, 20479);
    // 指纹随数据搬移
    let mut buf = [0u8; 512];
    use std::io::{Read, Seek, SeekFrom};
    f.seek(SeekFrom::Start(10240 * 512)).unwrap();
    f.read_exact(&mut buf).unwrap();
    assert!(buf.iter().all(|&b| b == 0xAA), "b head fingerprint must survive");
    f.seek(SeekFrom::Start(12287 * 512)).unwrap();
    f.read_exact(&mut buf).unwrap();
    assert!(buf.iter().all(|&b| b == 0xBB), "b tail fingerprint must survive");
    f.seek(SeekFrom::Start(18432 * 512)).unwrap();
    f.read_exact(&mut buf).unwrap();
    assert!(buf.iter().all(|&b| b == 0xCC), "c head fingerprint must survive");
    drop(f);

    // 让位后的空闲不足：d 挡在末端且无尾部空间 → 明确拒绝
    let (c, _, stderr) = run(&["resize", &format!("{img_s}:1"), "+64M", "--allow-move", "--yes"]);
    assert_eq!(c, 10, "grow beyond usable range must refuse: {stderr}");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn info_on_gpt_image() {
    let dir = std::env::temp_dir().join(format!("diskedit_e2e_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let img = dir.join("disk.img");
    fixture_gpt_image(&img);

    let out = Command::new(env!("CARGO_BIN_EXE_DiskEdit"))
        .args(["info", img.to_str().unwrap()])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0), "info must exit 0");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let v: serde_json::Value = serde_json::from_str(stdout.trim()).expect("info must emit valid JSON");
    assert_eq!(v["label"], "gpt");
    assert_eq!(v["sector_size"], 512);
    assert_eq!(v["partitions"][0]["name"], "smoke");
    // swap 类型 GUID 标准文本（info 由磁盘字节序重排回 8-4-4-4-12）
    assert_eq!(v["partitions"][0]["type"], "0657FD6D-A4AB-43C4-84E5-0933C84B4F4F");

    // plan/apply 对 swap 走"重建"语义：plan 成功且标记 swap
    let ss = 512u64;
    let data2 = vec![0u8; 200 * ss as usize];
    let mut cur = Cursor::new(data2);
    let mut gpt = gptman::GPT::new_from(&mut cur, ss, [0xCD; 16]).unwrap();
    gpt[1] = gptman::GPTPartitionEntry {
        partition_type_guid: [0x11; 16],
        unique_partition_guid: [0x12; 16],
        starting_lba: 34,
        ending_lba: 100,
        attribute_bits: 0,
        partition_name: "root".into(),
    };
    gpt[2] = gptman::GPTPartitionEntry {
        partition_type_guid: [
            0x6D, 0xFD, 0x57, 0x06, 0xAB, 0xA4, 0xC4, 0x43, 0x84, 0xE5, 0x09, 0x33, 0xC8, 0x4B, 0x4F, 0x4F,
        ],
        unique_partition_guid: [0x13; 16],
        starting_lba: 101,
        ending_lba: 130,
        attribute_bits: 0,
        partition_name: "swap".into(),
    };
    gpt.write_into(&mut cur).unwrap();
    let img2 = dir.join("disk2.img");
    std::fs::write(&img2, with_protective_mbr(cur.into_inner())).unwrap();

    let out = Command::new(env!("CARGO_BIN_EXE_DiskEdit"))
        .args(["plan", img2.to_str().unwrap(), "--grow", "1"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0), "plan must succeed with swap-recreate semantics");
    let plan_out = String::from_utf8_lossy(&out.stdout);
    assert!(plan_out.contains("[swap: recreate, no data move]"), "{plan_out}");

    let _ = std::fs::remove_dir_all(&dir);
}

/// 真实扩容路径：truncate 预扩使 backup GPT 与保护 MBR 同时过期 ——
/// info 只报告、plan 只记录（两者都不写盘），写入命令才修复（搬备份头 + 重写 PMBR）
#[test]
fn stale_after_enlarge_info_plan_write() {
    use std::io::Write as _;
    let dir = std::env::temp_dir().join(format!("diskedit_stale_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let img = dir.join("grown.img");
    fixture_gpt_image(&img); // 100 扇区，分区 1 = 34..60
    {
        // 模拟 truncate 预扩：尾部追加 1 MiB（备份头与保护 MBR 都停在旧末端）
        let mut f = std::fs::OpenOptions::new().append(true).open(&img).unwrap();
        f.write_all(&vec![0u8; 1024 * 1024]).unwrap();
    }
    let exe = env!("CARGO_BIN_EXE_DiskEdit");
    let img_s = img.to_str().unwrap();
    let run = |args: &[&str]| -> (i32, String, String) {
        let out = Command::new(exe).args(args).output().unwrap();
        (out.status.code().unwrap_or(-1),
         String::from_utf8_lossy(&out.stdout).into_owned(),
         String::from_utf8_lossy(&out.stderr).into_owned())
    };
    let total = std::fs::metadata(&img).unwrap().len() / 512; // 2148
    let before = std::fs::read(&img).unwrap();

    // info：两条提示（备份头 + 保护 MBR），且不得改写盘
    let (c, out, e) = run(&["info", img_s]);
    assert_eq!(c, 0, "{e}");
    assert!(out.contains("\"label\":\"gpt\""), "{out}");
    assert!(e.contains("backup GPT header is stale"), "info must warn about stale backup GPT: {e}");
    assert!(e.contains("protective MBR SizeInLBA is stale"), "info must warn about stale PMBR: {e}");
    assert_eq!(std::fs::read(&img).unwrap(), before, "info must not modify the image");

    // plan：把修复动作列为计划项，同样不得写盘
    let (c, out, e) = run(&["plan", img_s, "--grow", "1"]);
    assert_eq!(c, 0, "{e}");
    assert!(out.contains("[repair] relocate backup GPT"), "{out}");
    assert_eq!(std::fs::read(&img).unwrap(), before, "plan must not modify the image");

    // 写入命令：修复备份头 + 保护 MBR
    let (c, _, e) = run(&["set", &format!("{img_s}:1"), "name", "renamed"]);
    assert_eq!(c, 0, "write path must repair: {e}");
    let (_, _, e3) = run(&["info", img_s]);
    assert!(!e3.contains("stale"), "nothing stale must remain: {e3}");
    let after = std::fs::read(&img).unwrap();
    let pmbr_size = u32::from_le_bytes([after[446 + 12], after[446 + 13], after[446 + 14], after[446 + 15]]);
    assert_eq!(pmbr_size as u64, total - 1, "protective MBR SizeInLBA must match the container");
    let backup_lba = u64::from_le_bytes(after[512 + 32..512 + 40].try_into().unwrap());
    assert_eq!(backup_lba, total - 1, "backup GPT must sit at the device end");

    let _ = std::fs::remove_dir_all(&dir);
}

/// 错误路径走真实二进制：bail() 会退出进程，进程内测不到，故用 subprocess 断言退出码 + 文案
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

    // 有 GPT 结构但缺保护 MBR → 不按 GPT 认（label none），resize 拒绝
    let no_pmbr = dir.join("nopmbr.img");
    {
        let mut cur = Cursor::new(vec![0u8; 100 * 512]);
        let mut gpt = gptman::GPT::new_from(&mut cur, 512, [0xAB; 16]).unwrap();
        gpt.write_into(&mut cur).unwrap();
        std::fs::write(&no_pmbr, cur.into_inner()).unwrap(); // 故意不补保护 MBR
    }
    let (c, out, _) = run(&["info", no_pmbr.to_str().unwrap()]);
    assert_eq!(c, 0);
    assert!(out.contains("\"label\":\"none\""), "GPT without protective MBR must not be claimed: {out}");
    let (c, _, e) = run(&["resize", &format!("{}:1", no_pmbr.display()), "10M"]);
    assert_eq!(c, 10, "resize on unlabelled target must refuse: {e}");
    // label=none 走 superfloppy 分支：带 :1 说明用户以为有分区表，故明确拒绝
    assert!(e.contains("no partition table"), "{e}");

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
    // 同一判据延伸到 plan/apply/check（此前这三处把"表非法"压成了 10）
    let (c, _, e) = run(&["plan", bad2.to_str().unwrap(), "--grow", "1"]);
    assert_eq!(c, 30, "plan on a corrupt table must be infra: {e}");
    assert!(e.contains("parse failed"), "{e}");
    let (c, _, e) = run(&["apply", bad2.to_str().unwrap(), "--grow", "1", "--yes"]);
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
    assert!(e.contains("no GPT on target"), "{e}");

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
    let (c, _, _) = run(&["info", img_s, "--sector-size", "300"]);
    assert_eq!(c, 30, "非 2 的幂扇区大小须被拒绝");

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
/// 布局坐标系不变量：4Kn GPT 表放在 512e 容器上（g.ss ≠ src.sector_size）时，
/// add/create 的对齐单位与 --size 换算必须按**表头记录的 ss** 折算（1MiB = 256 个表 LBA）。
/// 按容器 ss 折算会把 1MiB 错算成 8MiB（2048 个容器扇区），合法 add 被误拒
#[test]
fn layout_uses_table_sector_size_not_container() {
    let dir = std::env::temp_dir().join(format!("diskedit_4kn_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let img = dir.join("mixed.img");
    // 8 MiB 镜像：4Kn 表视角共 2048 个 LBA，可用区 34..2014
    std::fs::write(&img, vec![0u8; 8 * 1024 * 1024]).unwrap();
    let exe = env!("CARGO_BIN_EXE_DiskEdit");
    let img_s = img.to_str().unwrap();
    let run = |args: &[&str]| -> (i32, String, String) {
        let out = Command::new(exe).args(args).output().unwrap();
        (out.status.code().unwrap_or(-1),
         String::from_utf8_lossy(&out.stdout).into_owned(),
         String::from_utf8_lossy(&out.stderr).into_owned())
    };

    // 4Kn 容器建表：表头记录 ss = 4096
    let (c, _, o) = run(&["new", img_s, "--sector-size", "4096", "--yes"]);
    assert_eq!(c, 0, "new with 4kn sectors: {o}");

    // 换 512 容器重开（不带 --sector-size）：表 ss 与容器 ss 不一致。
    // --start 256（表 LBA 256 = 字节 1MiB）已按表坐标对齐，必须成功
    let (c, _, o) = run(&["add", img_s, "--start", "256", "--end", "511", "--name", "t"]);
    assert_eq!(c, 0, "add on 4kn-table/512-container: {o}");

    let (c, out, _) = run(&["info", img_s]);
    assert_eq!(c, 0);
    let v: serde_json::Value = serde_json::from_str(out.trim()).expect("info must emit valid JSON");
    assert_eq!(v["sector_size"], 4096, "table ss must be probed from the GPT header: {out}");
    let p = &v["partitions"][0];
    assert_eq!(p["first_lba"], 256);
    assert_eq!(p["last_lba"], 511);
    assert_eq!(p["size_bytes"], 1024 * 1024);

    // create 的空闲区与 --size 换算同样按表 ss：下一个 1MiB 边界 = 表 LBA 512
    let (c, _, o) = run(&["create", img_s, "--size", "1M"]);
    assert_eq!(c, 0, "create on 4kn-table/512-container: {o}");
    let (c, out, _) = run(&["info", img_s]);
    assert_eq!(c, 0);
    let v: serde_json::Value = serde_json::from_str(out.trim()).expect("info must emit valid JSON");
    let parts = v["partitions"].as_array().unwrap();
    assert_eq!(parts.len(), 2, "{out}");
    assert_eq!(parts[1]["first_lba"], 512);
    assert_eq!(parts[1]["last_lba"], 767);
    assert_eq!(parts[1]["size_bytes"], 1024 * 1024);

    let _ = std::fs::remove_dir_all(&dir);
}

/// fail-closed 参数契约：命令不消费的旗标一律拒绝（而非静默忽略）；
/// 语义冲突的旗标组合（MBR resize 的 --allow-move、superfloppy 的 --no-fs、
/// 离线 resizefs 的 --size）显式拒绝；MBR type 接受大写 0X 前缀
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
    assert!(out.contains("\"type\":\"0x83\""), "{out}");

    let _ = std::fs::remove_dir_all(&dir);
}