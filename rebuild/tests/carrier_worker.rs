//! `__worker` 入口的最小会话：同一二进制担当来宾，握手后正常收尾。
//!
//! 真正在虚拟机里执行步骤依赖内核与 root，这里只核对协议与入口分流。

#![cfg(target_os = "linux")]

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

use serde_json::Value;

fn read_frame(reader: &mut BufReader<impl std::io::Read>) -> Value {
    let mut line = String::new();
    reader.read_line(&mut line).expect("读取帧");
    serde_json::from_str(line.trim_end()).expect("解析帧")
}

#[test]
fn worker_handshake_and_shutdown() {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|value| value.as_nanos())
        .unwrap_or(0);
    let image = std::env::temp_dir().join(format!("diskedit-worker-{nanos}.img"));
    std::fs::write(&image, vec![0u8; 1 << 20]).expect("创建镜像");
    let size = std::fs::metadata(&image).expect("镜像元数据").len();

    let mut child = Command::new(env!("CARGO_BIN_EXE_diskedit"))
        .arg("__worker")
        .env("DISKEDIT_WORKER_TOKEN", "token")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("启动工作进程");
    let mut stdin = child.stdin.take().expect("标准输入");
    let mut stdout = BufReader::new(child.stdout.take().expect("标准输出"));

    let boot = read_frame(&mut stdout);
    assert_eq!(boot["type"], "boot");
    let hello = serde_json::json!({
        "v": 3,
        "type": "hello",
        "token": "token",
        "binary_version": env!("CARGO_PKG_VERSION"),
        "target": image.display().to_string(),
        "sector_size": 512,
        "disk_size": size,
        "plan_digest": "00".repeat(32),
        "job": "test",
    });
    writeln!(stdin, "{hello}").expect("写握手");
    stdin.flush().expect("刷新");

    let ready = read_frame(&mut stdout);
    assert_eq!(ready["type"], "ready");
    assert_eq!(ready["disk_size"], size);
    assert_eq!(ready["sector_size"], 512);
    let sid = ready["sid"].as_str().expect("会话号").to_owned();

    let shutdown = serde_json::json!({ "v": 3, "type": "shutdown", "sid": sid });
    writeln!(stdin, "{shutdown}").expect("写收尾");
    stdin.flush().expect("刷新");
    let bye = read_frame(&mut stdout);
    assert_eq!(bye["type"], "bye");

    drop(stdin);
    let status = child.wait().expect("等待退出");
    assert!(status.success(), "工作进程应正常退出");
    std::fs::remove_file(&image).ok();
}
