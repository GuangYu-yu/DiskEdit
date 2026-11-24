//! 用例表执行器：逐条运行二进制并核对退出码与输出片段。

mod cases;

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

#[test]
fn every_case_matches_its_observable_result() {
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
