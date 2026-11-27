//! 用户可见文本的字符集守卫：帮助、版本与用法错误只输出 ASCII。
//!
//! 基线文案是英文；混入中文标点或全角字符会让脚本化调用与逐字对照失效。

use std::process::Command;

fn run(argv: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_diskedit"))
        .args(argv)
        .output()
        .unwrap_or_else(|error| panic!("运行 diskedit {argv:?} 失败: {error}"))
}

fn assert_ascii(argv: &[&str], output: &std::process::Output) {
    for (stream, bytes) in [("stdout", &output.stdout), ("stderr", &output.stderr)] {
        if let Some((at, byte)) = bytes.iter().enumerate().find(|(_, byte)| !byte.is_ascii()) {
            panic!(
                "diskedit {argv:?} 的 {stream} 第 {at} 字节是 0x{byte:02X}，用户可见文本必须为 ASCII"
            );
        }
    }
}

// covers: T14
#[test]
fn help_version_and_usage_are_ascii_only() {
    for argv in [
        vec!["--help"],
        vec!["--version"],
        vec!["layout"],
        vec!["resize", "--unknown-flag"],
        vec!["no-such-command"],
    ] {
        let output = run(&argv);
        assert_ascii(&argv, &output);
    }
}

// covers: T14
#[test]
fn every_command_help_is_ascii_only() {
    for command in diskedit::app::cli::declare::COMMANDS {
        let argv = vec![command.name, "--help"];
        let output = run(&argv);
        assert_ascii(&argv, &output);
        assert!(
            output.status.success(),
            "diskedit {} --help 退出码为 {:?}",
            command.name,
            output.status.code()
        );
    }
}

// covers: T14
#[test]
fn layout_json_is_ascii_only() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let image = root.join("target/fixtures/s1/gpt.img");
    let argv = vec!["layout", image.to_str().expect("路径应为 UTF-8"), "--json"];
    let output = run(&argv);
    assert_ascii(&argv, &output);
}
