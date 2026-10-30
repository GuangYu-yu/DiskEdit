use super::*;

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

    // delete：删除 #1，info 不再含 root（JSON 结构化断言，非子串匹配）
    let (c, _, _) = run(&["delete", &format!("{img_s}:1"), "--yes"]);
    assert_eq!(c, 0, "delete must succeed");
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

/// 对齐放大到溢出的 start（`--start` 逼近 u64::MAX、`--align cyl`）必须拒绝：
/// `div_ceil(unit) * unit` 会静默回绕成一个"从 0 起"的合法区间，把无法对齐的输入
/// 伪装成能落盘的坐标
#[test]
fn huge_start_with_cyl_alignment_is_refused_not_wrapped() {
    let dir = std::env::temp_dir().join(format!("diskedit_ovf_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let img = dir.join("t.img");
    std::fs::write(&img, vec![0u8; 4 * 1024 * 1024]).unwrap();
    let exe = env!("CARGO_BIN_EXE_DiskEdit");
    let img_s = img.to_str().unwrap();
    let run = |args: &[&str]| -> (i32, String, String) {
        let out = Command::new(exe).args(args).output().unwrap();
        (out.status.code().unwrap_or(-1),
         String::from_utf8_lossy(&out.stdout).into_owned(),
         String::from_utf8_lossy(&out.stderr).into_owned())
    };
    assert_eq!(run(&["new", img_s, "--yes"]).0, 0);
    // cyl = 16065 扇区/柱面：上取整后乘法越过 u64::MAX
    let (c, _, err) = run(&["add", img_s, "--start", "18446744073709551614", "--end", "18446744073709551615", "--align", "cyl", "--name", "x"]);
    assert_eq!(c, 10, "an unalignable start must be refused, not wrapped: {err}");
    assert!(err.contains("overflows the LBA range"), "{err}");
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

    // 两份都清零 → 保护 MBR 仍在，盘型可辨：gpt + damaged，不得落成 none——
    // none 会让 new 把还能救回的表当无表盘覆盖、resize 走 superfloppy 整盘扩
    wipe(1);
    let last = std::fs::metadata(&img).unwrap().len() / 512 - 1;
    wipe(last);
    let (c, out, _) = run(&["info", img_s]);
    assert_eq!(c, 0);
    let v: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(v["label"], "gpt", "pmbr intact must still identify as gpt: {out}");
    assert_eq!(v["damaged"], true, "headers gone must read as damaged: {out}");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn new_gpt_refuses_to_clear_bytes_inside_an_existing_partition() {
    let dir = std::env::temp_dir().join(format!("diskedit_newcollide_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let img = dir.join("c.img");
    std::fs::write(&img, vec![0u8; 8 * 1024 * 1024]).unwrap();
    let exe = env!("CARGO_BIN_EXE_DiskEdit");
    let img_s = img.to_str().unwrap();
    let run = |args: &[&str]| -> (i32, String, String) {
        let out = Command::new(exe).args(args).output().unwrap();
        (out.status.code().unwrap_or(-1),
         String::from_utf8_lossy(&out.stdout).into_owned(),
         String::from_utf8_lossy(&out.stderr).into_owned())
    };

    // MBR 分区铺到盘尾（2048..16383，16384 扇区的镜像）：GPT 备份区落在这些字节里
    let (c, _, e) = run(&["new", img_s, "--table", "msdos", "--yes"]);
    assert_eq!(c, 0, "{e}");
    let (c, _, e) = run(&["add", img_s, "--start", "2048", "--end", "16383"]);
    assert_eq!(c, 0, "{e}");

    let before = std::fs::read(&img).unwrap();
    let (c, _, e) = run(&["new", img_s, "--table", "gpt", "--yes"]);
    assert_eq!(c, 10, "--yes alone must not license clearing bytes inside a partition: {e}");
    assert!(e.contains("backup entry array"), "the collision must be named: {e}");
    assert!(e.contains("--overwrite-partition-data"), "the flag that unblocks it must be named: {e}");
    assert_eq!(std::fs::read(&img).unwrap(), before, "the refusal must not write");

    let (c, _, e) = run(&["new", img_s, "--table", "gpt", "--yes", "--overwrite-partition-data"]);
    assert_eq!(c, 0, "explicit consent to overwrite those bytes must go through: {e}");
    let (c, out, _) = run(&["info", img_s]);
    assert_eq!(c, 0);
    let v: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(v["label"], "gpt", "{out}");
    assert_eq!(v["partitions"].as_array().map(|a| a.len()), Some(0), "{out}");

    // 独立镜像：GPT 最后一个分区止于 last_usable（16350）时，重建同款表不得误报
    let ok = dir.join("ok.img");
    std::fs::write(&ok, vec![0u8; 8 * 1024 * 1024]).unwrap();
    let ok_s = ok.to_str().unwrap();
    let (c, _, e) = run(&["new", ok_s, "--yes"]);
    assert_eq!(c, 0, "{e}");
    let (c, _, e) = run(&["add", ok_s, "--start", "2048", "--end", "16350"]);
    assert_eq!(c, 0, "{e}");
    let (c, _, e) = run(&["new", ok_s, "--yes"]);
    assert_eq!(c, 0, "a partition ending at last_usable does not reach the backup area: {e}");

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

    let (c, _) = run(&["delete", &format!("{img_s}:2"), "--yes"]);
    assert_eq!(c, 0);
    let raw = std::fs::read(&img).unwrap();
    assert_eq!(&raw[446 + 16..446 + 32], &[0u8; 16], "slot 2 must be zeroed");

    let _ = std::fs::remove_dir_all(&dir);
}

/// `set type` 的 CLI 分发：GPT 写类型 GUID、MBR 写类型字节，都能读回；全零 GUID 与
/// 0x00 / 扩展容器类型（0x05/0x0F/0x85）在写盘前拒绝，拒绝后盘上原值不变
#[test]
fn set_type_cli() {
    let dir = std::env::temp_dir().join(format!("diskedit_stype_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let exe = env!("CARGO_BIN_EXE_DiskEdit");
    let run = |args: &[&str]| -> (i32, String) {
        let out = Command::new(exe).args(args).output().unwrap();
        (out.status.code().unwrap_or(-1), String::from_utf8_lossy(&out.stdout).into_owned())
    };
    let gpt_type = |img: &std::path::Path| -> String {
        let (c, out) = run(&["info", img.to_str().unwrap()]);
        assert_eq!(c, 0, "info must succeed: {out}");
        let v: serde_json::Value = serde_json::from_str(out.trim()).expect("info must emit valid JSON");
        v["partitions"][0]["type"].as_str().unwrap().to_string()
    };

    // GPT：文本 GUID（连字符可省）落进条目
    let g = dir.join("g.img");
    std::fs::write(&g, vec![0u8; 8 * 1024 * 1024]).unwrap();
    let g_s = g.to_str().unwrap();
    let (c, _) = run(&["new", g_s, "--yes"]);
    assert_eq!(c, 0);
    let (c, _) = run(&["add", g_s, "--start", "2048", "--end", "4095"]);
    assert_eq!(c, 0);
    let (c, _) = run(&["set", &format!("{g_s}:1"), "type", "c12a7328f81f11d2ba4b00a0c93ec93b"]);
    assert_eq!(c, 0);
    assert_eq!(gpt_type(&g), "C12A7328-F81F-11D2-BA4B-00A0C93EC93B");
    // 全零 GUID 是"未用条目"的形状：拒绝且不改盘
    let (c, _) = run(&["set", &format!("{g_s}:1"), "type", "00000000-0000-0000-0000-000000000000"]);
    assert_eq!(c, 10, "all-zero type GUID must refuse");
    assert_eq!(gpt_type(&g), "C12A7328-F81F-11D2-BA4B-00A0C93EC93B", "refused set must not touch the table");
    // 类型字节不是 GUID 文本；空槽没有类型可改
    let (c, _) = run(&["set", &format!("{g_s}:1"), "type", "0x83"]);
    assert_eq!(c, 10, "GPT target must reject an MBR-style byte");
    let (c, _) = run(&["set", &format!("{g_s}:2"), "type", "C12A7328-F81F-11D2-BA4B-00A0C93EC93B"]);
    assert_eq!(c, 10, "empty slot must refuse");

    // MBR：类型字节直改，0x 前缀大小写不限
    let m = dir.join("m.img");
    std::fs::write(&m, vec![0u8; 8 * 1024 * 1024]).unwrap();
    let m_s = m.to_str().unwrap();
    let (c, _) = run(&["new", m_s, "--yes", "--table", "msdos"]);
    assert_eq!(c, 0);
    let (c, _) = run(&["add", m_s, "--start", "2048", "--end", "4095", "--type", "0x83"]);
    assert_eq!(c, 0);
    let (c, _) = run(&["set", &format!("{m_s}:1"), "type", "0X07"]);
    assert_eq!(c, 0);
    assert_eq!(std::fs::read(&m).unwrap()[446 + 4], 0x07, "type byte must land in slot 1");
    for bad in ["0x00", "0x05", "0x0F", "0x85"] {
        let (c, _) = run(&["set", &format!("{m_s}:1"), "type", bad]);
        assert_eq!(c, 10, "{bad} must be refused");
    }
    assert_eq!(std::fs::read(&m).unwrap()[446 + 4], 0x07, "refused set must not touch slot 1");
    let (c, _) = run(&["set", &format!("{m_s}:2"), "type", "0x07"]);
    assert_eq!(c, 10, "empty slot must refuse");
    let (c, _) = run(&["set", &format!("{m_s}:5"), "type", "0x07"]);
    assert_eq!(c, 10, "slot 5 is outside the msdos primary range");

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

    // 分区里没有 FS：只改表必须显式声明（--no-fs），否则写表之前就拒绝——"类型识别不出"
    // 不等于"里面没东西"，不能让工具替用户下这个结论
    let raw_before = std::fs::read(&img).unwrap();
    let (c, o) = run(&["resize", &format!("{img_s}:1"), "+1M"]);
    assert_eq!(c, 10, "growing an unidentifiable region must refuse without --no-fs: {o}");
    assert!(o.contains("--no-fs"), "{o}");
    assert_eq!(std::fs::read(&img).unwrap(), raw_before, "refused resize must not write");

    // +delta --no-fs：分区 1 → 4096 扇区（恰好吃掉右侧空闲）
    let (c, o) = run(&["resize", &format!("{img_s}:1"), "+1M", "--no-fs"]);
    assert_eq!(c, 0, "{o}");
    let raw = std::fs::read(&img).unwrap();
    assert_eq!(slot(&raw, 1), (2048, 4096), "relative grow");
    assert_eq!(slot(&raw, 2), (6144, 2048), "partition 2 untouched");

    // 右侧已无空闲：grow 无法扩分区 ⇒ 拒绝（10），与 GPT 及在线路径同语义。
    // 不带 --no-fs 时先被"类型识别不出"拦下；两次都必须一字未写
    let raw_before = std::fs::read(&img).unwrap();
    let (c, o) = run(&["resize", &format!("{img_s}:1"), "grow"]);
    assert_eq!(c, 10, "grow must refuse an unidentifiable region without --no-fs: {o}");
    let (c, o) = run(&["resize", &format!("{img_s}:1"), "grow", "--no-fs"]);
    assert_eq!(c, 10, "grow with no free space to the right must refuse: {o}");
    assert!(o.contains("no free space"), "{o}");
    assert_eq!(std::fs::read(&img).unwrap(), raw_before, "refused grow must not write");
    let raw = std::fs::read(&img).unwrap();
    assert_eq!(slot(&raw, 1), (2048, 4096), "partition 1 unchanged");

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

    // :N 命中核验按表类型分派：MBR 分区也要能解析出来，不得按"无 GPT"拒绝。
    // 解析成功之后失败落在 FS 层：unknown 没有 check 工具 ⇒ 拒绝（10）——
    // 与"表里没这个分区"同为 10，靠文案区分（后者会说 no GPT / MBR covers slots）
    let (c, o) = run(&["check", &format!("{img_s}:1")]);
    assert_eq!(c, 10, "empty MBR partition must resolve, then be refused for lack of FS tooling: {o}");
    assert!(!o.contains("no GPT"), "{o}");
    let (c, o) = run(&["check", &format!("{img_s}:9")]);
    assert_eq!(c, 10, "out-of-range MBR slot must refuse: {o}");
    assert!(o.contains("MBR covers primary slots"), "{o}");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn new_msdos_over_gpt_is_recognized_as_msdos() {
    let dir = std::env::temp_dir().join(format!("diskedit_mbr_over_gpt_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let img = dir.join("g.img");
    std::fs::write(&img, vec![0u8; 16 * 1024 * 1024]).unwrap();
    let exe = env!("CARGO_BIN_EXE_DiskEdit");
    let img_s = img.to_str().unwrap();
    let run = |args: &[&str]| -> (i32, String) {
        let out = Command::new(exe).args(args).output().unwrap();
        (out.status.code().unwrap_or(-1), format!("{}\n{}",
            String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr)))
    };
    let (c, o) = run(&["new", img_s, "--yes"]);
    assert_eq!(c, 0, "{o}");
    // 覆盖已有 GPT：残留的主头签名必须一并清掉，否则这张新 MBR 盘会被判 GptDamaged
    let (c, o) = run(&["new", img_s, "--table", "msdos", "--yes"]);
    assert_eq!(c, 0, "{o}");
    let (c, o) = run(&["add", img_s, "--start", "2048", "--end", "4095", "--type", "0x83"]);
    assert_eq!(c, 0, "a stale GPT signature must not block writes after `new --table msdos`: {o}");
    let (c, o) = run(&["info", img_s]);
    assert_eq!(c, 0, "{o}");
    assert!(o.contains("\"label\":\"mbr\""), "{o}");
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

/// D5（C1）：set name 超过 36 个 UTF-16 码元必须拒绝（而非静默截断）；恰 36 个码元可接受
#[test]
fn set_name_over_36_code_units_is_refused() {
    let dir = std::env::temp_dir().join(format!("diskedit_nm36_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let img = dir.join("blank.img");
    std::fs::write(&img, vec![0u8; 8 * 1024 * 1024]).unwrap();
    let exe = env!("CARGO_BIN_EXE_DiskEdit");
    let img_s = img.to_str().unwrap();
    let run = |args: &[&str]| -> (i32, String, String) {
        let out = Command::new(exe).args(args).output().unwrap();
        (out.status.code().unwrap_or(-1),
         String::from_utf8_lossy(&out.stdout).into_owned(),
         String::from_utf8_lossy(&out.stderr).into_owned())
    };
    assert_eq!(run(&["new", img_s, "--yes"]).0, 0);
    assert_eq!(run(&["add", img_s, "--start", "2048", "--end", "4095", "--name", "root"]).0, 0);

    // 37 个 ASCII 字符 = 37 码元 > 36 → 拒绝，文案点明 UTF-16 码元
    let over = "a".repeat(37);
    let (c, _, err) = run(&["set", &format!("{img_s}:1"), "name", &over]);
    assert_eq!(c, 10, "a name over 36 UTF-16 code units must be refused: {err}");
    assert!(err.contains("UTF-16 code units"), "{err}");

    // 边界：恰 36 个码元应被接受
    let exact = "a".repeat(36);
    let (c, _, err) = run(&["set", &format!("{img_s}:1"), "name", &exact]);
    assert_eq!(c, 0, "a 36-code-unit name must be accepted: {err}");
    let _ = std::fs::remove_dir_all(&dir);
}