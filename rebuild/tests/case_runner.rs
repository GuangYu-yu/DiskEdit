//! 用例表执行器：逐条运行二进制并核对退出码与输出片段。

mod cases;
mod fixtures;

#[cfg(target_os = "linux")]
use std::os::unix::fs::MetadataExt;
#[cfg(target_os = "linux")]
use std::path::{Path, PathBuf};
use std::process::Command;

use cases::{CASES, Case};
#[cfg(target_os = "linux")]
use sha2::{Digest, Sha256};

fn run(case: &Case) -> (i32, String, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_diskedit"))
        .args(case.argv)
        .output()
        .unwrap_or_else(|error| panic!("运行 {} 失败: {error}", case.name));
    (
        output
            .status
            .code()
            .expect("子进程应正常退出而不是被信号终止"),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

fn run_argv(argv: &[&str]) -> (i32, String, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_diskedit"))
        .args(argv)
        .output()
        .unwrap_or_else(|error| panic!("运行 {argv:?} 失败: {error}"));
    (
        output.status.code().expect("子进程应正常退出"),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

/// 把空白或占用镜像复制成一次用例的私有目标。
fn copy_fixture(name: &str, destination: &str) {
    std::fs::copy(
        std::path::Path::new(fixtures::S2_BASE).join(name),
        destination,
    )
    .unwrap_or_else(|error| panic!("复制夹具 {name} 失败: {error}"));
}

// covers: B10, B12
#[test]
fn new_writes_a_readable_table_and_refuses_to_clobber() {
    fixtures::prepare();
    let gpt = "target/fixtures/s2/flow-gpt.img";
    copy_fixture("blank.img", gpt);
    let (code, stdout, stderr) = run_argv(&["new", gpt, "--yes"]);
    assert_eq!(code, 0, "空白镜像建 GPT 应成功：{stderr}");
    assert!(stdout.contains("created gpt table"), "{stdout}");
    let (code, stdout, _) = run_argv(&["layout", gpt, "--json"]);
    assert_eq!(code, 0);
    assert!(stdout.contains("\"label\":\"gpt\""), "{stdout}");

    let mbr = "target/fixtures/s2/flow-mbr.img";
    copy_fixture("blank.img", mbr);
    let (code, _, stderr) = run_argv(&["new", mbr, "--yes", "--table", "mbr"]);
    assert_eq!(code, 0, "空白镜像建 MBR 应成功：{stderr}");
    let (code, stdout, _) = run_argv(&["layout", mbr, "--json"]);
    assert_eq!(code, 0);
    assert!(stdout.contains("\"label\":\"mbr\""), "{stdout}");

    let occupied = "target/fixtures/s2/flow-data.img";
    copy_fixture("data.img", occupied);
    let before = std::fs::read(occupied).expect("读取夹具");
    let (code, _, _) = run_argv(&["new", occupied, "--yes"]);
    assert_eq!(code, 3, "未给 --overwrite-data 应拒绝");
    assert_eq!(
        std::fs::read(occupied).expect("读取夹具"),
        before,
        "拒绝时目标字节不得改变"
    );
    let (code, _, stderr) = run_argv(&["new", occupied, "--yes", "--overwrite-data"]);
    assert_eq!(code, 0, "显式覆盖应成功：{stderr}");
    let (code, stdout, _) = run_argv(&["layout", occupied, "--json"]);
    assert_eq!(code, 0);
    assert!(stdout.contains("\"label\":\"gpt\""), "{stdout}");

    let guarded = "target/fixtures/s2/flow-guard.img";
    copy_fixture("blank.img", guarded);
    let (code, _, _) = run_argv(&["new", guarded]);
    assert_eq!(code, 2, "缺 --yes 应是用法错误");
    assert_eq!(
        std::fs::read(guarded).expect("读取夹具"),
        std::fs::read(std::path::Path::new(fixtures::S2_BASE).join("blank.img"))
            .expect("读取空白夹具"),
        "缺 --yes 时目标字节不得改变"
    );
}

// covers: B11
#[test]
fn new_refuses_a_target_below_the_gpt_minimum() {
    fixtures::prepare();
    let directory = "target/fixtures/s2";
    std::fs::create_dir_all(directory).expect("写夹具目录可建");
    let small = format!("{directory}/below-min.img");
    std::fs::write(&small, vec![0u8; 67 * 512]).expect("过小镜像可写");
    let before = std::fs::read(&small).expect("读取镜像");
    let (code, _, stderr) = run_argv(&["new", &small, "--yes"]);
    assert_eq!(code, 3, "低于 GPT 最小容量必须拒绝：{stderr}");
    assert_eq!(
        std::fs::read(&small).expect("读取镜像"),
        before,
        "拒绝时盘面不变"
    );

    let exact = format!("{directory}/exact-min.img");
    std::fs::write(&exact, vec![0u8; 68 * 512]).expect("最小镜像可写");
    let (code, _, stderr) = run_argv(&["new", &exact, "--yes"]);
    assert_eq!(code, 0, "恰好最小容量应可建表：{stderr}");
}

// covers: B25
#[test]
fn new_refuses_an_inconsistent_protective_mbr_and_keeps_a_canonical_one() {
    fixtures::prepare();
    let directory = "target/fixtures/s2";
    std::fs::create_dir_all(directory).expect("写夹具目录可建");

    let inconsistent = format!("{directory}/pmbr-inconsistent.img");
    let mut bytes = vec![0u8; 4 * 1024 * 1024];
    bytes[446 + 4] = 0xEE;
    bytes[446 + 12..446 + 16].copy_from_slice(&u32::MAX.to_le_bytes());
    bytes[510] = 0x55;
    bytes[511] = 0xAA;
    std::fs::write(&inconsistent, &bytes).expect("镜像可写");
    let (code, _, stderr) = run_argv(&["new", &inconsistent, "--yes"]);
    assert_eq!(code, 3, "SizeInLBA 超出两种解释必须拒绝：{stderr}");
    assert_eq!(
        std::fs::read(&inconsistent).expect("读取镜像"),
        bytes,
        "拒绝时盘面不变"
    );

    let canonical = format!("{directory}/pmbr-canonical.img");
    std::fs::write(&canonical, vec![0u8; 4 * 1024 * 1024]).expect("镜像可写");
    let (code, _, stderr) = run_argv(&["new", &canonical, "--yes"]);
    assert_eq!(code, 0, "空白镜像建 GPT 应成功：{stderr}");
    let before = std::fs::read(&canonical).expect("读取镜像");
    let (code, _, stderr) = run_argv(&["add", &canonical, "1M", "--yes"]);
    assert_eq!(code, 0, "新增分区应成功：{stderr}");
    let after = std::fs::read(&canonical).expect("读取镜像");
    assert_eq!(&after[..512], &before[..512], "已合规的保护 MBR 不得重写");
}

// covers: B26
// covers: B12
#[test]
fn add_delete_and_set_rewrite_both_copies() {
    fixtures::prepare();
    let directory = "target/fixtures/s3";
    std::fs::create_dir_all(directory).expect("编辑夹具目录可建");
    let gpt = format!("{directory}/gpt.img");
    let mbr = format!("{directory}/mbr.img");
    std::fs::copy(std::path::Path::new(fixtures::BASE).join("gpt.img"), &gpt)
        .expect("复制 GPT 夹具");
    std::fs::copy(std::path::Path::new(fixtures::BASE).join("mbr.img"), &mbr)
        .expect("复制 MBR 夹具");

    let (code, stdout, stderr) = run_argv(&["add", &gpt, "1M", "--yes"]);
    assert_eq!(code, 0, "GPT 新增应成功：{stderr}");
    assert!(stdout.contains("added partition #3"), "{stdout}");
    let (code, stdout, stderr) = run_argv(&["layout", &gpt, "--json"]);
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("\"num\":3"), "{stdout}");
    assert!(stdout.contains("\"first_lba\":4096"), "{stdout}");
    let bytes = std::fs::read(&gpt).expect("读取镜像");
    assert_eq!(
        &bytes[bytes.len() - 512..bytes.len() - 504],
        b"EFI PART",
        "备份 GPT 头必须同步重写"
    );

    let (code, stdout, stderr) = run_argv(&["set", &format!("{gpt}:1"), "name", "rootfs", "--yes"]);
    assert_eq!(code, 0, "改 GPT 名字应成功：{stderr}");
    assert!(stdout.contains("renamed partition #1"), "{stdout}");
    let (code, stdout, _) = run_argv(&["layout", &gpt, "--json"]);
    assert_eq!(code, 0);
    assert!(stdout.contains("\"name\":\"rootfs\""), "{stdout}");

    let (code, _, stderr) = run_argv(&[
        "set",
        &format!("{gpt}:2"),
        "type",
        "C12A7328-F81F-11D2-BA4B-00A0C93EC93B",
        "--yes",
    ]);
    assert_eq!(code, 0, "改 GPT 类型应成功：{stderr}");
    let (code, stdout, _) = run_argv(&["layout", &gpt, "--json"]);
    assert_eq!(code, 0);
    assert!(
        stdout.contains("\"type\":\"C12A7328-F81F-11D2-BA4B-00A0C93EC93B\""),
        "{stdout}"
    );

    let (code, stdout, stderr) =
        run_argv(&["set", &format!("{gpt}:1"), "flag", "hidden", "on", "--yes"]);
    assert_eq!(code, 0, "改 GPT 旗标应成功：{stderr}");
    assert!(stdout.contains("flag hidden=on"), "{stdout}");

    let (code, stdout, stderr) = run_argv(&["delete", &format!("{gpt}:3"), "--yes"]);
    assert_eq!(code, 0, "GPT 删除应成功：{stderr}");
    assert!(stdout.contains("deleted partition #3"), "{stdout}");
    let (code, stdout, _) = run_argv(&["layout", &gpt, "--json"]);
    assert_eq!(code, 0);
    assert!(!stdout.contains("\"num\":3"), "{stdout}");

    let (code, stdout, stderr) = run_argv(&["add", &mbr, "512K", "--yes"]);
    assert_eq!(code, 0, "MBR 新增应成功：{stderr}");
    assert!(stdout.contains("added partition #2"), "{stdout}");
    let (code, stdout, _) = run_argv(&["layout", &mbr, "--json"]);
    assert_eq!(code, 0);
    assert!(stdout.contains("\"num\":2"), "{stdout}");

    let (code, stdout, stderr) =
        run_argv(&["set", &format!("{mbr}:1"), "flag", "boot", "on", "--yes"]);
    assert_eq!(code, 0, "MBR 启动标志应成功：{stderr}");
    assert!(stdout.contains("flag boot=on"), "{stdout}");

    let (code, stdout, stderr) = run_argv(&["delete", &format!("{mbr}:2"), "--yes"]);
    assert_eq!(code, 0, "MBR 删除应成功：{stderr}");
    assert!(stdout.contains("deleted partition #2"), "{stdout}");

    let before = std::fs::read(&gpt).expect("读取镜像");
    let (code, _, _) = run_argv(&["add", &gpt, "4M", "--yes"]);
    assert_eq!(code, 3, "放不下的新增应拒绝");
    assert_eq!(
        std::fs::read(&gpt).expect("读取镜像"),
        before,
        "拒绝时不得改字节"
    );

    let explicit = format!("{directory}/explicit.img");
    std::fs::copy(
        std::path::Path::new(fixtures::BASE).join("gpt.img"),
        &explicit,
    )
    .expect("复制 GPT 夹具");
    let (code, _, stderr) = run_argv(&["add", &explicit, "1M", "--start", "5000", "--yes"]);
    assert_eq!(code, 0, "显式起点应成功：{stderr}");
    let (code, stdout, _) = run_argv(&["layout", &explicit, "--json"]);
    assert_eq!(code, 0);
    assert!(stdout.contains("\"first_lba\":5000"), "{stdout}");

    let aligned = format!("{directory}/aligned.img");
    std::fs::copy(
        std::path::Path::new(fixtures::BASE).join("gpt.img"),
        &aligned,
    )
    .expect("复制 GPT 夹具");
    let (code, _, stderr) = run_argv(&[
        "add", &aligned, "300K", "--start", "5000", "--align", "mib", "--yes",
    ]);
    assert_eq!(code, 0, "对齐起点应成功：{stderr}");
    let (code, stdout, _) = run_argv(&["layout", &aligned, "--json"]);
    assert_eq!(code, 0);
    assert!(stdout.contains("\"first_lba\":6144"), "{stdout}");
}

/// 在一张镜像的指定绝对扇区写入可辨认的标记字节。
fn stamp_at(image: &str, sector: u64, marker: &[u8]) {
    let mut bytes = std::fs::read(image).expect("读取镜像");
    let offset = sector as usize * 512;
    bytes[offset..offset + marker.len()].copy_from_slice(marker);
    std::fs::write(image, bytes).expect("写回镜像");
}

/// 读回镜像指定绝对扇区的若干字节。
fn read_at(image: &str, sector: u64, len: usize) -> Vec<u8> {
    let bytes = std::fs::read(image).expect("读取镜像");
    let offset = sector as usize * 512;
    bytes[offset..offset + len].to_vec()
}

// covers: B14, B21, D8, R0033
#[test]
fn move_and_copy_relocate_data_and_keep_type_guids() {
    fixtures::prepare();
    let directory = "target/fixtures/s4";
    std::fs::create_dir_all(directory).expect("搬移夹具目录可建");

    // 搬移：#2（3072..4095）平移到 5120，分区内容随数据步一起搬到新位置。
    let moved = format!("{directory}/move.img");
    std::fs::copy(std::path::Path::new(fixtures::BASE).join("gpt.img"), &moved)
        .expect("复制 GPT 夹具");
    let marker = *b"DATA-MARK";
    // #2 起点之后第 28 个扇区，搬移后应出现在 5120 + 28 处。
    stamp_at(&moved, 3100, &marker);
    let (code, stdout, stderr) =
        run_argv(&["move", &format!("{moved}:2"), "--start", "5120", "--yes"]);
    assert_eq!(code, 0, "搬移应成功：{stderr}");
    assert!(stdout.contains("moved partition #2"), "{stdout}");
    let (code, stdout, _) = run_argv(&["layout", &moved, "--json"]);
    assert_eq!(code, 0);
    assert!(stdout.contains("\"first_lba\":5120"), "{stdout}");
    assert!(stdout.contains("\"last_lba\":6143"), "{stdout}");
    // D8：搬移不改写已有分区的类型 GUID。
    assert!(
        stdout.contains("\"type\":\"0FC63DAF-8483-4772-8E79-3D69D8477DE4\""),
        "搬移后 #2 的类型 GUID 应保持不变：{stdout}"
    );
    assert!(
        stdout.contains("\"type\":\"C12A7328-F81F-11D2-BA4B-00A0C93EC93B\""),
        "未参与搬移的 #1 类型 GUID 应保持不变：{stdout}"
    );
    // B14/B21：内容确实搬到新起点。
    assert_eq!(
        read_at(&moved, 5148, marker.len()),
        marker,
        "搬移后的分区内容缺失"
    );

    // 复制：#1（2048..3071）内容复制到新槽位 6144，源保持不变。
    let copied = format!("{directory}/copy.img");
    std::fs::copy(
        std::path::Path::new(fixtures::BASE).join("gpt.img"),
        &copied,
    )
    .expect("复制 GPT 夹具");
    stamp_at(&copied, 2050, &marker);
    let (code, stdout, stderr) = run_argv(&[
        "copy",
        &format!("{copied}:1"),
        "--start",
        "6144",
        "--name",
        "clone",
        "--yes",
    ]);
    assert_eq!(code, 0, "复制应成功：{stderr}");
    assert!(stdout.contains("copied to partition #3"), "{stdout}");
    let (code, stdout, _) = run_argv(&["layout", &copied, "--json"]);
    assert_eq!(code, 0);
    assert!(stdout.contains("\"first_lba\":6144"), "{stdout}");
    assert!(stdout.contains("\"name\":\"clone\""), "{stdout}");
    // 副本继承源类型 GUID（D8 的“不改写用户已有类型”）。
    assert!(
        stdout.contains("\"type\":\"C12A7328-F81F-11D2-BA4B-00A0C93EC93B\""),
        "副本应继承源类型 GUID：{stdout}"
    );
    // 源与副本都保留内容。
    assert_eq!(
        read_at(&copied, 2050, marker.len()),
        marker,
        "复制不得移动源内容"
    );
    assert_eq!(read_at(&copied, 6146, marker.len()), marker, "副本内容缺失");

    // 越界的搬移应拒绝且不改字节。
    let guarded = format!("{directory}/guard.img");
    std::fs::copy(
        std::path::Path::new(fixtures::BASE).join("gpt.img"),
        &guarded,
    )
    .expect("复制 GPT 夹具");
    let before = std::fs::read(&guarded).expect("读取镜像");
    let (code, _, _) = run_argv(&["move", &format!("{guarded}:2"), "--start", "3072", "--yes"]);
    assert_eq!(code, 0, "起点相同的搬移应是 no-op 成功");
    let (code, _, _) = run_argv(&["move", &format!("{guarded}:2"), "--start", "8000", "--yes"]);
    assert_eq!(code, 3, "越过可用区的搬移应拒绝");
    assert_eq!(
        std::fs::read(&guarded).expect("读取镜像"),
        before,
        "拒绝时不得改字节"
    );
}

// covers: B13, B26, R0059, R0203, R0209
// covers: B13, R0209
#[test]
fn resize_rewrites_the_table_and_refuses_unsafe_shrinks() {
    fixtures::prepare();
    let directory = "target/fixtures/s6";
    std::fs::create_dir_all(directory).expect("扩缩夹具目录可建");
    let image = format!("{directory}/gpt.img");
    std::fs::copy(std::path::Path::new(fixtures::BASE).join("gpt.img"), &image)
        .expect("复制 GPT 夹具");
    let original = std::fs::read(&image).expect("读取夹具");

    // 用法错误：缺 SIZE/--end、同给两者、--end 缺分区选择器。
    let (code, _, stderr) = run_argv(&["resize", &format!("{image}:2"), "--yes"]);
    assert_eq!(code, 2, "缺尺寸应是用法错误：{stderr}");
    let (code, _, _) = run_argv(&[
        "resize",
        &format!("{image}:2"),
        "8M",
        "--end",
        "5000",
        "--yes",
    ]);
    assert_eq!(code, 2, "SIZE 与 --end 互斥");
    let (code, _, _) = run_argv(&["resize", &image, "--end", "5000", "--yes"]);
    assert_eq!(code, 2, "--end 需要分区选择器");
    let (code, _, _) = run_argv(&["resize", &format!("{image}:2"), "grow"]);
    assert_eq!(code, 2, "缺 --yes 应是用法错误");
    assert_eq!(
        std::fs::read(&image).expect("读取夹具"),
        original,
        "用法错误不得改字节"
    );

    // 扩到右侧空闲末端：partition #2 由 4095 延伸到 last usable 8158。
    let (code, stdout, stderr) =
        run_argv(&["resize", &format!("{image}:2"), "grow", "--no-fs", "--yes"]);
    assert_eq!(code, 0, "只改表的扩容应成功：{stderr}");
    assert!(stdout.contains("resized partition #2"), "{stdout}");
    let (code, stdout, _) = run_argv(&["layout", &image, "--json"]);
    assert_eq!(code, 0);
    assert!(stdout.contains("\"last_lba\":8158"), "{stdout}");

    // 收缩表项但没有先缩文件系统：拒绝，且不改字节。
    let grown = std::fs::read(&image).expect("读取夹具");
    let (code, _, _) = run_argv(&[
        "resize",
        &format!("{image}:2"),
        "--end",
        "5000",
        "--no-fs",
        "--yes",
    ]);
    assert_eq!(code, 3, "--no-fs 不得收缩");
    // 越过后端与越过可用区都拒绝。
    let (code, _, _) = run_argv(&[
        "resize",
        &format!("{image}:2"),
        "--end",
        "99999",
        "--no-fs",
        "--yes",
    ]);
    assert_eq!(code, 3, "终点越过可用区应拒绝");
    assert_eq!(
        std::fs::read(&image).expect("读取夹具"),
        grown,
        "拒绝时目标字节不得改变"
    );

    // 干跑：打印计划但不写字节。
    let dry = format!("{directory}/dry.img");
    std::fs::copy(std::path::Path::new(fixtures::BASE).join("gpt.img"), &dry)
        .expect("复制 GPT 夹具");
    let before = std::fs::read(&dry).expect("读取夹具");
    let (code, stdout, stderr) = run_argv(&[
        "resize",
        &format!("{dry}:2"),
        "--end",
        "6000",
        "--no-fs",
        "--dry-run",
    ]);
    assert_eq!(code, 0, "干跑应成功：{stderr}");
    assert!(stdout.contains("dry run"), "{stdout}");
    assert!(stdout.contains("6000"), "{stdout}");
    assert_eq!(
        std::fs::read(&dry).expect("读取夹具"),
        before,
        "干跑不得改字节"
    );

    // `--align mib` 把终点下取整到 1 MiB 边界（6000 -> 4096）。
    let aligned = format!("{directory}/aligned.img");
    std::fs::copy(
        std::path::Path::new(fixtures::BASE).join("gpt.img"),
        &aligned,
    )
    .expect("复制 GPT 夹具");
    let (code, stdout, stderr) = run_argv(&[
        "resize",
        &format!("{aligned}:2"),
        "--end",
        "6000",
        "--align",
        "mib",
        "--no-fs",
        "--dry-run",
    ]);
    assert_eq!(code, 0, "对齐干跑应成功：{stderr}");
    assert!(stdout.contains("4096"), "对齐后的终点应是 4096：{stdout}");

    // 内容未识别时默认要扩文件系统：拒绝。
    let unknown = format!("{directory}/unknown.img");
    std::fs::copy(
        std::path::Path::new(fixtures::BASE).join("gpt.img"),
        &unknown,
    )
    .expect("复制 GPT 夹具");
    let before = std::fs::read(&unknown).expect("读取夹具");
    let (code, _, _) = run_argv(&["resize", &format!("{unknown}:2"), "--end", "6000", "--yes"]);
    assert_eq!(code, 3, "内容未识别时不得臆测文件系统");
    assert_eq!(
        std::fs::read(&unknown).expect("读取夹具"),
        before,
        "拒绝时目标字节不得改变"
    );
}

// covers: R0347, R0365, R0438, R0439, R0440, R0441, R0442, R0443
#[test]
fn resize_refuses_sizes_that_overflow_or_fall_below_a_sector() {
    fixtures::prepare();
    let directory = "target/fixtures/s6";
    std::fs::create_dir_all(directory).expect("扩缩夹具目录可建");
    let image = format!("{directory}/overflow.img");
    std::fs::copy(std::path::Path::new(fixtures::BASE).join("gpt.img"), &image)
        .expect("复制 GPT 夹具");
    let original = std::fs::read(&image).expect("读取夹具");
    let target = format!("{image}:2");

    // `+N`/`-N` 越过 u64 可表达的范围：SIZE 溢出，退 3（R0438/R0439）。
    // 同一段还覆盖百分比形态：先乘后除的结果超出 u64（R0440），以及百分比收缩
    // 超过当前尺寸（R0442）。R0441 是百分比放大在 checked_add 处的同类溢出，
    // 与 R0440 同文案、同退出码；当前分区不足 1 MiB 时被 1 MiB 下取整遮蔽，按同类认领。
    for size in [
        "+18446744073709551615",
        "-18446744073709551615",
        "+18446744073709551615%",
        "-100000000%",
    ] {
        let (code, _, stderr) = run_argv(&["resize", &target, size, "--no-fs", "--yes"]);
        assert_eq!(code, 3, "SIZE {size} 应拒绝：{stderr}");
    }

    // 绝对 SIZE 不足一个扇区：退 3（R0443）。
    let (code, _, stderr) = run_argv(&["resize", &target, "1", "--no-fs", "--yes"]);
    assert_eq!(code, 3, "不足一个扇区应拒绝：{stderr}");

    // 绝对 SIZE 越出 LBA 区间：退 3（R0347）；干跑走同一判据（R0365）。
    for dry_run in [false, true] {
        let mut argv = vec![
            "resize",
            target.as_str(),
            "18446744073709551615",
            "--no-fs",
            "--yes",
        ];
        if dry_run {
            argv.push("--dry-run");
        }
        let (code, _, stderr) = run_argv(&argv);
        assert_eq!(
            code, 3,
            "SIZE 越出 LBA 区间应拒绝（dry_run={dry_run}）：{stderr}"
        );
    }

    assert_eq!(
        std::fs::read(&image).expect("读取夹具"),
        original,
        "拒绝时目标字节不得改变"
    );
}

// covers: R0341, R0342, R0348, R0349
#[test]
fn resize_refuses_grow_without_room_and_sizes_past_the_usable_range() {
    fixtures::prepare();
    let directory = "target/fixtures/s6";
    std::fs::create_dir_all(directory).expect("扩缩夹具目录可建");
    let image = format!("{directory}/no-room.img");
    std::fs::copy(std::path::Path::new(fixtures::BASE).join("gpt.img"), &image)
        .expect("复制 GPT 夹具");
    let target = |n: u32| format!("{image}:{n}");

    // 顶到 last usable 后再 `grow`：拒绝，退 3（R0341）。
    let (code, _, stderr) = run_argv(&["resize", &target(2), "grow", "--no-fs", "--yes"]);
    assert_eq!(code, 0, "首次扩容应成功：{stderr}");
    let after_grow = std::fs::read(&image).expect("读取夹具");
    let (code, _, stderr) = run_argv(&["resize", &target(2), "grow", "--no-fs", "--yes"]);
    assert_eq!(code, 3, "已顶到 last usable 的 grow 应拒绝：{stderr}");
    assert_eq!(
        std::fs::read(&image).expect("读取夹具"),
        after_grow,
        "拒绝时目标字节不得改变"
    );

    // 右侧被邻接分区挡住（#1 的右边是 #2）：`grow` 拒绝，退 3（R0342）。
    let (code, _, stderr) = run_argv(&["resize", &target(1), "grow", "--no-fs", "--yes"]);
    assert_eq!(code, 3, "右侧被占的 grow 应拒绝：{stderr}");

    // 绝对 SIZE 换算后越出可用区：拒绝，退 3（R0348）。
    let before = std::fs::read(&image).expect("读取夹具");
    let (code, _, stderr) = run_argv(&["resize", &target(2), "99999999", "--no-fs", "--yes"]);
    assert_eq!(code, 3, "SIZE 越出可用区应拒绝：{stderr}");

    // 绝对 SIZE 需要的增长超过右侧空闲：拒绝，退 3（R0349）。
    let (code, _, stderr) = run_argv(&["resize", &target(1), "2M", "--no-fs", "--yes"]);
    assert_eq!(code, 3, "增长超过右侧空闲应拒绝：{stderr}");
    assert_eq!(
        std::fs::read(&image).expect("读取夹具"),
        before,
        "拒绝时目标字节不得改变"
    );
}

// covers: R0329, R0372, R0373, R0394, R0395, R0444, R0452
#[test]
fn resize_refuses_mbr_relocation_flags_and_no_table_targets() {
    fixtures::prepare();
    let directory = "target/fixtures/s6";
    std::fs::create_dir_all(directory).expect("扩缩夹具目录可建");

    // MBR 主表没有搬移通道：`--allow-move` 与 `--chunk` 均按拒绝处理，退 3。
    let mbr = format!("{directory}/mbr-flags.img");
    std::fs::copy(std::path::Path::new(fixtures::BASE).join("mbr.img"), &mbr)
        .expect("复制 MBR 夹具");
    let before = std::fs::read(&mbr).expect("读取夹具");
    let (code, _, stderr) = run_argv(&[
        "resize",
        &format!("{mbr}:1"),
        "grow",
        "--allow-move",
        "--no-fs",
        "--yes",
    ]);
    assert_eq!(code, 3, "MBR 不接受 --allow-move：{stderr}");
    let (code, _, stderr) = run_argv(&[
        "resize",
        &format!("{mbr}:1"),
        "+1M",
        "--chunk",
        "1",
        "--no-fs",
        "--yes",
    ]);
    assert_eq!(code, 3, "MBR 不接受 --chunk：{stderr}");
    assert_eq!(
        std::fs::read(&mbr).expect("读取夹具"),
        before,
        "拒绝时目标字节不得改变"
    );

    // 无分区表的目标：没有可扩缩的表，拒绝（退 3）；分区与整盘两种形态同判据。
    let empty = format!("{directory}/no-table.img");
    std::fs::copy(
        std::path::Path::new(fixtures::BASE).join("empty.img"),
        &empty,
    )
    .expect("复制空夹具");
    let (code, _, stderr) =
        run_argv(&["resize", &format!("{empty}:1"), "grow", "--no-fs", "--yes"]);
    assert_eq!(code, 3, "无表目标应拒绝：{stderr}");
    let (code, _, stderr) = run_argv(&["resize", &format!("{empty}:1"), "1M", "--no-fs", "--yes"]);
    assert_eq!(code, 3, "无表目标应拒绝：{stderr}");
}

// covers: B15, R0064, R0094
#[test]
fn filesystem_commands_refuse_without_touching_the_target() {
    fixtures::prepare();
    let directory = "target/fixtures/s5";
    std::fs::create_dir_all(directory).expect("文件系统夹具目录可建");
    let image = format!("{directory}/blank-content.img");
    std::fs::copy(std::path::Path::new(fixtures::BASE).join("gpt.img"), &image)
        .expect("复制带分区的 GPT 夹具");
    let original = std::fs::read(&image).expect("读取夹具");

    let (code, _, _) = run_argv(&["mkfs", &format!("{image}:1"), "ext4"]);
    assert_eq!(code, 2, "缺 --yes 应是用法错误");

    let (code, _, _) = run_argv(&["mkfs", &format!("{image}:1"), "bogus", "--yes"]);
    assert_eq!(code, 2, "未知文件系统应是用法错误");

    let (code, _, _) = run_argv(&["mkfs", &format!("{image}:5"), "ext4", "--yes"]);
    assert_eq!(code, 3, "不存在的分区应拒绝");

    let (code, _, _) = run_argv(&["resize", &format!("{image}:1"), "--fs-only", "--yes"]);
    assert_eq!(code, 3, "内容未识别时不得臆测文件系统");

    assert_eq!(
        std::fs::read(&image).expect("读取夹具"),
        original,
        "拒绝时目标字节不得改变"
    );
}

#[test]
fn every_case_matches_its_observable_result() {
    fixtures::prepare();
    for case in CASES {
        let (code, stdout, stderr) = run(case);
        assert_eq!(code, i32::from(case.exit), "{} 的退出码不符", case.name);
        if !case.stdout.is_empty() {
            assert!(
                stdout.contains(case.stdout),
                "{} 的 stdout 缺少 `{}`；实际：{stdout}",
                case.name,
                case.stdout
            );
        }
        if !case.stderr.is_empty() {
            assert!(
                stderr.contains(case.stderr),
                "{} 的 stderr 缺少 `{}`；实际：{stderr}",
                case.name,
                case.stderr
            );
        }
    }
}

/// 以指定的环境变量运行二进制；用于把记录根目录隔离到夹具内。
fn run_argv_env(argv: &[&str], env: &[(&str, &str)]) -> (i32, String, String) {
    let mut command = Command::new(env!("CARGO_BIN_EXE_diskedit"));
    command.args(argv);
    for (key, value) in env {
        command.env(key, value);
    }
    let output = command
        .output()
        .unwrap_or_else(|error| panic!("运行 {argv:?} 失败: {error}"));
    (
        output.status.code().expect("子进程应正常退出"),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

// covers: R0302
// covers: R0851, R0853, R0855, R0857, R0859
#[test]
fn undo_refuses_a_target_without_a_record() {
    fixtures::prepare();
    let directory = "target/fixtures/s7";
    std::fs::create_dir_all(directory).expect("夹具目录可建");
    let image = "target/fixtures/s7/blank.img";
    copy_fixture("blank.img", image);
    let state = "target/fixtures/s7/state";
    drop(std::fs::remove_dir_all(state));
    let (code, _stdout, stderr) =
        run_argv_env(&["undo", image, "--yes"], &[("XDG_STATE_HOME", state)]);
    assert_eq!(code, 3, "没有记录应被拒绝：{stderr}");
    assert!(stderr.contains("no recovery record"), "{stderr}");
}

// covers: R0301
#[test]
fn undo_refuses_a_partition_selector() {
    let (code, _stdout, stderr) = run_argv(&["undo", "disk.img:1", "--yes"]);
    assert_eq!(code, 2, "undo 不接受分区选择器：{stderr}");
}

// covers: D9
#[test]
fn read_only_layout_emits_one_structured_json_document() {
    // 只读命令统一输出本实现的结构化 JSON：单个对象、字段集稳定。
    let (code, stdout, _) = run_argv(&["layout", "target/fixtures/s1/gpt.img", "--json"]);
    assert_eq!(code, 0);
    let trimmed = stdout.trim();
    assert!(
        trimmed.starts_with('{') && trimmed.ends_with('}'),
        "{stdout}"
    );
    for key in ["\"label\":\"gpt\"", "\"disk_guid\":", "\"partitions\":["] {
        assert!(stdout.contains(key), "缺少字段 {key}：{stdout}");
    }
}

// 只读命令不得改动目标任何一个字节（parted NEWS：只读操作曾改写混合 MBR）。
#[test]
fn read_only_commands_leave_the_target_bytes_unchanged() {
    let path = "target/fixtures/s1/gpt.img";
    let before = std::fs::read(path).expect("读取夹具");
    let (code, _stdout, stderr) = run_argv(&["layout", path, "--json"]);
    assert_eq!(code, 0, "只读 layout 应成功：{stderr}");
    assert_eq!(
        std::fs::read(path).expect("读取夹具"),
        before,
        "只读命令改动了目标字节"
    );
}

// 无盘标的目标只报告空表，不崩溃、不写盘（parted disk_set 曾在无盘标时崩溃）。
#[test]
fn layout_reports_an_empty_table_without_crashing() {
    let path = "target/fixtures/s1/empty.img";
    let before = std::fs::read(path).expect("读取夹具");
    let (code, stdout, stderr) = run_argv(&["layout", path, "--json"]);
    assert_eq!(code, 0, "无表目标应正常报告：{stderr}");
    assert!(stdout.contains("\"label\":\"none\""), "{stdout}");
    assert!(stdout.contains("\"partitions\":[]"), "{stdout}");
    assert_eq!(
        std::fs::read(path).expect("读取夹具"),
        before,
        "info 改动了目标"
    );
}

/// 读 GPT 主分区项数组里第 `index` 个条目的 PARTUUID（从 1 计）。
fn entry_partuuid(bytes: &[u8], index: usize) -> [u8; 16] {
    let table =
        u64::from_le_bytes(bytes[512 + 72..512 + 80].try_into().expect("表头完整")) as usize * 512;
    let offset = table + (index - 1) * 128 + 16;
    bytes[offset..offset + 16]
        .try_into()
        .expect("分区项在镜像内")
}

// 复制出的分区必须有自己的 PARTUUID（sfdisk 还原式复制会撞 UUID）；同一目标重复复制
// 到同一槽位必须得到同一个值（派生不含随机源）。
#[test]
fn a_copied_partition_gets_its_own_deterministic_partuuid() {
    fixtures::prepare();
    let directory = "target/fixtures/s4";
    std::fs::create_dir_all(directory).expect("复制夹具目录可建");
    let path = format!("{directory}/uuid.img");
    std::fs::copy(std::path::Path::new(fixtures::BASE).join("gpt.img"), &path)
        .expect("复制 GPT 夹具");
    let source = entry_partuuid(&std::fs::read(&path).expect("读取镜像"), 1);
    let copy = |start: &str| {
        let (code, _stdout, stderr) =
            run_argv(&["copy", &format!("{path}:1"), "--start", start, "--yes"]);
        assert_eq!(code, 0, "复制应成功：{stderr}");
        entry_partuuid(&std::fs::read(&path).expect("读取镜像"), 3)
    };
    let first = copy("6144");
    assert_ne!(first, source, "副本不得复用源的 PARTUUID");
    let (code, _stdout, stderr) = run_argv(&["delete", &format!("{path}:3"), "--yes"]);
    assert_eq!(code, 0, "删除副本应成功：{stderr}");
    assert_eq!(copy("6144"), first, "同一槽位的重复复制应得到同一 PARTUUID");
}

// covers: R0358, R0359, R0360
#[test]
fn resize_refuses_a_target_without_a_table_or_a_missing_partition() {
    let (code, _stdout, stderr) =
        run_argv(&["resize", "target/fixtures/s1/empty.img", "32M", "--yes"]);
    assert_eq!(code, 3, "无表目标应拒绝：{stderr}");
    assert!(
        stderr.contains("target exceeds device/image size"),
        "{stderr}"
    );

    let (code, _stdout, stderr) =
        run_argv(&["resize", "target/fixtures/s1/mbr.img:9", "32M", "--yes"]);
    assert_eq!(code, 3, "缺失分区应拒绝：{stderr}");
    assert!(stderr.contains("does not exist"), "{stderr}");
}

// covers: D3
#[test]
fn check_reports_the_reason_it_cannot_act() {
    // 检查器的失败原因必须可见：既给结论，也给成因与下一步。
    let (code, _stdout, stderr) = run_argv(&["check", "target/fixtures/s1/gpt.img:1"]);
    assert_eq!(code, 6, "未识别内容应不支持：{stderr}");
    assert!(stderr.contains("unsupported:"), "{stderr}");
    assert!(stderr.contains("filesystem type unrecognized"), "{stderr}");
}

/// 按 Linux 宿主的同一算式（`file\0` + dev + ino + len 的 SHA-256）取镜像指纹。
#[cfg(target_os = "linux")]
fn image_fingerprint(image: &str) -> diskedit_core::fingerprint::Fingerprint {
    let metadata = std::fs::metadata(image).expect("镜像可读");
    let mut hasher = Sha256::new();
    hasher.update(b"file\0");
    hasher.update(metadata.dev().to_le_bytes());
    hasher.update(metadata.ino().to_le_bytes());
    hasher.update(metadata.len().to_le_bytes());
    diskedit_core::fingerprint::Fingerprint::strong(hasher.finalize().into())
}

/// 记录目录与宿主一致：`<XDG_STATE_HOME>/diskedit/images/<指纹前 16 字节>`。
#[cfg(target_os = "linux")]
fn record_state_dir(state: &str, image: &str) -> PathBuf {
    let fingerprint = image_fingerprint(image);
    let name: String = fingerprint.bytes()[..16]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    Path::new(state).join("diskedit").join("images").join(name)
}

#[cfg(target_os = "linux")]
fn record_body(
    target: diskedit_core::fingerprint::Fingerprint,
) -> diskedit_core::record::RecordBody {
    diskedit_core::record::RecordBody {
        target,
        geometry_generation: 1,
        plan_digest: [0u8; 32],
    }
}

// covers: R0149, R0150, R0151, R0152, R0153, R0154
#[cfg(target_os = "linux")]
#[test]
fn records_show_refuses_a_damaged_or_foreign_record() {
    fixtures::prepare();
    let directory = "target/fixtures/s7/records-show";
    drop(std::fs::remove_dir_all(directory));
    std::fs::create_dir_all(directory).expect("夹具目录可建");
    let image = format!("{directory}/gpt.img");
    std::fs::copy(Path::new(fixtures::BASE).join("gpt.img"), &image).expect("复制夹具");
    let state = format!("{directory}/state");
    drop(std::fs::remove_dir_all(&state));
    let record_dir = record_state_dir(&state, &image);
    std::fs::create_dir_all(&record_dir).expect("记录目录可建");
    let record = record_dir.join("record");

    let body = record_body(image_fingerprint(&image));
    let valid = diskedit_core::record::encode(&body, &[]).expect("记录可编码");

    // 截断：头部声明长度大于实际字节。
    std::fs::write(&record, &valid[..valid.len() - 4]).expect("记录可写");
    let (code, _stdout, stderr) =
        run_argv_env(&["records", "show", &image], &[("XDG_STATE_HOME", &state)]);
    assert_eq!(code, 3, "截断记录必须拒绝：{stderr}");
    assert!(
        stderr.contains("shorter than its declared length"),
        "{stderr}"
    );

    // 校验不符：翻转载荷中间的一个字节。
    let mut corrupt = valid.clone();
    let at = corrupt.len() / 2;
    corrupt[at] ^= 0xFF;
    std::fs::write(&record, &corrupt).expect("记录可写");
    let (code, _stdout, stderr) =
        run_argv_env(&["records", "show", &image], &[("XDG_STATE_HOME", &state)]);
    assert_eq!(code, 3, "校验不符必须拒绝：{stderr}");
    assert!(stderr.contains("CRC does not match"), "{stderr}");

    // 格式不认：长度足够但标识不是本实现的格式。
    std::fs::write(&record, vec![0u8; 200]).expect("记录可写");
    let (code, _stdout, stderr) =
        run_argv_env(&["records", "show", &image], &[("XDG_STATE_HOME", &state)]);
    assert_eq!(code, 3, "格式不认必须拒绝：{stderr}");
    assert!(stderr.contains("not recognized"), "{stderr}");

    // 外来：合法记录，但属于另一个目标。
    let foreign = diskedit_core::record::encode(
        &record_body(diskedit_core::fingerprint::Fingerprint::strong([0x42; 32])),
        &[],
    )
    .expect("记录可编码");
    std::fs::write(&record, &foreign).expect("记录可写");
    let (code, _stdout, stderr) =
        run_argv_env(&["records", "show", &image], &[("XDG_STATE_HOME", &state)]);
    assert_eq!(code, 3, "外来记录必须拒绝：{stderr}");
    assert!(stderr.contains("belongs to a different target"), "{stderr}");

    // 非普通文件：记录位置被目录占住，读不出字节。
    std::fs::remove_file(&record).expect("记录可删");
    std::fs::create_dir(&record).expect("目录可建");
    let (code, _stdout, stderr) =
        run_argv_env(&["records", "show", &image], &[("XDG_STATE_HOME", &state)]);
    assert_eq!(code, 3, "非普通记录条目必须拒绝：{stderr}");
    assert!(stderr.contains("cannot be decoded"), "{stderr}");
}
// covers: R0674
#[test]
fn resize_allow_move_relocates_the_blocking_partition() {
    fixtures::prepare();
    let directory = "target/fixtures/s6";
    std::fs::create_dir_all(directory).expect("扩缩夹具目录可建");
    let image = format!("{directory}/allow-move.img");
    std::fs::copy(std::path::Path::new(fixtures::BASE).join("gpt.img"), &image)
        .expect("复制 GPT 夹具");

    // 右侧本来就空时 `--allow-move` 没有可搬对象：按基线拒绝，退 3（R0674）。
    let (code, _, stderr) = run_argv(&[
        "resize",
        &format!("{image}:2"),
        "grow",
        "--allow-move",
        "--no-fs",
        "--yes",
    ]);
    assert_eq!(code, 3, "右侧空闲时 --allow-move 应拒绝：{stderr}");

    // `grow --allow-move`：右侧的 #2 被整体右移，#1 吃到盘尾余量。
    let (code, stdout, stderr) = run_argv(&[
        "resize",
        &format!("{image}:1"),
        "grow",
        "--allow-move",
        "--no-fs",
        "--yes",
    ]);
    assert_eq!(code, 0, "带 --allow-move 的扩容应成功：{stderr}");
    assert!(
        stdout.contains("relocated 1 blocking partition"),
        "应报告搬移了挡路分区：{stdout}"
    );

    let (code, stdout, _) = run_argv(&["layout", &image, "--json"]);
    assert_eq!(code, 0);
    assert!(
        stdout.contains("\"first_lba\":2048,\"fs\":\"unknown\",\"last_lba\":7134"),
        "{stdout}"
    );
    assert!(
        stdout.contains("\"first_lba\":7135,\"fs\":\"unknown\",\"last_lba\":8158"),
        "{stdout}"
    );
}
// covers: R0445, R0446, R0447, R0448, R0449, R0450
#[test]
fn superfloppy_resize_grows_the_filesystem_and_refuses_unextendable_requests() {
    // 造一个"文件系统即整盘"的现场需要 mke2fs；没有它的宿主跳过（由发版环境验证）。
    if std::process::Command::new("mke2fs")
        .arg("-V")
        .output()
        .is_err()
    {
        return;
    }
    let directory = "target/fixtures/s6";
    std::fs::create_dir_all(directory).expect("扩缩夹具目录可建");
    let image = format!("{directory}/superfloppy.img");
    std::fs::File::create(&image)
        .and_then(|file| file.set_len(8 << 20))
        .expect("创建 8 MiB 镜像");
    let status = std::process::Command::new("mke2fs")
        .args(["-F", "-q", "-t", "ext4", "-b", "1024", &image, "4096"])
        .status()
        .expect("运行 mke2fs");
    assert!(status.success(), "mke2fs 应成功");

    // 文件系统只占 4 MiB：grow 把它撑到整盘 8 MiB。
    let (code, stdout, stderr) = run_argv(&["resize", &image, "grow", "--yes"]);
    assert_eq!(code, 0, "superfloppy 扩容应成功：{stderr}");
    assert!(stdout.contains("superfloppy: ext grown"), "{stdout}");
    let (code, stdout, _) = run_argv(&["layout", &image, "--json"]);
    assert_eq!(code, 0);
    assert!(stdout.contains("\"size_bytes\":8388608"), "{stdout}");

    // 无分区边界可缩、超过容器、以及各旗标的"无事可做"都按拒绝处理，退 3。
    for argv in [
        vec!["resize", image.as_str(), "4M", "--yes"],
        vec!["resize", image.as_str(), "16M", "--yes"],
        vec!["resize", image.as_str(), "grow", "--no-fs", "--yes"],
        vec!["resize", image.as_str(), "grow", "--allow-move", "--yes"],
        vec!["resize", image.as_str(), "grow", "--chunk", "2", "--yes"],
        vec!["resize", image.as_str(), "grow", "--grow-lv", "--yes"],
    ] {
        let (code, _, stderr) = run_argv(&argv);
        assert_eq!(code, 3, "{argv:?} 应拒绝：{stderr}");
    }
}

/// sfdisk 在 GPT 上执行 `-A`（激活）会告警并自动进入嵌套 PMBR，把 legacy 启动位写进
/// 保护 MBR（实测 util-linux 2.39.3：打印 "Activation is unsupported for GPT --
/// entering nested PMBR."，并在 PMBR 写 0x80 + 0x01/0xFF 两条记录）。本实现不采纳这条
/// 隐式决定：`set flag boot` 落在 GPT 的 ESP 类型 GUID 上，legacy active 与 GPT 旗标
/// 分开建模，保护 MBR 保持规范形态。处置理由见 incident-classes.tsv SA1-015。
#[test]
fn a_gpt_boot_flag_becomes_the_esp_type_and_leaves_the_protective_mbr_alone() {
    fixtures::prepare();
    let directory = "target/fixtures/sa1-015";
    std::fs::create_dir_all(directory).expect("夹具目录可建");
    let image = format!("{directory}/gpt.img");
    std::fs::write(&image, vec![0u8; 8 * 1024 * 1024]).expect("镜像可写");
    let (code, _, stderr) = run_argv(&["new", &image, "--yes"]);
    assert_eq!(code, 0, "建 GPT 应成功：{stderr}");
    let (code, _, stderr) = run_argv(&["add", &image, "1M", "--yes"]);
    assert_eq!(code, 0, "加分区应成功：{stderr}");

    let before = std::fs::read(&image).expect("读取镜像");
    let (code, stdout, stderr) =
        run_argv(&["set", &format!("{image}:1"), "flag", "boot", "on", "--yes"]);
    assert_eq!(code, 0, "GPT boot 旗标应成功：{stderr}");
    assert!(stdout.contains("flag boot=on"), "{stdout}");
    let after = std::fs::read(&image).expect("读取镜像");

    assert_eq!(&after[..512], &before[..512], "改 GPT 旗标不得写保护 MBR");
    assert_eq!(after[446], 0x00, "保护 MBR 的启动位必须保持 0");
    assert_eq!(after[450], 0xEE, "保护 MBR 的类型必须仍是 0xEE");

    let (code, stdout, _) = run_argv(&["layout", &image, "--json"]);
    assert_eq!(code, 0);
    assert!(
        stdout.contains("C12A7328-F81F-11D2-BA4B-00A0C93EC93B"),
        "boot 旗标应落到 ESP 类型 GUID：{stdout}"
    );
}

/// 一份带两个分区的 GPT，返回镜像路径与已写入的字节。
fn two_partition_gpt(image: &str) {
    std::fs::write(image, vec![0u8; 32 * 1024 * 1024]).expect("镜像可写");
    let (code, _, stderr) = run_argv(&["new", image, "--yes"]);
    assert_eq!(code, 0, "建 GPT 应成功：{stderr}");
    for _ in 0..2 {
        let (code, _, stderr) = run_argv(&["add", image, "4M", "--yes"]);
        assert_eq!(code, 0, "加分区应成功：{stderr}");
    }
}

/// 混合 MBR（GPT 的 PMBR 里带一条 legacy 条目）在 gdisk 里被识别并保留；本实现只读
/// 时接受原样，任何表写都把 PMBR 收敛为规范的单一 0xEE 全覆盖记录。处置理由见
/// incident-classes.tsv SA3-002。
#[test]
fn a_hybrid_mbr_is_read_as_is_and_normalized_only_on_a_table_write() {
    fixtures::prepare();
    let directory = "target/fixtures/sa3-hybrid";
    std::fs::create_dir_all(directory).expect("夹具目录可建");
    let image = format!("{directory}/hybrid.img");
    two_partition_gpt(&image);

    let mut bytes = std::fs::read(&image).expect("读取镜像");
    let sector = 512usize;
    let first = u64::from_le_bytes(bytes[1024 + 32..1024 + 40].try_into().unwrap());
    let last = u64::from_le_bytes(bytes[1024 + 40..1024 + 48].try_into().unwrap());
    let total = (bytes.len() / sector) as u64;
    // 第 0 条：一条 legacy Linux 记录（0x0C）覆盖分区 1，并置启动位。
    bytes[446] = 0x80;
    bytes[450] = 0x0C;
    bytes[454..458].copy_from_slice(&u32::try_from(first).unwrap().to_le_bytes());
    bytes[458..462].copy_from_slice(&u32::try_from(last - first + 1).unwrap().to_le_bytes());
    // 第 1 条：0xEE 保护记录覆盖整盘。
    bytes[462] = 0x00;
    bytes[466] = 0xEE;
    bytes[470..474].copy_from_slice(&1u32.to_le_bytes());
    bytes[474..478].copy_from_slice(&u32::try_from(total - 1).unwrap().to_le_bytes());
    bytes[478..510].fill(0);
    bytes[510] = 0x55;
    bytes[511] = 0xAA;
    std::fs::write(&image, &bytes).expect("写回混合 MBR");
    let before = std::fs::read(&image).expect("读取混合 MBR");

    let (code, _, stderr) = run_argv(&["layout", &image, "--json"]);
    assert_eq!(code, 0, "混合 MBR 应可读：{stderr}");
    assert_eq!(
        std::fs::read(&image).expect("读取镜像"),
        before,
        "只读 info 不得重写混合 MBR"
    );

    let (code, _, stderr) = run_argv(&["set", &format!("{image}:1"), "name", "hybrid", "--yes"]);
    assert_eq!(code, 0, "表写应成功：{stderr}");
    let after = std::fs::read(&image).expect("读取镜像");
    assert_eq!(after[446 + 4], 0xEE, "表写应把保护 MBR 收敛为 0xEE");
    assert_eq!(
        &after[446 + 8..446 + 12],
        &1u32.to_le_bytes(),
        "保护记录从 LBA 1 开始"
    );
    assert_eq!(
        &after[446 + 12..446 + 16],
        &u32::try_from(total - 1).unwrap().to_le_bytes(),
        "保护记录覆盖整盘"
    );
    assert!(
        after[462..510].iter().all(|&byte| byte == 0),
        "混合条目应在规范化时清除"
    );
}

/// 盘上已有的重复 PARTUUID：gdisk 用专家命令 `f` 显式重随机化；本实现读取时保留，
/// 任何表写只改用户点名的条目，不静默改写身份数据（同 D8 原则）。处置理由见
/// incident-classes.tsv SA3-001。
#[test]
fn duplicate_partuuids_are_read_and_preserved_not_rewritten() {
    fixtures::prepare();
    let directory = "target/fixtures/sa3-dup";
    std::fs::create_dir_all(directory).expect("夹具目录可建");
    let image = format!("{directory}/dup.img");
    two_partition_gpt(&image);

    let mut bytes = std::fs::read(&image).expect("读取镜像");
    let sector = 512usize;
    let count = u32::from_le_bytes(bytes[512 + 80..512 + 84].try_into().unwrap()) as usize;
    let entry_size = u32::from_le_bytes(bytes[512 + 84..512 + 88].try_into().unwrap()) as usize;
    let backup_lba = u64::from_le_bytes(bytes[512 + 32..512 + 40].try_into().unwrap()) as usize;
    let primary_array =
        u64::from_le_bytes(bytes[512 + 72..512 + 80].try_into().unwrap()) as usize * sector;
    let backup_header = backup_lba * sector;
    let backup_array = u64::from_le_bytes(
        bytes[backup_header + 72..backup_header + 80]
            .try_into()
            .unwrap(),
    ) as usize
        * sector;
    // 让第 2 条复用第 1 条的 PARTUUID，并同时修正两处数组与两个表头的 CRC。
    let shared: [u8; 16] = bytes[primary_array + 16..primary_array + 32]
        .try_into()
        .unwrap();
    bytes[primary_array + entry_size + 16..primary_array + entry_size + 32]
        .copy_from_slice(&shared);
    bytes[backup_array + entry_size + 16..backup_array + entry_size + 32].copy_from_slice(&shared);
    let logical = count * entry_size;
    let crc = diskedit_core::table::crc32(&bytes[primary_array..primary_array + logical]);
    bytes[512 + 88..512 + 92].copy_from_slice(&crc.to_le_bytes());
    bytes[backup_header + 88..backup_header + 92].copy_from_slice(&crc.to_le_bytes());
    for header in [512usize, backup_header] {
        let size = u32::from_le_bytes(bytes[header + 12..header + 16].try_into().unwrap()) as usize;
        bytes[header + 16..header + 20].copy_from_slice(&[0, 0, 0, 0]);
        let header_crc = diskedit_core::table::crc32(&bytes[header..header + size]);
        bytes[header + 16..header + 20].copy_from_slice(&header_crc.to_le_bytes());
    }
    std::fs::write(&image, &bytes).expect("写回重复身份");
    let duplicated = std::fs::read(&image).expect("读取镜像");

    let (code, stdout, stderr) = run_argv(&["layout", &image, "--json"]);
    assert_eq!(code, 0, "重复 PARTUUID 应可读：{stderr}");
    assert!(stdout.contains("\"num\":2"), "{stdout}");
    assert_eq!(
        std::fs::read(&image).expect("读取镜像"),
        duplicated,
        "只读 layout 不得改写盘面"
    );

    let (code, _, stderr) = run_argv(&["set", &format!("{image}:2"), "name", "two", "--yes"]);
    assert_eq!(code, 0, "编辑应成功：{stderr}");
    let after = std::fs::read(&image).expect("读取镜像");
    assert_eq!(
        &after[primary_array + 16..primary_array + 32],
        &shared[..],
        "重复身份不得被静默改写"
    );
    assert_eq!(
        &after[primary_array + entry_size + 16..primary_array + entry_size + 32],
        &shared[..],
        "重复身份不得被静默改写"
    );
}
