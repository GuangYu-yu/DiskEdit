//! 同机载体协议：真实 `__worker` 子进程，NDJSON 帧走 stdin/stdout。
//!
//! 这里覆盖的是协议本身的承诺：检查点必须等到 ack 才推进、断点按已 ack 的游标续跑、
//! 探测失败不把 `applied` 变成已提交、worker 不碰记录目录、噪声与坏帧不改变会话语义。
//! 搬移步骤用镜像上的自重叠小位移驱动，不需要 root 与外部工具。

#![cfg(target_os = "linux")]

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread;
use std::time::Duration;

use serde_json::{json, Value};

const TOKEN: &str = "carrier-test-token";
/// 帧版本，与 `src/exec/protocol.rs::VERSION` 同值：集成测试没有库目标，取不到那个常量，
/// 版本一升这里必须跟着改——这也是"协议变了"在测试侧的显式痕迹
const VERSION: u64 = 3;
const SECTOR: u64 = 512;
const DISK_BYTES: usize = 8 * 1024 * 1024;
const DISK_SECTORS: u64 = DISK_BYTES as u64 / SECTOR;

/// 自重叠小位移：位移 128 个扇区（64KiB），源与目的相交，块收敛到位移本身，
/// 块大小远小于一个刷新批次，逐块报检查点
const SRC_FIRST: u64 = 2048;
const DST_FIRST: u64 = SRC_FIRST + 128;
const SECTORS: u64 = 8192;
const CHUNK: u64 = 64 * 1024;
const CHUNK_BYTES: u64 = 1024 * 1024;
const CHUNKS: u64 = SECTORS / (CHUNK / SECTOR);

fn pattern(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i % 251) as u8).collect()
}

fn fixture(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "diskedit_worker_{tag}_{}",
        std::process::id()
    ));
    // 上一次同名残留可能不存在，也可能正被自己占用；两者都不影响新建
    std::fs::remove_dir_all(&dir).ok();
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("disk.img");
    let mut bytes = vec![0u8; DISK_BYTES];
    let start = SRC_FIRST as usize * SECTOR as usize;
    let len = SECTORS as usize * SECTOR as usize;
    bytes[start..start + len].copy_from_slice(&pattern(len));
    std::fs::write(&path, &bytes).unwrap();
    path
}

fn assert_moved(image: &Path) {
    let bytes = std::fs::read(image).unwrap();
    let start = DST_FIRST as usize * SECTOR as usize;
    let len = SECTORS as usize * SECTOR as usize;
    let moved = &bytes[start..start + len];
    let expected = pattern(len);
    if let Some(at) = moved.iter().zip(&expected).position(|(a, b)| a != b) {
        panic!(
            "the moved bytes differ at offset {at} (sector {}, byte-in-sector {})",
            at / SECTOR as usize,
            at % SECTOR as usize
        );
    }
}

/// 一帧的读取：协议帧与噪声分开，噪声在测试里只用于确认它被跳过
struct Worker {
    child: Child,
    stdin: ChildStdin,
    frames: Receiver<std::io::Result<String>>,
    /// 握手应答里签发的会话号；之后的每一帧都回带它（见 [`Worker::send`]）
    sid: Option<String>,
}

