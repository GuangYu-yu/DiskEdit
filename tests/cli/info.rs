//! `info` 的 Linux 端到端用例，覆盖六种场景。

use super::*;

fn run(args: &[&str]) -> (i32, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_DiskEdit"))
        .args(args)
        .output()
        .expect("spawn DiskEdit");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn info_json(img: &std::path::Path) -> serde_json::Value {
    let (code, out, err) = run(&["info", img.to_str().expect("utf-8 path")]);
    assert_eq!(code, 0, "info must succeed: {err}");
    serde_json::from_str(out.trim()).unwrap_or_else(|e| panic!("info must emit one JSON document: {e}\n{out}"))
}

/// GPT 镜像的只读报告：label / 扇区大小 / 条目名与类型 GUID 文本（磁盘字节序）
#[test]
fn info_on_gpt_image() {
    let dir = FixtureDir::new();
    let img = dir.join("disk.img");
    fixture_gpt_image(&img);

    let v = info_json(&img);
    assert_eq!(v["label"], "gpt");
    assert_eq!(v["sector_size"], 512);
    assert_eq!(v["partitions"][0]["name"], "smoke");
    assert_eq!(v["partitions"][0]["type"], "0657FD6D-A4AB-43C4-84E5-0933C84B4F4F");

    // 联动搬移预览里 swap 条目只重建、不搬数据：`plan --grow` 仍是只读报告
    let ss = 512u64;
    let data = vec![0u8; 200 * ss as usize];
    let mut cur = Cursor::new(data);
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

    let arg = format!("{}:1", img2.display());
    let (code, out, err) = run(&["plan", &arg, "--grow"]);
    assert_eq!(code, 0, "plan --grow must stay read-only and succeed: {err}");
    // 报告与预览同在一份文档里：stdout 必须是单个可解析的 JSON 对象
    let v: serde_json::Value = serde_json::from_str(out.trim()).expect("plan --grow must emit one JSON document");
    let moves = v["preview"]["moves"].as_array().expect("preview.moves must be an array");
    assert_eq!(moves[0]["swap"], true, "a swap area is recreated, not copied: {out}");
    assert_eq!(v["preview"]["grow_part"], 1);
    assert!(v["preview"]["new_end_lba"].is_u64(), "{out}");
}

/// 容器条目只说明「里面还有分区」，不说明有几个：观察侧必须点明列表不完整，
/// 且不得把「未解析」写成数量 0
#[test]
fn info_names_an_extended_container_as_unenumerated() {
    let dir = FixtureDir::new();
    let img = dir.join("mbr.img");
    let mut bytes = vec![0u8; 4 * 1024 * 1024];
    bytes[510] = 0x55;
    bytes[511] = 0xAA;
    bytes[446 + 4] = 0x05;
    bytes[446 + 8..446 + 12].copy_from_slice(&2048u32.to_le_bytes());
    bytes[446 + 12..446 + 16].copy_from_slice(&2048u32.to_le_bytes());
    std::fs::write(&img, &bytes).unwrap();

    let (code, out, err) = run(&["info", img.to_str().expect("utf-8 path")]);
    assert_eq!(code, 0, "{err}");
    let v: serde_json::Value = serde_json::from_str(out.trim()).expect("info must emit valid JSON");
    assert_eq!(v["label"], "mbr");
    assert_eq!(v["partitions"].as_array().map(|a| a.len()), Some(1));
    assert_eq!(v["partitions"][0]["type"], "0x05");
    assert!(err.contains("extended container"), "{err}");
    assert!(err.contains("not enumerated"), "{err}");
    assert!(!err.contains("0 logical"), "{err}");
}

/// 顶层键集跨 label 分支一致：none / mbr / gpt 都含那六个键；`last_part`
/// 只在有可操作条目时出现（两分支同规则），`disk_guid` 在 MBR 如实为 null
#[test]
fn info_json_top_level_keys_are_consistent_across_label_branches() {
    let dir = FixtureDir::new();
    let base: Vec<String> = [
        "damaged", "disk_guid", "label", "partitions", "sector_size", "size_bytes",
    ]
    .iter()
    .map(|s| (*s).to_string())
    .collect();
    let keys = |img: &std::path::Path| -> Vec<String> {
        let mut k: Vec<String> = info_json(img)
            .as_object()
            .expect("the envelope must be an object")
            .keys()
            .cloned()
            .collect();
        k.sort();
        k
    };

    // none：既不是 GPT 也不是 MBR
    let none = dir.join("none.img");
    std::fs::write(&none, vec![0u8; 2 * 1024 * 1024]).unwrap();
    assert_eq!(keys(&none), base, "the `none` branch must carry the common key set");

    // mbr：空表 → 无 last_part；disk_guid 如实为 null
    let mbr = dir.join("mbr.img");
    std::fs::write(&mbr, vec![0u8; 8 * 1024 * 1024]).unwrap();
    let p = mbr.to_str().expect("utf-8 path");
    let (code, _, err) = run(&["new", p, "--table", "mbr", "--yes"]);
    assert_eq!(code, 0, "setup `new --table mbr` failed: {err}");
    assert_eq!(keys(&mbr), base, "the `mbr` branch must carry the common key set");
    assert!(info_json(&mbr)["disk_guid"].is_null(), "mbr must report disk_guid as null");

    // 有可操作条目 → 两分支同规则地多出 last_part
    let (code, _, err) = run(&["add", p, "1M", "--yes"]);
    assert_eq!(code, 0, "setup `add` failed: {err}");
    let mut with_last = base.clone();
    with_last.push("last_part".to_string());
    with_last.sort();
    assert_eq!(keys(&mbr), with_last, "an operable entry adds last_part");

    // gpt：缺省表种类即 GPT；同一套键，disk_guid 是 GUID 串
    let gpt = dir.join("gpt.img");
    std::fs::write(&gpt, vec![0u8; 8 * 1024 * 1024]).unwrap();
    let p = gpt.to_str().expect("utf-8 path");
    let (code, _, err) = run(&["new", p, "--yes"]);
    assert_eq!(code, 0, "setup `new` failed: {err}");
    assert_eq!(keys(&gpt), base, "the `gpt` branch must carry the common key set");
    assert!(info_json(&gpt)["disk_guid"].is_string(), "gpt must report a GUID string");
}

/// `:last` 指末端最靠后的已用条目：`info` 报告 `last_part`，只报告整盘布局的
/// 命令明确拒绝分区指代（3），写命令上 `:last` 与具体号落到同一份字节
#[test]
fn last_partref_resolves_like_the_explicit_number() {
    let dir = FixtureDir::new();
    let img = dir.join("a.img");
    std::fs::write(&img, vec![0u8; 8 * 1024 * 1024]).unwrap();
    let p = img.to_str().expect("utf-8 path");
    assert_eq!(run(&["new", p, "--yes"]).0, 0, "setup `new` failed");
    assert_eq!(run(&["add", p, "1M", "--name", "a", "--yes"]).0, 0, "setup `add a` failed");
    assert_eq!(run(&["add", p, "1M", "--name", "b", "--yes"]).0, 0, "setup `add b` failed");

    let img2 = dir.join("b.img");
    std::fs::copy(&img, &img2).unwrap();
    let p2 = img2.to_str().expect("utf-8 path");

    let (code, out, err) = run(&["info", p]);
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("\"last_part\":2"), "info must report the ending-last entry: {out}");

    let (code, _, err) = run(&["info", &format!("{p}:last")]);
    assert_eq!(code, 3, "info must refuse :last: {err}");

    let (code, _, err) = run(&["delete", &format!("{p}:last"), "--yes"]);
    assert_eq!(code, 0, "delete :last must succeed: {err}");
    let (code, _, err) = run(&["delete", &format!("{p2}:2"), "--yes"]);
    assert_eq!(code, 0, "delete :2 must succeed: {err}");
    assert_eq!(
        std::fs::read(&img).unwrap(),
        std::fs::read(&img2).unwrap(),
        ":last and the explicit number must produce the same table"
    );

    let (code, out, err) = run(&["info", p]);
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("\"last_part\":1"), "{out}");
}

