//! 宿主侧载体会话：拉起 guest、握手，然后逐步骤下发并接收检查点与结果。
//!
//! 宿主是唯一的记录写者：guest 的检查点先落进 sink，宿主回 ack 之后 guest 才继续。
//! 步骤结果经探测复核；探测缺席、探测不成立或会话超时都按 [`StepOutcome::Unknown`]
//! 交回，记录保留，重跑从同一步继续。

use std::io::BufReader;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread;
use std::time::Duration;

use serde_json::{Value, json};

use crate::dev::WriteSource;
use crate::execution::{StepSink, TransferState};
use crate::outcome::RunOutcome;

use super::protocol::{self, Decoded, Lines, Read, WireStep};
use super::{ProbeReport, ProbeRequest, StepOutcome, StepRun};

/// 普通帧的等待上限：检查点与结果之间 guest 只做内存操作
const STEP_TIMEOUT: Duration = Duration::from_secs(60);
/// 文件系统工具运行时的上限：宿主收到 note/checkpoint 会重置计时
const FS_TIMEOUT: Duration = Duration::from_secs(600);
const READY_TIMEOUT: Duration = Duration::from_secs(30);
/// 会话建立或传输失败。宿主据此区分"还没写"与"可能已写"
enum SessionError {
    Io(String),
    Timeout,
    Closed,
    Protocol(String),
}

impl SessionError {
    fn text(&self) -> String {
        match self {
            Self::Io(text) | Self::Protocol(text) => text.clone(),
            Self::Timeout => "the carrier session stopped responding".to_string(),
            Self::Closed => "the carrier session closed its channel".to_string(),
        }
    }
}

/// 一次作业的载体会话
pub(crate) struct Session {
    child: Child,
    stdin: ChildStdin,
    lines: Receiver<std::io::Result<Read>>,
    next_id: u64,
    /// 载体在应答里签发的会话号；置位后每一帧都回带它（见 [`Session::send`]）
    sid: Option<String>,
}

impl Session {
    /// 与 VM 载体建立会话：目标句柄按 fd 交给 QEMU，协议帧走它的串口
    #[cfg(target_os = "linux")]
    pub(crate) fn open_vm(
        bundle: &super::qemu::Bundle,
        src: &WriteSource,
        sector_size: u64,
        job: &str,
    ) -> Result<Self, RunOutcome> {
        let token = token()?;
        let command = super::qemu::command(bundle, src, &token)?;
        Self::attach(command, token, super::qemu::TARGET.to_string(), src, sector_size, job)
    }

