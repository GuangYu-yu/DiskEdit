//! 宿主侧载体会话：启动来宾、握手、下发一次在线扩容请求并核对结果。
//!
//! 会话只负责把请求送达来宾并把几何带回来；记录、锁与退出码留在宿主。

use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::platform::{OnlineReport, OnlineRequest, Platform, PlatformError, PlatformResult};

use super::manifest::Bundle;
use super::protocol::{self, Reply};
use super::qemu::Guest;

/// 宿主上的整盘：来宾将通过 virtio-blk 看到它的当前内容。
pub struct Disk {
    pub path: PathBuf,
    pub len: u64,
    pub sector: u64,
}

impl Disk {
    pub fn open(path: &Path) -> PlatformResult<Self> {
        let handle = crate::platform::host().open_target_read_only(path)?;
        Ok(Self {
            path: path.to_path_buf(),
            len: handle.len(),
            sector: handle.sector_size().unwrap_or(512),
        })
    }
}

/// 一台已握手的来宾。
pub struct Session {
    guest: Guest,
    sid: String,
}

impl Session {
    /// 启动来宾并完成握手；`request` 只用于声明目标几何。
    pub fn open(
        bundle: &Bundle,
        disk: &Disk,
        request: &OnlineRequest,
        token: String,
    ) -> PlatformResult<Self> {
        let sector = request.sector_size;
        let mut guest = Guest::start(
            &bundle.kernel,
            &bundle.initramfs,
            &disk.path,
            sector,
            &token,
        )?;
        expect(&mut guest, "boot")?;
        guest.write(&protocol::hello(
            &token,
            env!("CARGO_PKG_VERSION"),
            disk.len,
            sector,
        ))?;
        let ready = until(&mut guest, "ready")?;
        if ready.get("binary_version").and_then(Value::as_str) != Some(env!("CARGO_PKG_VERSION")) {
            guest.terminate();
            return Err(PlatformError::not_supported(
                "the guest worker version differs from the host",
            ));
        }
        if ready.get("disk_size").and_then(Value::as_u64) != Some(disk.len) {
            guest.terminate();
            return Err(PlatformError::parse(
                "the guest sees a different disk capacity than the host",
            ));
        }
        if ready.get("sector_size").and_then(Value::as_u64) != Some(sector) {
            guest.terminate();
            return Err(PlatformError::parse(
                "the guest sees a different sector size than the host",
            ));
        }
        let sid = ready
            .get("sid")
            .and_then(Value::as_str)
            .ok_or_else(|| PlatformError::parse("the guest ready frame has no session id"))?
            .to_owned();
        Ok(Self { guest, sid })
    }

    /// 下发在线扩容并等待结果。
    pub fn online_grow(&mut self, request: &OnlineRequest) -> PlatformResult<OnlineReport> {
        let frame = protocol::online_grow(
            0,
            &self.sid,
            request.partition,
            request.filesystem,
            request.bytes,
        );
        self.guest.write(&frame)?;
        let reply = until(&mut self.guest, "result")?;
        match protocol::decode_result(&reply, 0, &self.sid)? {
            Reply::Applied(report) => Ok(report),
            Reply::Failed(cause) => Err(PlatformError::not_applicable(format!(
                "the guest could not grow the filesystem: {cause}"
            ))),
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        drop(self.guest.write(&protocol::shutdown(&self.sid)));
    }
}

/// 等待指定类型的一帧；其他帧跳过。
fn until(guest: &mut Guest, wanted: &str) -> PlatformResult<Value> {
    loop {
        let frame = guest.read()?;
        if frame.get("type").and_then(Value::as_str) == Some(wanted) {
            return Ok(frame);
        }
    }
}

fn expect(guest: &mut Guest, wanted: &str) -> PlatformResult<()> {
    until(guest, wanted).map(|_| ())
}
