//! guest 侧 worker：只按会话步骤写目标，不解析用户命令、不写记录、不碰宿主的锁。
//!
//! 入口是 `__worker` 子命令，在命令解析之前拦截；令牌从环境变量取，stdin 必须不是
//! 终端。握手复核目标的几何之后才开始接收步骤。

use std::io::{self, BufReader, IsTerminal, Stdin, Stdout};
use std::path::Path;

use serde_json::{Value, json};

use crate::dev::{FileSource, WriteSource};
use crate::execution::{StepSink, TransferState};
use crate::outcome::RunOutcome;

use super::native;
use super::protocol::{self, Lines, Read};
use super::ProbeRequest;

const TOKEN_VAR: &str = "DISKEDIT_WORKER_TOKEN";
/// 载体 init 拉起 worker 时置位。串口控制台也是终端，不置位就会被下面的守卫拒掉
const FROM_INIT_VAR: &str = "DISKEDIT_WORKER_FROM_INIT";

/// 本进程是否是 worker 调用
pub(crate) fn is_invoked() -> bool {
    std::env::args_os().nth(1).is_some_and(|arg| arg.to_str() == Some("__worker"))
}

/// worker 主循环。退出码只表达会话本身：0 正常收尾，3 握手或几何不符，5 协议错误
pub(crate) fn run() -> u8 {
    let token = match std::env::var(TOKEN_VAR) {
        Ok(token) if !token.is_empty() => token,
        _ => {
            eprintln!("diskedit worker: {TOKEN_VAR} is not set");
            return 3;
        }
    };
    // 载体 init 拉起时 stdin 就是串口控制台（isatty 为真），这不是有人在宿主上误敲了
    // `__worker`：init 显式置位放行。宿主上直接运行仍按终端拒绝
    let from_init = std::env::var(FROM_INIT_VAR).is_ok_and(|v| v == "1");
    if !from_init && io::stdin().is_terminal() {
        eprintln!("diskedit worker: refusing to read a one-time token from a terminal");
        return 3;
    }
    let mut io = WorkerIo { input: Lines::new(BufReader::new(io::stdin())), output: io::stdout() };
    match serve(&mut io, &token) {
        Ok(()) => 0,
        Err(Failure::Handshake(text)) => {
            io.fail(&text);
            3
        }
        Err(Failure::Protocol(text)) => {
            io.fail(&text);
            5
        }
    }
}

enum Failure {
    Handshake(String),
    Protocol(String),
}

impl From<io::Error> for Failure {
    fn from(error: io::Error) -> Self {
        Self::Protocol(error.to_string())
    }
}