impl Worker {
    fn start(token: &str, extra: &[(&str, &Path)]) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_DiskEdit"));
        child
            .arg("__worker")
            .env("DISKEDIT_WORKER_TOKEN", token)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped());
        for (key, value) in extra {
            child.env(key, value);
        }
        let mut child = child.spawn().expect("spawn __worker");
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let (tx, frames) = mpsc::channel();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let failed = line.is_err();
                if tx.send(line).is_err() || failed {
                    return;
                }
            }
        });
        Self { child, stdin, frames, sid: None }
    }

    /// 与生产侧同一套：握手之后每一帧都回带会话号。测试里手搓帧也走这条规则，
    /// 否则测到的是"没有会话边界的协议"
    fn send(&mut self, frame: &Value) {
        let mut frame = frame.clone();
        if let Some(sid) = &self.sid
            && frame.get("type").and_then(Value::as_str) != Some("hello")
        {
            frame["sid"] = json!(sid);
        }
        writeln!(self.stdin, "{frame}").unwrap();
        self.stdin.flush().unwrap();
    }

    /// 不补会话号地发一帧：用来验证"接在后面的陌生人"会被拒
    fn send_as_stranger(&mut self, frame: &Value) {
        writeln!(self.stdin, "{frame}").unwrap();
        self.stdin.flush().unwrap();
    }

    /// 认领握手应答里签发的会话号。之后的帧都回带它；不认领就等于冒充陌生人
    fn adopt_session(&mut self, ready: &Value) {
        self.sid = Some(
            ready["sid"]
                .as_str()
                .unwrap_or_else(|| panic!("the ready frame names its session: {ready}"))
                .to_string(),
        );
    }

    /// 跳过噪声行；超时返回 `None`，关闭返回 `None`
    fn try_frame(&mut self, timeout: Duration) -> Option<Value> {
        loop {
            match self.frames.recv_timeout(timeout) {
                Ok(Ok(line)) => {
                    let line = line.trim();
                    if !line.starts_with('{') {
                        continue;
                    }
                    return Some(serde_json::from_str(line).expect("a frame is JSON"));
                }
                Ok(Err(e)) => panic!("cannot read the worker output: {e}"),
                Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => return None,
            }
        }
    }

    fn frame(&mut self) -> Value {
        self.try_frame(Duration::from_secs(30)).expect("the worker sent no frame")
    }

    fn shutdown(mut self) {
        self.send(&json!({ "v": VERSION, "type": "shutdown" }));
        let bye = self.frame();
        assert_eq!(bye["type"].as_str(), Some("bye"), "{bye}");
        let status = self.child.wait().unwrap();
        assert_eq!(status.code(), Some(0), "the worker did not exit cleanly");
    }

    fn kill(mut self) {
        self.child.kill().unwrap();
        // 收尸只是避免留下僵尸进程，退出码在这里没有意义
        self.child.wait().ok();
    }
}

fn start_ready(image: &Path, extra: &[(&str, &Path)]) -> Worker {
    let mut worker = Worker::start(TOKEN, extra);
    let size = std::fs::metadata(image).unwrap().len();
    worker.send(&json!({
        "v": VERSION,
        "type": "hello",
        "token": TOKEN,
        "binary_version": env!("CARGO_PKG_VERSION"),
        "target": image.to_string_lossy(),
        "sector_size": SECTOR,
        "disk_size": size,
        "job": "carrier protocol test",
    }));
    let ready = worker.frame();
    assert_eq!(ready["type"].as_str(), Some("ready"), "{ready}");
    worker.adopt_session(&ready);
    worker
}

fn zero_state() -> Value {
    json!({
        "entry": 0,
        "chunks_in_entry": 0,
        "completed_steps": 0,
        "cursor": 0,
        "digest": "00".repeat(32),
    })
}

fn relocate_step(id: u64, state: &Value) -> Value {
    json!({
        "v": VERSION,
        "type": "step",
        "id": id,
        "step": {
            "kind": "relocate",
            "part": 1,
            "src_first": SRC_FIRST,
            "dst_first": DST_FIRST,
            "sectors": SECTORS,
            "chunk_bytes": CHUNK_BYTES,
            "chunk": CHUNK,
            "chunks": CHUNKS,
        },
        "state": state,
    })
}

fn ack(id: u64, cursor: u64) -> Value {
    json!({ "v": VERSION, "type": "ack", "id": id, "cursor": cursor })
}

