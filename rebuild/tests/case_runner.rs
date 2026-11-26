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
