//! 用例表执行器：逐条运行二进制并核对退出码与输出片段。

mod cases;
mod fixtures;

use std::process::Command;

use cases::{CASES, Case};

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
    let (code, stdout, _) = run_argv(&["info", gpt]);
    assert_eq!(code, 0);
    assert!(stdout.contains("\"label\":\"gpt\""), "{stdout}");

    let mbr = "target/fixtures/s2/flow-mbr.img";
    copy_fixture("blank.img", mbr);
    let (code, _, stderr) = run_argv(&["new", mbr, "--yes", "--table", "mbr"]);
    assert_eq!(code, 0, "空白镜像建 MBR 应成功：{stderr}");
    let (code, stdout, _) = run_argv(&["info", mbr]);
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
    let (code, stdout, _) = run_argv(&["info", occupied]);
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
    let (code, stdout, stderr) = run_argv(&["info", &gpt]);
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
    let (code, stdout, _) = run_argv(&["info", &gpt]);
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
    let (code, stdout, _) = run_argv(&["info", &gpt]);
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
    let (code, stdout, _) = run_argv(&["info", &gpt]);
    assert_eq!(code, 0);
    assert!(!stdout.contains("\"num\":3"), "{stdout}");

    let (code, stdout, stderr) = run_argv(&["add", &mbr, "512K", "--yes"]);
    assert_eq!(code, 0, "MBR 新增应成功：{stderr}");
    assert!(stdout.contains("added partition #2"), "{stdout}");
    let (code, stdout, _) = run_argv(&["info", &mbr]);
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
    let (code, stdout, _) = run_argv(&["info", &explicit]);
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
    let (code, stdout, _) = run_argv(&["info", &aligned]);
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
    let (code, stdout, _) = run_argv(&["info", &moved]);
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
    let (code, stdout, _) = run_argv(&["info", &copied]);
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
    let (code, stdout, _) = run_argv(&["info", &image]);
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

    let (code, _, _) = run_argv(&["resizefs", &format!("{image}:1"), "--yes"]);
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
#[test]
fn recover_refuses_a_target_without_a_record() {
    fixtures::prepare();
    let directory = "target/fixtures/s7";
    std::fs::create_dir_all(directory).expect("夹具目录可建");
    let image = "target/fixtures/s7/blank.img";
    copy_fixture("blank.img", image);
    let state = "target/fixtures/s7/state";
    drop(std::fs::remove_dir_all(state));
    let (code, _stdout, stderr) =
        run_argv_env(&["recover", image, "--yes"], &[("XDG_STATE_HOME", state)]);
    assert_eq!(code, 3, "没有记录应被拒绝：{stderr}");
    assert!(stderr.contains("no recovery record"), "{stderr}");
}

// covers: R0301
#[test]
fn recover_refuses_a_partition_selector() {
    let (code, _stdout, stderr) = run_argv(&["recover", "disk.img:1", "--yes"]);
    assert_eq!(code, 2, "recover 不接受分区选择器：{stderr}");
}