fn serve(io: &mut WorkerIo, token: &str) -> Result<(), Failure> {
    let hello = io.frame()?;
    if hello.get("type").and_then(Value::as_str) != Some("hello") {
        return Err(Failure::Handshake("the first frame is not a handshake".into()));
    }
    if hello.get("token").and_then(Value::as_str) != Some(token) {
        return Err(Failure::Handshake("the session token does not match".into()));
    }
    if hello.get("binary_version").and_then(Value::as_str) != Some(protocol::BINARY_VERSION) {
        return Err(Failure::Handshake(
            "the host and the worker run different builds".into(),
        ));
    }
    let target = hello
        .get("target")
        .and_then(Value::as_str)
        .ok_or_else(|| Failure::Handshake("the handshake names no target".into()))?;
    let sector_size = hello
        .get("sector_size")
        .and_then(Value::as_u64)
        .ok_or_else(|| Failure::Handshake("the handshake has no sector size".into()))?;
    let disk_size = hello
        .get("disk_size")
        .and_then(Value::as_u64)
        .ok_or_else(|| Failure::Handshake("the handshake has no disk size".into()))?;
    let source = FileSource::open_in_carrier(Path::new(target), Some(sector_size))
        .map_err(|e| Failure::Handshake(format!("cannot open {target}: {e}")))?;
    reconcile_geometry(target, source.size, disk_size, source.sector_size, sector_size)?;
    let measured_ss = source.sector_size;
    let mut src = WriteSource::borrowed(source);
    // 会话号：只在这次应答里交给宿主，随后每一帧都必须回带。只验一次握手等于把"拿到
    // stdin 写权限的第三方"当成宿主——它们不必知道 token 就能接在后面发帧
    let sid = session_id();
    io.emit(&json!({
        "v": protocol::VERSION,
        "type": "ready",
        "binary_version": protocol::BINARY_VERSION,
        "sector_size": measured_ss,
        "disk_size": disk_size,
        "sid": sid.clone(),
    }))?;

    loop {
        let frame = io.frame()?;
        if frame.get("sid").and_then(Value::as_str) != Some(sid.as_str()) {
            return Err(Failure::Protocol(
                "the frame does not carry this session's id".into(),
            ));
        }
        match frame.get("type").and_then(Value::as_str) {
            Some("step") => {
                let id = id_of(&frame)?;
                let Some(step) = frame.get("step").and_then(|value| protocol::decode_step(value, sector_size)) else {
                    return Err(Failure::Protocol("the host sent a step this build cannot decode".into()));
                };
                let mut state = state_of(&frame)?;
                let mut sink = WorkerSink { io, id };
                let run = native::execute(&mut src, step.carrier(), sector_size, &mut state, &mut sink);
                match &run {
                    Ok(run) => {
                        let mut value = protocol::encode_result(run);
                        value["id"] = json!(id);
                        io.emit(&value)?;
                    }
                    Err(error) => {
                        let mut value = protocol::encode_failure(error);
                        value["id"] = json!(id);
                        io.emit(&value)?;
                    }
                }
            }
            Some("probe") => {
                let id = id_of(&frame)?;
                let request = ProbeRequest {
                    part: u32_of(&frame, "part")?,
                    first_lba: u64_of(&frame, "first_lba")?,
                    sectors: u64_of(&frame, "sectors")?,
                };
                match native::execute_probe(&src, request, sector_size) {
                    Ok(report) => {
                        io.emit(&json!({
                            "v": protocol::VERSION,
                            "type": "probe_result",
                            "id": id,
                            "part": report.part,
                            "content": report.content.as_str(),
                        }))?;
                    }
                    Err(error) => {
                        let mut value = protocol::encode_failure(&error);
                        value["type"] = json!("probe_result");
                        value["id"] = json!(id);
                        io.emit(&value)?;
                    }
                }
            }
            Some("shutdown") => {
                io.emit(&json!({ "v": protocol::VERSION, "type": "bye" }))?;
                return Ok(());
            }
            _ => {
                return Err(Failure::Protocol(
                    "the host sent a frame outside the worker protocol".into(),
                ));
            }
        }
    }
}

/// 握手几何对账：实测容量与实测扇区大小都必须与计划声明一致。
///
/// 扇区大小一维不可省：块设备的逻辑扇区由内核经 `BLKSSZGET` 实测给出，握手声明的 ss 只是
/// 宿主计划值。若只把握手值原样回显，宿主那道几何比对就是拿声明核对声明的自证循环，guest
/// 报告了不同逻辑块大小时抓不到。任一一维不符都按握手失败拒绝——会话尚未开始，不存在
/// "部分改动"。
fn reconcile_geometry(
    target: &str,
    measured_size: u64,
    declared_size: u64,
    measured_ss: u64,
    declared_ss: u64,
) -> Result<(), Failure> {
    if measured_size != declared_size {
        return Err(Failure::Handshake(format!(
            "{target} is {measured_size} bytes but the plan was made for {declared_size}"
        )));
    }
    if measured_ss != declared_ss {
        return Err(Failure::Handshake(format!(
            "{target} reports a sector size of {measured_ss} bytes but the plan was made for {declared_ss}"
        )));
    }
    Ok(())
}