    /// 启动载体并完成握手。`target` 是 guest 里看到的设备路径
    fn attach(
        mut command: Command,
        token: String,
        target: String,
        src: &WriteSource,
        sector_size: u64,
        job: &str,
    ) -> Result<Self, RunOutcome> {
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|e| RunOutcome::unsupported(format!("cannot start the carrier: {e}")))?;
        let stdin = child.stdin.take().expect("the carrier was spawned with a piped stdin");
        let stdout = child.stdout.take().expect("the carrier was spawned with a piped stdout");
        let (sender, lines) = mpsc::channel();
        thread::spawn(move || {
            let mut lines = Lines::new(BufReader::new(stdout));
            loop {
                let read = lines.read();
                let done = matches!(read, Ok(Read::Closed) | Ok(Read::Oversize) | Err(_));
                if sender.send(read).is_err() || done {
                    return;
                }
            }
        });
        let mut session = Self { child, stdin, lines, next_id: 1, sid: None };
        let hello = json!({
            "v": protocol::VERSION,
            "type": "hello",
            "token": token,
            "binary_version": protocol::BINARY_VERSION,
            "target": target,
            "sector_size": sector_size,
            "disk_size": src.size,
            "job": job,
        });
        session.send(&hello).map_err(|e| {
            session.kill();
            RunOutcome::unsupported(format!("the carrier handshake failed: {}", e.text()))
        })?;
        let ready = session.wait_frame(READY_TIMEOUT).map_err(|e| {
            session.kill();
            RunOutcome::unsupported(format!("the carrier handshake failed: {}", e.text()))
        })?;
        if ready.get("type").and_then(Value::as_str) != Some("ready") {
            session.kill();
            return Err(RunOutcome::unsupported(
                "the carrier answered the handshake with an unexpected frame",
            ));
        }
        if ready.get("binary_version").and_then(Value::as_str) != Some(protocol::BINARY_VERSION)
            || ready.get("disk_size").and_then(Value::as_u64) != Some(src.size)
            || ready.get("sector_size").and_then(Value::as_u64) != Some(sector_size)
        {
            session.kill();
            return Err(RunOutcome::refused(
                "the carrier sees a different target geometry than the host plan",
            ));
        }
        // 会话号由载体签发：随后的每一帧都回带它，只验握手不验后续帧等于没有会话边界
        let Some(sid) = ready.get("sid").and_then(Value::as_str) else {
            session.kill();
            return Err(RunOutcome::refused(
                "the carrier did not name its session; every later frame would be indistinguishable \
                 from a stranger writing on the same pipe",
            ));
        };
        session.sid = Some(sid.to_string());
        Ok(session)
    }

    /// 下发一个步骤并把它的事件交给宿主 sink。任何传输失败都按 `Unknown` 交回
    pub(crate) fn run_step(
        &mut self,
        src: &mut WriteSource,
        step: &WireStep,
        state: &TransferState,
        sink: &mut dyn StepSink,
    ) -> Result<StepRun, RunOutcome> {
        let id = self.next_id;
        self.next_id += 1;
        let (entry, chunks_in_entry, completed_steps, cursor) = state.durable();
        let request = json!({
            "v": protocol::VERSION,
            "type": "step",
            "id": id,
            "step": protocol::encode_step(step),
            "state": {
                "entry": entry,
                "chunks_in_entry": chunks_in_entry,
                "completed_steps": completed_steps,
                "cursor": cursor,
                "digest": protocol::hex_digest(&state.prefix_digest()),
            },
        });
        if let Err(e) = self.send(&request) {
            return Ok(StepRun::unknown(e.text()));
        }
        let mut run = match self.collect(src, id, sink) {
            Ok(Decoded::Run(run)) => run,
            // 载体判定过的失败按它的类别重建，与宿主进程内的失败同一条通道
            Ok(Decoded::Failed(error)) => return Err(error),
            // 传输层失败：写没写不知道，记录保留，重跑从同一步继续
            Err(e) => return Ok(StepRun::unknown(e.text())),
        };
        if run.outcome == StepOutcome::Applied
            && let Some(request) = probe_of(step)
            && let Err(e) = self.probe(request, &mut run, step)
        {
            // 探测没能证实：结论退成"不知道"，宿主已收到的事件不足以断言落点
            run.probe_unconfirmed(format!("the carrier probe did not confirm the step: {}", e.text()));
            sink.note(&format!("the carrier probe did not confirm the step: {}", e.text()));
        }
        Ok(run)
    }

    /// 读事件直到本步骤的结果；检查点落地之后才回 ack
    fn collect(
        &mut self,
        src: &mut WriteSource,
        id: u64,
        sink: &mut dyn StepSink,
    ) -> Result<Decoded, SessionError> {
        loop {
            let frame = self.wait_frame(STEP_TIMEOUT)?;
            match frame.get("type").and_then(Value::as_str) {
                Some("checkpoint") => {
                    let state = state_of(&frame)?;
                    sink.transfer_checkpoint(src, &state)
                        .map_err(|e| SessionError::Io(protocol::failure_text(&e)))?;
                    self.ack(id, state.durable().3)?;
                }
                Some("barrier") => {
                    match frame.get("kind").and_then(Value::as_str) {
                        Some("entry_table") => sink
                            .entry_table_started(src, state_of(&frame)?.durable().0)
                            .map_err(|e| SessionError::Io(protocol::failure_text(&e)))?,
                        Some("table") => sink
                            .table_step_started(src)
                            .map_err(|e| SessionError::Io(protocol::failure_text(&e)))?,
                        _ => return Err(SessionError::Protocol("an unknown barrier".into())),
                    }
                    self.ack(id, state_of(&frame)?.durable().3)?;
                }
                Some("progress") => {
                    // 步骤序号必填：缺省成 0 会把别的步骤的完成记到第 0 步上
                    let index = frame
                        .get("index")
                        .and_then(Value::as_u64)
                        .and_then(|index| usize::try_from(index).ok())
                        .ok_or_else(|| {
                            SessionError::Protocol("a progress frame has no step index".into())
                        })?;
                    let state = state_of(&frame)?;
                    match frame.get("kind").and_then(Value::as_str) {
                        Some("table_done") => sink
                            .table_step_done(src, index, &state)
                            .map_err(|e| SessionError::Io(protocol::failure_text(&e)))?,
                        Some("fs_done") => sink
                            .fs_step_done(src, index, &state)
                            .map_err(|e| SessionError::Io(protocol::failure_text(&e)))?,
                        _ => {
                            return Err(SessionError::Protocol(
                                "a progress frame names no known step outcome".into(),
                            ));
                        }
                    }
                }
                Some("note") => {
                    if let Some(text) = frame.get("text").and_then(Value::as_str) {
                        sink.note(text);
                    }
                }
                Some("result") => {
                    return protocol::decode_result(&frame)
                        .ok_or_else(|| SessionError::Protocol("a malformed result frame".into()));
                }
                Some("error") => {
                    let text = frame
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("the worker reported an error");
                    return Ok(Decoded::Run(StepRun::unknown(text.to_string())));
                }
                _ => {
                    return Err(SessionError::Protocol(
                        "the worker sent a frame outside the step protocol".into(),
                    ));
                }
            }
        }
    }

    /// 结果之后的探测：宿主确认落点上读得到预期内容
    fn probe(
        &mut self,
        request: ProbeRequest,
        run: &mut StepRun,
        step: &WireStep,
    ) -> Result<(), SessionError> {
        let id = self.next_id;
        self.next_id += 1;
        self.send(&json!({
            "v": protocol::VERSION,
            "type": "probe",
            "id": id,
            "part": request.part,
            "first_lba": request.first_lba,
            "sectors": request.sectors,
        }))?;
        let frame = self.wait_frame(FS_TIMEOUT)?;
        if frame.get("type").and_then(Value::as_str) != Some("probe_result")
            || frame.get("id").and_then(Value::as_u64) != Some(id)
        {
            return Err(SessionError::Protocol("the worker did not answer the probe".into()));
        }
        let report = ProbeReport {
            // 分区号是这次探测结果的一部分：缺了它就无法把内容归到某个分区，按协议错误
            // 处理，而不是默认成整盘（0）
            part: u32::try_from(
                frame
                    .get("part")
                    .and_then(Value::as_u64)
                    .ok_or_else(|| {
                        SessionError::Protocol(
                            "the probe result does not name its partition".into(),
                        )
                    })?,
            )
            .map_err(|_| {
                SessionError::Protocol("the probe reported a partition number out of range".into())
            })?,
            content: frame
                .get("content")
                .and_then(Value::as_str)
                .and_then(crate::domain::ContentKind::from_name)
                .ok_or_else(|| SessionError::Protocol("the probe reported no content".into()))?,
        };
        run.probe = Some(report);
        // 文件系统步骤要求落点能识别出内容；纯数据搬移只确认可读
        let needs_content = matches!(step, WireStep::FsResize(_) | WireStep::FsCreate(_));
        if needs_content && report.content == crate::domain::ContentKind::Unknown {
            return Err(SessionError::Protocol(
                "the partition reads back as unknown content".into(),
            ));
        }
        Ok(())
    }

    fn ack(&mut self, id: u64, cursor: u64) -> Result<(), SessionError> {
        self.send(&json!({
            "v": protocol::VERSION,
            "type": "ack",
            "id": id,
            "cursor": cursor,
        }))
    }

    /// 唯一的发帧入口：握手之后每一帧都补上会话号。放在这里而不是各个调用点，
    /// 是为了让"新加了一种帧"不可能忘记回带
    fn send(&mut self, value: &Value) -> Result<(), SessionError> {
        let mut value = value.clone();
        if let Some(sid) = &self.sid
            && value.get("type").and_then(Value::as_str) != Some("hello")
        {
            value["sid"] = json!(sid);
        }
        protocol::emit(&mut self.stdin, &value).map_err(|e| SessionError::Io(e.to_string()))
    }

    /// 读下一帧。非 `{` 开头的行转交 stderr 之后继续等
    fn wait_frame(&mut self, timeout: Duration) -> Result<Value, SessionError> {
        loop {
            match self.lines.recv_timeout(timeout) {
                Ok(Ok(Read::Frame(value))) => return Ok(value),
                Ok(Ok(Read::Noise(text))) => eprintln!("{text}"),
                Ok(Ok(Read::Oversize)) => {
                    self.kill();
                    return Err(SessionError::Protocol("the worker sent an oversized frame".into()));
                }
                Ok(Ok(Read::Closed)) | Ok(Err(_)) => {
                    self.kill();
                    return Err(SessionError::Closed);
                }
                Err(RecvTimeoutError::Timeout) => {
                    self.kill();
                    return Err(SessionError::Timeout);
                }
                Err(RecvTimeoutError::Disconnected) => return Err(SessionError::Closed),
            }
        }
    }

    fn kill(&mut self) {
        best_effort_reap_child(&mut self.child);
    }
}

