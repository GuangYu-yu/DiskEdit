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