/// 会话号：进程熵（`RandomState` 的密钥）上取 128 位，不额外依赖平台 API。
/// 它只经 stdout 交给宿主，因而不在"只拿到 stdin 写权限"的一方的可见范围内
fn session_id() -> String {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    let state = RandomState::new();
    let mut sid = String::with_capacity(32);
    for salt in 0u64..2 {
        let mut hasher = state.build_hasher();
        hasher.write(b"diskedit worker session");
        hasher.write(&salt.to_be_bytes());
        sid.push_str(&format!("{:016x}", hasher.finish()));
    }
    sid
}

fn id_of(frame: &Value) -> Result<u64, Failure> {
    frame
        .get("id")
        .and_then(Value::as_u64)
        .ok_or_else(|| Failure::Protocol("a step frame has no id".into()))
}

fn u64_of(frame: &Value, name: &str) -> Result<u64, Failure> {
    frame
        .get(name)
        .and_then(Value::as_u64)
        .ok_or_else(|| Failure::Protocol(format!("a probe frame carries no `{name}`")))
}

fn u32_of(frame: &Value, name: &str) -> Result<u32, Failure> {
    u32::try_from(u64_of(frame, name)?)
        .map_err(|_| Failure::Protocol(format!("`{name}` does not fit in 32 bits")))
}

/// 宿主随步骤下发的进度状态。**每个字段都必填**：缺字段时按 0 补会把"记录指向
/// 第 0 条/游标 0"当成真的进度，而它恰恰是重放与续跑的判定输入
fn state_of(frame: &Value) -> Result<TransferState, Failure> {
    let state = frame
        .get("state")
        .ok_or_else(|| Failure::Protocol("a step frame carries no progress state".into()))?;
    let entry = u32_of(state, "entry")?;
    let chunks = u64_of(state, "chunks_in_entry")?;
    let steps = u32_of(state, "completed_steps")?;
    let cursor = u64_of(state, "cursor")?;
    let digest = state
        .get("digest")
        .and_then(Value::as_str)
        .and_then(protocol::digest_from_hex)
        .ok_or_else(|| Failure::Protocol("a step frame carries no progress digest".into()))?;
    Ok(TransferState::restore(entry, chunks, steps, cursor, digest))
}

/// 步骤执行期间的事件出口。检查点与屏障都等宿主 ack 之后才放行
struct WorkerSink<'a> {
    io: &'a mut WorkerIo,
    id: u64,
}

impl StepSink for WorkerSink<'_> {
    fn transfer_checkpoint(
        &mut self,
        _src: &mut WriteSource,
        state: &TransferState,
    ) -> Result<(), RunOutcome> {
        self.event("checkpoint", None, 0, state, None)
            .map_err(|e| RunOutcome::partial(format!("the checkpoint did not reach the host: {e}")))
    }

    fn entry_table_started(
        &mut self,
        _src: &mut WriteSource,
        entry: u32,
    ) -> Result<(), RunOutcome> {
        self.event("barrier", Some("entry_table"), 0, &TransferState::default(), Some(entry))
            .map_err(|e| RunOutcome::partial(format!("the entry barrier did not reach the host: {e}")))
    }

    fn table_step_started(&mut self, _src: &mut WriteSource) -> Result<(), RunOutcome> {
        self.event("barrier", Some("table"), 0, &TransferState::default(), None)
            .map_err(|e| RunOutcome::partial(format!("the table barrier did not reach the host: {e}")))
    }

    fn table_step_done(
        &mut self,
        _src: &mut WriteSource,
        step: usize,
        state: &TransferState,
    ) -> Result<(), RunOutcome> {
        self.event("progress", Some("table_done"), step, state, None)
            .map_err(|e| RunOutcome::partial(format!("the table progress did not reach the host: {e}")))
    }

    fn fs_step_done(
        &mut self,
        _src: &mut WriteSource,
        step: usize,
        state: &TransferState,
    ) -> Result<(), RunOutcome> {
        self.event("progress", Some("fs_done"), step, state, None)
            .map_err(|e| RunOutcome::partial(format!("the filesystem progress did not reach the host: {e}")))
    }

    fn note(&mut self, msg: &str) {
        if let Err(error) =
            self.io.emit(&json!({ "v": protocol::VERSION, "type": "note", "text": msg }))
        {
            eprintln!("diskedit worker: {}", error);
        }
    }
}

