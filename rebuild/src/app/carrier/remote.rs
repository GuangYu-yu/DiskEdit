//! 宿主侧的载体会话：握手、逐步下发、收结果。
//!
//! 记录、锁与退出码都留在宿主；会话只负责把步骤送达来宾并把结果带回来。读端
//! 在独立线程里转成事件队列，因此等待有超时，收尾时也不会卡在读上。

use std::path::Path;
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::thread;
use std::time::Duration;

use diskedit_core::region::Region;
use serde_json::Value;

use super::manifest::Bundle;
use super::protocol::{self, Lines, StepResult};
use crate::app::decide::step::StepKind;
use crate::platform::{CarrierLaunch, CarrierProcess, Platform};

/// 启动参数。
pub struct Launch<'a> {
    pub target: &'a Path,
    pub sector_size: u64,
    pub disk_size: u64,
    pub plan_digest: &'a [u8; 32],
    pub job: &'a str,
}

/// 握手超时。
const HANDSHAKE: Duration = Duration::from_secs(30);
/// 普通步骤超时。
const STEP: Duration = Duration::from_secs(60);
/// 文件系统步骤超时；外部工具可能长时间运行。
const FILESYSTEM_STEP: Duration = Duration::from_secs(600);
/// 收尾超时。
const FAREWELL: Duration = Duration::from_secs(5);

enum Event {
    Frame(Value),
    Closed(String),
}

/// 一台已握手的载体。
pub struct Session {
    writer: Box<dyn std::io::Write + Send>,
    events: Receiver<Event>,
    process: Box<dyn CarrierProcess>,
    sid: String,
    next: u32,
    closed: bool,
}

impl Session {
    /// 启动载体并完成握手。
    pub fn open(
        platform: &dyn Platform,
        bundle: &Bundle,
        launch: Launch<'_>,
    ) -> Result<Self, String> {
        let guest = platform
            .carrier_target()
            .ok_or_else(|| "本宿主没有可用的来宾设备".to_owned())?;
        let token = super::token();
        let request = CarrierLaunch {
            target: launch.target.to_path_buf(),
            sector_size: launch.sector_size,
            disk_size: launch.disk_size,
            kernel: bundle.kernel.clone(),
            initramfs: bundle.initramfs.clone(),
            arch: std::env::consts::ARCH.to_owned(),
            token: token.clone(),
        };
        let mut process = platform
            .launch_carrier(&request)
            .map_err(|error| format!("启动虚拟机载体失败：{error}"))?;
        let (mut writer, reader) = process
            .take_io()
            .ok_or_else(|| "虚拟机载体没有可用的通信通道".to_owned())?;
        let events = reader_events(reader);
        wait_for(&events, "boot", HANDSHAKE, "等待来宾启动")?;
        protocol::emit(
            &mut writer,
            &protocol::hello(
                &token,
                env!("CARGO_PKG_VERSION"),
                guest,
                launch.sector_size,
                launch.disk_size,
                launch.plan_digest,
                launch.job,
            ),
        )
        .map_err(|error| format!("发送握手失败：{error}"))?;
        let ready = wait_for(&events, "ready", HANDSHAKE, "等待来宾就绪")?;
        let version = field(&ready, "binary_version")?;
        if version != env!("CARGO_PKG_VERSION") {
            process.terminate();
            return Err(format!(
                "来宾版本 {version} 与宿主 {} 不一致",
                env!("CARGO_PKG_VERSION")
            ));
        }
        if ready.get("disk_size").and_then(Value::as_u64) != Some(launch.disk_size) {
            process.terminate();
            return Err("来宾看到的容量与宿主不一致".to_owned());
        }
        if ready.get("sector_size").and_then(Value::as_u64) != Some(launch.sector_size) {
            process.terminate();
            return Err("来宾看到的扇区尺寸与宿主不一致".to_owned());
        }
        let sid = field(&ready, "sid")?.to_owned();
        Ok(Self {
            writer,
            events,
            process,
            sid,
            next: 0,
            closed: false,
        })
    }

