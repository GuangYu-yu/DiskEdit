use super::*;

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

/// info 的顶层键集跨 label 分支一致：none / mbr / gpt 都含
/// damaged、disk_guid、label、partitions、sector_size、size_bytes。
/// `last_part` 只在有可操作条目时出现（两分支同规则）。按同一 schema 消费 JSON 的调用方
/// 不该因盘型不同而缺键——少写一个字段不报错，只会让它拿到空值
#[test]
fn info_json_top_level_keys_are_consistent_across_label_branches() {
    let dir = std::env::temp_dir().join(format!("diskedit_keys_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let exe = env!("CARGO_BIN_EXE_DiskEdit");
    let run = |args: &[&str]| -> (i32, String) {
        let out = Command::new(exe).args(args).output().unwrap();
        (out.status.code().unwrap_or(-1), String::from_utf8_lossy(&out.stdout).into_owned())
    };
    let info = |img: &std::path::Path| -> serde_json::Value {
        let (c, out) = run(&["info", img.to_str().unwrap()]);
        assert_eq!(c, 0, "info must succeed: {out}");
        serde_json::from_str(out.trim()).expect("info must emit valid JSON")
    };
    let keys = |img: &std::path::Path| -> Vec<String> {
        let mut k: Vec<String> = info(img).as_object().unwrap().keys().cloned().collect();
        k.sort();
        k
    };
    let base: Vec<String> = ["damaged", "disk_guid", "label", "partitions", "sector_size", "size_bytes"]
        .iter()
        .map(|s| s.to_string())
        .collect();

    // none：既不是 GPT 也不是 MBR
    let none = dir.join("none.img");
    std::fs::write(&none, vec![0u8; 2 * 1024 * 1024]).unwrap();
    assert_eq!(keys(&none), base, "the `none` branch must carry the common key set");

    // mbr：空表 → 无 last_part；disk_guid 如实为 null（MBR 没有磁盘 GUID 概念）
    let mbr = dir.join("mbr.img");
    std::fs::write(&mbr, vec![0u8; 8 * 1024 * 1024]).unwrap();
    assert_eq!(run(&["new", mbr.to_str().unwrap(), "--table", "msdos", "--yes"]).0, 0);
    assert_eq!(keys(&mbr), base, "the `mbr` branch must carry the common key set");
    assert!(info(&mbr)["disk_guid"].is_null(), "mbr must report disk_guid as null");
    // 有可操作条目 → 两分支同规则地多出 last_part
    assert_eq!(run(&["add", mbr.to_str().unwrap(), "--start", "2048", "--end", "4095"]).0, 0);
    let mut with_last = base.clone();
    with_last.push("last_part".to_string());
    with_last.sort();
    assert_eq!(keys(&mbr), with_last, "an operable entry adds last_part");

    // gpt：同一套键；disk_guid 是 GUID 串
    let gpt = dir.join("gpt.img");
    std::fs::write(&gpt, vec![0u8; 8 * 1024 * 1024]).unwrap();
    assert_eq!(run(&["new", gpt.to_str().unwrap(), "--yes"]).0, 0);
    assert_eq!(keys(&gpt), base, "the `gpt` branch must carry the common key set");
    assert!(info(&gpt)["disk_guid"].is_string(), "gpt must report a GUID string");

    let _ = std::fs::remove_dir_all(&dir);
}

/// `:last` 指最后一个可操作分区（GPT 上即末端最靠后的已用条目）：`info` 报告 `last_part`，
/// 写命令上 `:last` 与具体号等价，而只报告整盘布局的命令明确拒绝分区指代
#[test]
fn last_partref_resolves_like_the_explicit_number() {
    let dir = std::env::temp_dir().join(format!("diskedit_last_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let exe = env!("CARGO_BIN_EXE_DiskEdit");
    let run = |args: &[&str]| -> (i32, String, String) {
        let out = Command::new(exe).args(args).output().unwrap();
        (out.status.code().unwrap_or(-1),
         String::from_utf8_lossy(&out.stdout).into_owned(),
         String::from_utf8_lossy(&out.stderr).into_owned())
    };

    let img = dir.join("a.img");
    std::fs::write(&img, vec![0u8; 8 * 1024 * 1024]).unwrap();
    let img_s = img.to_str().unwrap();
    assert_eq!(run(&["new", img_s, "--yes"]).0, 0);
    assert_eq!(run(&["add", img_s, "--start", "2048", "--end", "4095", "--name", "a"]).0, 0);
    assert_eq!(run(&["add", img_s, "--start", "4096", "--end", "6143", "--name", "b"]).0, 0);
    // 第二份逐字节副本：`:last` 与 `:2` 各自的收尾必须落到同一份字节上
    let img2 = dir.join("b.img");
    std::fs::copy(&img, &img2).unwrap();
    let img2_s = img2.to_str().unwrap();

    // 末端最靠后的是 #2
    let (c, out, e) = run(&["info", img_s]);
    assert_eq!(c, 0, "{e}");
    assert!(out.contains("\"last_part\":2"), "info must report the ending-last entry: {out}");

    // 只报告整盘布局的命令不接受分区指代，`:last` 同样被回显拒绝
    let (c, _, e) = run(&["info", &format!("{img_s}:last")]);
    assert_eq!(c, 10, "info must refuse :last: {e}");

    // 写命令：`:last` 与 `:2` 等价
    let (c, _, e) = run(&["delete", &format!("{img_s}:last"), "--yes"]);
    assert_eq!(c, 0, "delete :last must succeed: {e}");
    let (c, _, e) = run(&["delete", &format!("{img2_s}:2"), "--yes"]);
    assert_eq!(c, 0, "delete :2 must succeed: {e}");
    assert_eq!(
        std::fs::read(&img).unwrap(),
        std::fs::read(&img2).unwrap(),
        ":last and the explicit number must produce the same table"
    );

    // 删掉 #2 后末端最靠后的是 #1
    let (c, out, e) = run(&["info", img_s]);
    assert_eq!(c, 0, "{e}");
    assert!(out.contains("\"last_part\":1"), "{out}");

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

/// 重叠 GPT 镜像：合法 GPT 写入后，改盘上条目数组让两条条目重叠，再重算条目数组
/// CRC 与主头 CRC（与 DiskEdit 同算法，见 src/table.rs::crc32）。load_gpt 优先用主头、
/// 仅当其失效才回退备份——所以主头里重叠的条目也能被 info 读到并判重叠
fn overlapping_gpt_image(path: &std::path::Path) {
    let ss = 512u64;
    // 夹具要能先经 gptman 合法写出再人为改坏：镜像须装得下条目 2 的末端（LBA 90）
    // 加 GPT 尾部结构，100 扇区的镜像在 write_into 的几何校验处就过不去
    let data = vec![0u8; 2048 * ss as usize];
    let mut cur = Cursor::new(data);
    let mut gpt = gptman::GPT::new_from(&mut cur, ss, [0xAB; 16]).unwrap();
    gpt[1] = gptman::GPTPartitionEntry {
        partition_type_guid: [0x11; 16],
        unique_partition_guid: [0x22; 16],
        starting_lba: 34,
        ending_lba: 60,
        attribute_bits: 0,
        partition_name: "p1".into(),
    };
    gpt[2] = gptman::GPTPartitionEntry {
        partition_type_guid: [0x11; 16],
        unique_partition_guid: [0x33; 16],
        starting_lba: 64,
        ending_lba: 90,
        attribute_bits: 0,
        partition_name: "p2".into(),
    };
    gpt.write_into(&mut cur).unwrap();
    let mut data = cur.into_inner();
    // 条目数组在 LBA2（偏移 1024），每条 128 字节；把条目 2 的起始拉进条目 1 区间内
    let e2 = 1024 + 128;
    data[e2 + 32..e2 + 40].copy_from_slice(&40u64.to_le_bytes()); // starting_lba = 40（落在 34..60 内）
    data[e2 + 40..e2 + 48].copy_from_slice(&90u64.to_le_bytes()); // ending_lba = 90
    let crc = crc::Crc::<u32>::new(&crc::CRC_32_ISO_HDLC);
    // 重算条目数组 CRC（默认 128 条 × 128 字节）
    let array_crc = crc.checksum(&data[1024..1024 + 128 * 128]);
    data[512 + 88..512 + 92].copy_from_slice(&array_crc.to_le_bytes());
    // 重算主头 CRC：先清占位字段，对 HeaderSize 字节算 CRC 写回
    let hdr_size = u32::from_le_bytes(data[512 + 12..512 + 16].try_into().unwrap()) as usize;
    data[512 + 16..512 + 20].fill(0);
    let hdr_crc = crc.checksum(&data[512..512 + hdr_size]);
    data[512 + 16..512 + 20].copy_from_slice(&hdr_crc.to_le_bytes());
    std::fs::write(path, with_protective_mbr(data)).unwrap();
}

/// D4（C4）：info 对**重叠的 GPT** 必须报 damaged:true（与 MBR 的 MbrDamage::Overlap 口径对称），
/// 且重叠条目仍可见（诊断不能因受损而消失）
#[test]
fn info_reports_damaged_for_overlapping_gpt() {
    let dir = std::env::temp_dir().join(format!("diskedit_ovgpt_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let img = dir.join("overlap.img");
    overlapping_gpt_image(&img);
    let exe = env!("CARGO_BIN_EXE_DiskEdit");
    let out = Command::new(exe).args(["info", img.to_str().unwrap()]).output().unwrap();
    let s = String::from_utf8_lossy(&out.stdout);
    let v: serde_json::Value = serde_json::from_str(s.trim()).expect("info must emit valid JSON");
    assert_eq!(v["label"], "gpt", "label must still be gpt: {s}");
    assert_eq!(v["damaged"], serde_json::json!(true), "overlapping GPT must be reported damaged: {s}");
    assert!(v["partitions"].as_array().unwrap().len() >= 2, "overlapping entries must remain visible: {s}");
    let _ = std::fs::remove_dir_all(&dir);
}