impl WorkerSink<'_> {
    /// 发出事件并等 ack。检查点、屏障共用同一份进度状态
    fn event(
        &mut self,
        kind: &str,
        sub: Option<&str>,
        index: usize,
        state: &TransferState,
        entry: Option<u32>,
    ) -> io::Result<()> {
        let mut progress = state_json(state);
        if let Some(entry) = entry {
            progress["entry"] = json!(entry);
        }
        let mut frame = json!({
            "v": protocol::VERSION,
            "type": kind,
            "id": self.id,
            "index": index,
            "state": progress,
        });
        if let Some(sub) = sub {
            frame["kind"] = json!(sub);
        }
        self.io.emit(&frame)?;
        self.io.await_ack(self.id, state.durable().3)
    }
}

fn state_json(state: &TransferState) -> Value {
    let (entry, chunks, steps, cursor) = state.durable();
    json!({
        "entry": entry,
        "chunks_in_entry": chunks,
        "completed_steps": steps,
        "cursor": cursor,
        "digest": protocol::hex_digest(&state.prefix_digest()),
    })
}

struct WorkerIo {
    input: Lines<BufReader<Stdin>>,
    output: Stdout,
}

impl WorkerIo {
    fn frame(&mut self) -> Result<Value, Failure> {
        loop {
            match self.input.read()? {
                Read::Frame(value) => return Ok(value),
                Read::Noise(text) => eprintln!("{text}"),
                Read::Oversize => {
                    return Err(Failure::Protocol("a frame exceeds the protocol limit".into()));
                }
                Read::Closed => {
                    return Err(Failure::Protocol("the host closed the session".into()));
                }
            }
        }
    }

    fn emit(&mut self, value: &Value) -> io::Result<()> {
        protocol::emit(&mut self.output, value)
    }

    fn await_ack(&mut self, id: u64, cursor: u64) -> io::Result<()> {
        let frame = self.frame().map_err(|failure| match failure {
            Failure::Handshake(text) | Failure::Protocol(text) => {
                io::Error::new(io::ErrorKind::InvalidData, text)
            }
        })?;
        match frame.get("type").and_then(Value::as_str) {
            Some("ack")
                if frame.get("id").and_then(Value::as_u64) == Some(id)
                    && frame.get("cursor").and_then(Value::as_u64) == Some(cursor) =>
            {
                Ok(())
            }
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "the host acknowledged a different progress state",
            )),
        }
    }

    fn fail(&mut self, message: &str) {
        if let Err(error) =
            self.emit(&json!({ "v": protocol::VERSION, "type": "error", "message": message }))
        {
            eprintln!("diskedit worker: {error}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matching_geometry_passes_the_handshake() {
        assert!(reconcile_geometry("/dev/vda", 1 << 20, 1 << 20, 512, 512).is_ok());
    }

    #[test]
    fn mismatched_sector_size_is_refused_as_a_handshake_failure() {
        let error = reconcile_geometry("/dev/vda", 1 << 20, 1 << 20, 4096, 512)
            .expect_err("a reported sector size different from the plan must be refused");
        assert!(
            matches!(error, Failure::Handshake(_)),
            "a geometry mismatch on the trust boundary is a handshake failure, not a protocol error"
        );
    }

    #[test]
    fn mismatched_disk_size_is_refused_as_a_handshake_failure() {
        let error = reconcile_geometry("/dev/vda", 1 << 20, 2 << 20, 512, 512)
            .expect_err("a reported capacity different from the plan must be refused");
        assert!(matches!(error, Failure::Handshake(_)));
    }
}
