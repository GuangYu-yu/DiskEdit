//! 端到端冒烟：直接跑构建出的二进制，覆盖建表、新增、查看、删除与用法错误。

use std::fs::File;
use std::process::Command;

fn image_path() -> std::path::PathBuf {
    let mut path = std::env::temp_dir();
    path.push(format!("diskedit-smoke-{}.img", std::process::id()));
    path
}

fn run(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_diskedit"))
        .args(args)
        .output()
        .expect("二进制应可启动")
}

#[test]
fn file_image_round_trip() {
    let path = image_path();
    let file = File::create(&path).expect("创建镜像应成功");
    file.set_len(64 * 1024 * 1024).expect("设置长度应成功");
    drop(file);
    let target = path.to_str().expect("临时路径应为 UTF-8");

    let output = run(&["new", target, "--table", "gpt", "--yes"]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");

    let output = run(&["add", target, "16M", "--name", "data", "--yes"]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");

    let output = run(&["inspect", target]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("\"partitions\":[{"), "应列出分区：{stdout}");

    let output = run(&["delete", &format!("{target}:1"), "--yes"]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");

    let output = run(&["delete", &format!("{target}:1")]);
    assert_eq!(output.status.code(), Some(2), "缺 --yes 应落用法错误");

    let output = run(&["info", target, "--bogus"]);
    assert_eq!(output.status.code(), Some(2), "未知选项应落用法错误");

    std::fs::remove_file(&path).ok();
}