/// 终止并回收 guest 进程：主错误已经返回，这里的失败只影响资源占用
fn best_effort_reap_child(child: &mut std::process::Child) {
    // guest 可能自己先退出，所以两次回收都允许失败：主错误已经返回，这里失败只影响
    // 资源占用，不改变已经得出的结论。错误在这里被丢弃是结论，不是疏忽
    child.kill().ok();
    child.wait().ok();
}

impl Drop for Session {
    fn drop(&mut self) {
        // 关闭要尽量走完：宿主已经落盘的部分由记录承载，guest 只需要退出
        if let Err(error) = protocol::emit(
            &mut self.stdin,
            &json!({ "v": protocol::VERSION, "type": "shutdown" }),
        ) {
            eprintln!("diskedit: the worker session could not be closed cleanly: {error}");
        }
        self.kill();
    }
}

/// 步骤自身的落点：结果之后由宿主发探测
fn probe_of(step: &WireStep) -> Option<ProbeRequest> {
    match step {
        WireStep::Table(_) | WireStep::FsSet(_) => None,
        WireStep::Relocate(step) => Some(ProbeRequest {
            part: step.part,
            first_lba: step.dst_first,
            sectors: step.sectors,
        }),
        WireStep::Copy { step, .. } => Some(ProbeRequest {
            part: step.part,
            first_lba: step.dst_first,
            sectors: step.sectors,
        }),
        WireStep::FsResize(step) => Some(ProbeRequest {
            part: step.part,
            first_lba: step.first_lba,
            sectors: step.sectors,
        }),
        WireStep::FsCreate(step) => Some(ProbeRequest {
            part: step.part,
            first_lba: step.first_lba,
            sectors: step.sectors,
        }),
        WireStep::RebuildSwap(step) => Some(ProbeRequest {
            part: step.part,
            first_lba: step.dst_first,
            sectors: step.sectors,
        }),
        WireStep::HiddenSectors(step) => Some(ProbeRequest {
            part: step.part,
            first_lba: step.dst_first,
            sectors: step.sectors,
        }),
    }
}