/// 未 ack 的检查点之后不得再有帧：游标推进以宿主的 ack 为界
#[test]
fn ack_before_advance() {
    let image = fixture("ack");
    let mut worker = start_ready(&image, &[]);
    worker.send(&relocate_step(1, &zero_state()));

    let mut checkpoints = 0u64;
    loop {
        let frame = worker.frame();
        match frame["type"].as_str() {
            Some("note") => continue,
            Some("checkpoint") => {
                checkpoints += 1;
                let cursor = frame["state"]["cursor"].as_u64().expect("the checkpoint records its cursor");
                assert!(
                    worker.try_frame(Duration::from_millis(300)).is_none(),
                    "the worker moved past checkpoint {cursor} before the host acknowledged it"
                );
                worker.send(&ack(1, cursor));
            }
            Some("result") => {
                assert_eq!(frame["outcome"].as_str(), Some("applied"), "{frame}");
                break;
            }
            other => panic!("unexpected frame {other:?}: {frame}"),
        }
    }
    assert_eq!(checkpoints, CHUNKS, "every chunk must report its own checkpoint");
    worker.shutdown();
    assert_moved(&image);
}

/// 检查点已到、宿主尚未 ack 时被杀：重启后从最后 ack 的游标继续，已拷贝的块
/// 不因重启而重写，最终目的区间与一次跑完相同
#[test]
fn resume_after_kill_before_ack() {
    let image = fixture("resume");
    let mut first = start_ready(&image, &[]);
    first.send(&relocate_step(1, &zero_state()));

    let acked = loop {
        let frame = first.frame();
        if frame["type"] == "note" {
            continue;
        }
        assert_eq!(frame["type"].as_str(), Some("checkpoint"), "{frame}");
        let cursor = frame["state"]["cursor"].as_u64().expect("cursor");
        first.send(&ack(1, cursor));
        break frame["state"].clone();
    };
    assert_eq!(acked["cursor"].as_u64(), Some(1));

    // 第二个检查点发出之后、ack 之前杀掉：这一块没有进入宿主的已确认进度
    let unacked = loop {
        let frame = first.frame();
        if frame["type"] == "note" {
            continue;
        }
        break frame;
    };
    assert_eq!(unacked["type"].as_str(), Some("checkpoint"), "{unacked}");
    assert_eq!(unacked["state"]["cursor"].as_u64(), Some(2));
    first.kill();

    let mut resumed = start_ready(&image, &[]);
    resumed.send(&relocate_step(1, &acked));
    let mut checkpoints = 0u64;
    loop {
        let frame = resumed.frame();
        match frame["type"].as_str() {
            Some("note") => continue,
            Some("checkpoint") => {
                checkpoints += 1;
                let cursor = frame["state"]["cursor"].as_u64().expect("cursor");
                resumed.send(&ack(1, cursor));
            }
            Some("result") => {
                assert_eq!(frame["outcome"].as_str(), Some("applied"), "{frame}");
                break;
            }
            other => panic!("unexpected frame {other:?}: {frame}"),
        }
    }
    assert_eq!(
        checkpoints,
        CHUNKS - 1,
        "the acknowledged chunk must not be copied again"
    );
    resumed.shutdown();
    assert_moved(&image);
}

/// `applied` 不是提交：宿主随后探测不到内容，只能按 unknown 收尾。
/// worker 在这里给出 applied，并对区间之外的探测回一个没有 `content` 的失败
#[test]
fn applied_requires_probe() {
    let image = fixture("probe");
    let mut worker = start_ready(&image, &[]);
    // 非重叠小搬移，一个块即完成
    worker.send(&json!({
        "v": VERSION,
        "type": "step",
        "id": 1,
        "step": {
            "kind": "relocate",
            "part": 1,
            "src_first": 1024,
            "dst_first": 12000,
            "sectors": 8,
            "chunk_bytes": 4096,
            "chunk": 4096,
            "chunks": 1,
        },
        "state": zero_state(),
    }));
    loop {
        let frame = worker.frame();
        match frame["type"].as_str() {
            Some("note") => continue,
            Some("checkpoint") => {
                let cursor = frame["state"]["cursor"].as_u64().expect("cursor");
                worker.send(&ack(1, cursor));
            }
            Some("result") => {
                assert_eq!(frame["outcome"].as_str(), Some("applied"), "{frame}");
                break;
            }
            other => panic!("unexpected frame {other:?}: {frame}"),
        }
    }

    // 区间之外：读失败，worker 给不出内容——宿主据此把 applied 降为 unknown
    worker.send(&json!({
        "v": VERSION,
        "type": "probe",
        "id": 2,
        "part": 1,
        "first_lba": DISK_SECTORS + 8,
        "sectors": 8,
    }));
    let probe = worker.frame();
    assert_eq!(probe["type"].as_str(), Some("probe_result"), "{probe}");
    assert_ne!(probe["outcome"].as_str(), Some("applied"), "{probe}");
    assert!(probe.get("content").is_none(), "a failed probe must not report content: {probe}");
    worker.shutdown();
}