/// 真实扩容路径：truncate 预扩使 backup GPT 与保护 MBR 同时过期 —— `info`
/// 只报告、`plan --grow` 只预览（都不写盘），前置修复由表写入命令经执行器完成
#[test]
fn stale_after_enlarge_info_plan_write() {
    use std::io::Write as _;

    let dir = FixtureDir::new();
    let img = dir.join("grown.img");
    fixture_gpt_image(&img); // 100 扇区，分区 1 = 34..60
    {
        let mut f = std::fs::OpenOptions::new().append(true).open(&img).unwrap();
        f.write_all(&vec![0u8; 1024 * 1024]).unwrap();
    }
    let p = img.to_str().expect("utf-8 path");
    let total = std::fs::metadata(&img).unwrap().len() / 512;
    let before = std::fs::read(&img).unwrap();

    let (code, out, err) = run(&["info", p]);
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("\"label\":\"gpt\""), "{out}");
    assert!(err.contains("backup GPT header is stale"), "info must warn about stale backup GPT: {err}");
    assert!(err.contains("protective MBR SizeInLBA is stale"), "info must warn about stale PMBR: {err}");
    assert_eq!(std::fs::read(&img).unwrap(), before, "info must not modify the image");

    let arg = format!("{p}:1");
    let (code, out, err) = run(&["plan", &arg, "--grow"]);
    assert_eq!(code, 0, "plan --grow must stay read-only and succeed: {err}");
    let v: serde_json::Value = serde_json::from_str(out.trim()).expect("plan --grow must emit one JSON document");
    assert_eq!(v["preview"]["grow_part"], 1, "{out}");
    assert!(v["preview"]["new_end_lba"].is_u64(), "{out}");
    assert_eq!(std::fs::read(&img).unwrap(), before, "plan must not modify the image");

    let (code, _, err) = run(&["set", &format!("{p}:1"), "name", "renamed", "--yes"]);
    assert_eq!(code, 0, "the table write must carry the upfront repair: {err}");
    let (_, _, err) = run(&["info", p]);
    assert!(!err.contains("stale"), "nothing stale must remain: {err}");
    let after = std::fs::read(&img).unwrap();
    let pmbr_size = u32::from_le_bytes([after[446 + 12], after[446 + 13], after[446 + 14], after[446 + 15]]);
    assert_eq!(pmbr_size as u64, total - 1, "protective MBR SizeInLBA must match the container");
    let backup_lba = u64::from_le_bytes(after[512 + 32..512 + 40].try_into().unwrap());
    assert_eq!(backup_lba, total - 1, "backup GPT must sit at the device end");
}