/// 事件帧里的进度状态。**每个字段都必填**：缺字段按 0 补会把 guest 的进度读成
/// "第 0 条、游标 0"，而宿主正是按它落检查点
fn state_of(frame: &Value) -> Result<TransferState, SessionError> {
    let state = frame
        .get("state")
        .ok_or_else(|| SessionError::Protocol("a progress frame has no state".into()))?;
    let entry = u32_of(state, "entry")
        .ok_or_else(|| SessionError::Protocol("the progress state has no entry".into()))?;
    let chunks = state
        .get("chunks_in_entry")
        .and_then(Value::as_u64)
        .ok_or_else(|| SessionError::Protocol("the progress state has no chunk count".into()))?;
    let steps = u32_of(state, "completed_steps")
        .ok_or_else(|| SessionError::Protocol("the progress state has no step count".into()))?;
    let cursor = state
        .get("cursor")
        .and_then(Value::as_u64)
        .ok_or_else(|| SessionError::Protocol("the progress state has no cursor".into()))?;
    let digest = state
        .get("digest")
        .and_then(Value::as_str)
        .and_then(protocol::digest_from_hex)
        .ok_or_else(|| SessionError::Protocol("the progress state has no digest".into()))?;
    Ok(TransferState::restore(entry, chunks, steps, cursor, digest))
}

fn u32_of(state: &Value, name: &str) -> Option<u32> {
    u32::try_from(state.get(name)?.as_u64()?).ok()
}

/// 会话令牌：每次会话新生成，握手两端比对
fn token() -> Result<String, RunOutcome> {
    protocol::random_token()
        .ok_or_else(|| RunOutcome::unsupported("no random source is available for the session token"))
}

impl From<SessionError> for RunOutcome {
    fn from(error: SessionError) -> Self {
        RunOutcome::partial(format!("the carrier session failed: {}", error.text()))
    }
}