/// worker 不写记录：宿主发来协议之外的帧时拒绝会话，目标与状态目录都不变
#[test]
fn worker_writes_no_record() {
    let image = fixture("record");
    let before = std::fs::read(&image).unwrap();
    let state_dir = image.parent().unwrap().join("state");
    std::fs::create_dir_all(&state_dir).unwrap();

    let mut worker = start_ready(&image, &[("DISKEDIT_STATE_DIR", &state_dir)]);
    worker.send(&json!({ "v": VERSION, "type": "commit", "id": 1 }));
    let error = worker.frame();
    assert_eq!(error["type"].as_str(), Some("error"), "{error}");
    let status = worker.child.wait().unwrap();
    assert_eq!(status.code(), Some(5), "a frame outside the protocol is a protocol error");

    assert_eq!(std::fs::read(&image).unwrap(), before, "the rejected session wrote the target");
    assert!(
        std::fs::read_dir(&state_dir).unwrap().next().is_none(),
        "the worker created a record directory"
    );
}

/// 噪声行被转交 stderr 而不打断会话；坏帧是协议错误，会话随即结束
#[test]
fn noise_and_truncated_frame() {
    let image = fixture("noise");
    let mut worker = Worker::start(TOKEN, &[]);
    writeln!(worker.stdin, "kernel: booting the carrier").unwrap();
    worker.stdin.flush().unwrap();
    let size = std::fs::metadata(&image).unwrap().len();
    worker.send(&json!({
        "v": VERSION,
        "type": "hello",
        "token": TOKEN,
        "binary_version": env!("CARGO_PKG_VERSION"),
        "target": image.to_string_lossy(),
        "sector_size": SECTOR,
        "disk_size": size,
        "job": "noise",
    }));
    let ready = worker.frame();
    assert_eq!(ready["type"].as_str(), Some("ready"));
    worker.adopt_session(&ready);
    worker.shutdown();
    let mut truncated = start_ready(&image, &[]);
    writeln!(truncated.stdin, "{{\"v\":1,").unwrap();
    truncated.stdin.flush().unwrap();
    let error = truncated.frame();
    assert_eq!(error["type"].as_str(), Some("error"), "{error}");
    let status = truncated.child.wait().unwrap();
    assert_eq!(status.code(), Some(5), "a truncated frame is a protocol error");
}

/// 会话边界：握手之后不带会话号的帧不是本会话的，worker 按协议错误结束，
/// 且它不会被执行——拿到 stdin 写权限的一方拿不到会话号（那只会经 stdout 交给宿主）
#[test]
fn a_frame_without_the_session_id_is_refused() {
    let image = fixture("stranger");
    let before = std::fs::read(&image).unwrap();
    let mut worker = start_ready(&image, &[]);
    worker.send_as_stranger(&json!({"v": VERSION, "type": "shutdown"}));
    let error = worker.frame();
    assert_eq!(error["type"].as_str(), Some("error"), "{error}");
    assert!(
        error["message"].as_str().unwrap_or_default().contains("session"),
        "the refusal must name the reason: {error}"
    );
    let status = worker.child.wait().unwrap();
    assert_eq!(status.code(), Some(5), "a frame from outside the session is a protocol error");
    assert_eq!(std::fs::read(&image).unwrap(), before, "the rejected session touched the target");
}