    /// 下发一步并等待结果。
    pub fn run_step(&mut self, kind: &StepKind, regions: &[Region]) -> Result<StepResult, String> {
        let id = self.next;
        self.next = self.next.wrapping_add(1);
        let frame = protocol::step(id, &self.sid, kind, regions)?;
        protocol::emit(&mut self.writer, &frame)
            .map_err(|error| format!("发送步骤 {id} 失败：{error}"))?;
        let timeout = if needs_filesystem_tools(kind) {
            FILESYSTEM_STEP
        } else {
            STEP
        };
        let reply = self.wait_result(id, timeout)?;
        Ok(reply)
    }

    fn wait_result(&self, id: u32, timeout: Duration) -> Result<StepResult, String> {
        let frame = match self.events.recv_timeout(timeout) {
            Ok(Event::Frame(frame)) => frame,
            Ok(Event::Closed(why)) => return Err(why),
            Err(RecvTimeoutError::Timeout) => {
                return Err(format!("步骤 {id} 等待结果超时"));
            }
            Err(RecvTimeoutError::Disconnected) => {
                return Err("虚拟机载体事件通道已断开".to_owned());
            }
        };
        if frame.get("v").and_then(Value::as_u64) != Some(protocol::VERSION) {
            return Err("来宾使用了不同的协议版本".to_owned());
        }
        let kind = frame.get("type").and_then(Value::as_str).unwrap_or("");
        if kind != "result" {
            return Err(format!("等待结果时收到 {kind} 帧"));
        }
        let (reply, result) = protocol::decode_result(&frame)?;
        if reply != id {
            return Err(format!("结果帧的步骤号 {reply} 与 {id} 不符"));
        }
        if frame.get("sid").and_then(Value::as_str) != Some(&self.sid) {
            return Err("结果帧的会话号不匹配".to_owned());
        }
        Ok(result)
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        if !self.closed {
            self.closed = true;
            drop(protocol::emit(
                &mut self.writer,
                &protocol::shutdown(&self.sid),
            ));
            drop(self.events.recv_timeout(FAREWELL));
        }
        self.process.terminate();
    }
}

fn reader_events(reader: Box<dyn std::io::Read + Send>) -> Receiver<Event> {
    let (sender, receiver) = channel();
    thread::spawn(move || {
        let mut lines = Lines::new(reader);
        loop {
            match lines.next_frame() {
                Ok(Some(frame)) => {
                    if sender.send(Event::Frame(frame)).is_err() {
                        return;
                    }
                }
                Ok(None) => {
                    drop(sender.send(Event::Closed("虚拟机载体已关闭输出".to_owned())));
                    return;
                }
                Err(error) => {
                    drop(sender.send(Event::Closed(format!("读取载体输出失败：{error}"))));
                    return;
                }
            }
        }
    });
    receiver
}

fn wait_for(
    events: &Receiver<Event>,
    wanted: &str,
    timeout: Duration,
    what: &str,
) -> Result<Value, String> {
    loop {
        let frame = match events.recv_timeout(timeout) {
            Ok(Event::Frame(frame)) => frame,
            Ok(Event::Closed(why)) => return Err(why),
            Err(RecvTimeoutError::Timeout) => return Err(format!("{what}超时")),
            Err(RecvTimeoutError::Disconnected) => {
                return Err("虚拟机载体事件通道已断开".to_owned());
            }
        };
        if frame.get("v").and_then(Value::as_u64) != Some(protocol::VERSION) {
            return Err("来宾使用了不同的协议版本".to_owned());
        }
        if frame.get("type").and_then(Value::as_str) == Some(wanted) {
            return Ok(frame);
        }
    }
}

fn field<'a>(frame: &'a Value, name: &str) -> Result<&'a str, String> {
    frame
        .get(name)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("来宾回复缺少 {name}"))
}

fn needs_filesystem_tools(kind: &StepKind) -> bool {
    matches!(
        kind,
        StepKind::MakeFilesystem { .. }
            | StepKind::GrowFilesystem { .. }
            | StepKind::ShrinkFilesystem { .. }
            | StepKind::SetProperty {
                property: crate::app::observe::SetProperty::Label
                    | crate::app::observe::SetProperty::Uuid,
                ..
            }
    )
}