/// 重叠 GPT 镜像：合法 GPT 写入后改条目数组让两条重叠，重算条目数组 CRC 与主头
/// CRC。`load_gpt` 优先用主头，故重叠条目能被 `info` 读到并判重叠
fn overlapping_gpt_image(path: &std::path::Path) {
    let ss = 512u64;
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
    // 条目数组在 LBA2（偏移 1024），每条 128 字节；把条目 2 的起点拉进条目 1 区间
    let e2 = 1024 + 128;
    data[e2 + 32..e2 + 40].copy_from_slice(&40u64.to_le_bytes());
    data[e2 + 40..e2 + 48].copy_from_slice(&90u64.to_le_bytes());
    let crc = crc::Crc::<u32>::new(&crc::CRC_32_ISO_HDLC);
    let array_crc = crc.checksum(&data[1024..1024 + 128 * 128]);
    data[512 + 88..512 + 92].copy_from_slice(&array_crc.to_le_bytes());
    let hdr_size = u32::from_le_bytes(data[512 + 12..512 + 16].try_into().unwrap()) as usize;
    data[512 + 16..512 + 20].fill(0);
    let hdr_crc = crc.checksum(&data[512..512 + hdr_size]);
    data[512 + 16..512 + 20].copy_from_slice(&hdr_crc.to_le_bytes());
    std::fs::write(path, with_protective_mbr(data)).unwrap();
}

/// 重叠 GPT 必须报 `damaged: true`（与 MBR 的 MbrDamage 口径对称），
/// 且重叠条目仍可见——诊断不能因受损而消失
#[test]
fn info_reports_damaged_for_overlapping_gpt() {
    let dir = FixtureDir::new();
    let img = dir.join("overlap.img");
    overlapping_gpt_image(&img);
    let v = info_json(&img);
    assert_eq!(v["label"], "gpt", "label must still be gpt");
    assert_eq!(v["damaged"], serde_json::json!(true), "overlapping GPT must be reported damaged");
    assert!(
        v["partitions"].as_array().is_some_and(|a| a.len() >= 2),
        "overlapping entries must remain visible"
    );
}
