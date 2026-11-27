//! 载体串口协议：一行一个 JSON 帧，握手、请求、结果与关闭。
//!
//! 解析是严格的：每帧必须带 `v` 与已知 `type`；结果帧必须同时带结论与几何，
//! 缺项按协议错误结束，不按缺省值补成某一类。

use std::io::{BufRead, Write};

use diskedit_core::fs::FsKind;
use serde_json::{Value, json};

use crate::platform::{OnlineReport, PlatformError, PlatformResult};

/// 协议版本。
pub const VERSION: u64 = 1;
/// 一行帧的上限。
pub const MAX_FRAME: usize = 1 << 20;

/// 写出一帧并立即刷新。
pub fn emit(out: &mut impl Write, frame: &Value) -> std::io::Result<()> {
    serde_json::to_writer(&mut *out, frame)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error.to_string()))?;
    out.write_all(b"\n")?;
    out.flush()
}

/// 有界行读取：只接受以 `{` 开头且版本相符的帧。
pub fn read_frame(reader: &mut impl BufRead) -> PlatformResult<Value> {
    let mut line = String::new();
    let read = reader
        .read_line(&mut line)
        .map_err(|error| PlatformError::parse(format!("cannot read a carrier frame: {error}")))?;
    if read == 0 {
        return Err(PlatformError::parse("the carrier closed its output"));
    }
    if line.len() > MAX_FRAME {
        return Err(PlatformError::parse(
            "a carrier frame exceeds the size limit",
        ));
    }
    let text = line.trim();
    if !text.starts_with('{') {
        return Err(PlatformError::parse(format!(
            "the carrier sent a non-frame line: {text}"
        )));
    }
    let value: Value = serde_json::from_str(text)
        .map_err(|error| PlatformError::parse(format!("a carrier frame is not JSON: {error}")))?;
    if value.get("v").and_then(Value::as_u64) != Some(VERSION) {
        return Err(PlatformError::parse(
            "a carrier frame declares an unsupported protocol version",
        ));
    }
    if value.get("type").and_then(Value::as_str).is_none() {
        return Err(PlatformError::parse("a carrier frame has no type"));
    }
    Ok(value)
}

pub fn boot() -> Value {
    json!({ "v": VERSION, "type": "boot" })
}

pub fn hello(token: &str, binary_version: &str, disk_size: u64, sector_size: u64) -> Value {
    json!({
        "v": VERSION,
        "type": "hello",
        "token": token,
        "binary_version": binary_version,
        "disk_size": disk_size,
        "sector_size": sector_size,
    })
}

pub fn ready(binary_version: &str, disk_size: u64, sector_size: u64, sid: &str) -> Value {
    json!({
        "v": VERSION,
        "type": "ready",
        "binary_version": binary_version,
        "disk_size": disk_size,
        "sector_size": sector_size,
        "sid": sid,
    })
}

pub fn online_grow(id: u64, sid: &str, partition: u32, filesystem: FsKind, bytes: u64) -> Value {
    json!({
        "v": VERSION,
        "type": "online_grow",
        "id": id,
        "sid": sid,
        "partition": partition,
        "filesystem": filesystem.as_str(),
        "bytes": bytes,
    })
}

/// 整盘离线写入帧：每段是绝对偏移与十六进制字节。
pub fn write_disk(id: u64, sid: &str, writes: &[(u64, &[u8])]) -> Value {
    let chunks: Vec<Value> = writes
        .iter()
        .map(|(offset, bytes)| json!({ "offset": offset, "bytes": hex_encode(bytes) }))
        .collect();
    json!({
        "v": VERSION,
        "type": "write_disk",
        "id": id,
        "sid": sid,
        "writes": chunks,
    })
}

/// 解出的整盘写入段。
pub struct DiskWrite {
    pub offset: u64,
    pub bytes: Vec<u8>,
}

pub fn decode_write_disk(frame: &Value, sid: &str) -> PlatformResult<(u64, Vec<DiskWrite>)> {
    if frame.get("sid").and_then(Value::as_str) != Some(sid) {
        return Err(PlatformError::parse("a request carries a wrong session id"));
    }
    let id = frame
        .get("id")
        .and_then(Value::as_u64)
        .ok_or_else(|| PlatformError::parse("a request has no id"))?;
    let chunks = frame
        .get("writes")
        .and_then(Value::as_array)
        .ok_or_else(|| PlatformError::parse("a write request has no segments"))?;
    let mut writes = Vec::with_capacity(chunks.len());
    for chunk in chunks {
        let offset = chunk
            .get("offset")
            .and_then(Value::as_u64)
            .ok_or_else(|| PlatformError::parse("a write segment has no offset"))?;
        let text = chunk
            .get("bytes")
            .and_then(Value::as_str)
            .ok_or_else(|| PlatformError::parse("a write segment has no bytes"))?;
        let bytes = hex_decode(text)
            .ok_or_else(|| PlatformError::parse("a write segment is not hexadecimal"))?;
        writes.push(DiskWrite { offset, bytes });
    }
    if writes.is_empty() {
        return Err(PlatformError::parse("a write request has no bytes"));
    }
    Ok((id, writes))
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(char::from(b"0123456789abcdef"[(byte >> 4) as usize]));
        out.push(char::from(b"0123456789abcdef"[(byte & 0x0f) as usize]));
    }
    out
}

fn hex_decode(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        return None;
    }
    let mut out = Vec::with_capacity(text.len() / 2);
    for pair in text.as_bytes().chunks(2) {
        let hi = digit(pair[0])?;
        let lo = digit(pair[1])?;
        out.push(hi << 4 | lo);
    }
    Some(out)
}

fn digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// 来宾对一次请求的结论。
pub enum Reply {
    /// 操作完成；几何是可选的回读事实。
    Applied(OnlineReport),
    /// 操作失败；成因照原文带回。
    Failed(String),
}

pub fn result(id: u64, sid: &str, reply: &Reply) -> Value {
    match reply {
        Reply::Applied(report) => json!({
            "v": VERSION,
            "type": "result",
            "id": id,
            "sid": sid,
            "outcome": "applied",
            "blocks": report.blocks,
            "block_size": report.block_size,
            "cause": Value::Null,
        }),
        Reply::Failed(cause) => json!({
            "v": VERSION,
            "type": "result",
            "id": id,
            "sid": sid,
            "outcome": "failed",
            "blocks": Value::Null,
            "block_size": Value::Null,
            "cause": cause,
        }),
    }
}

pub fn shutdown(sid: &str) -> Value {
    json!({ "v": VERSION, "type": "shutdown", "sid": sid })
}

pub fn bye(sid: &str) -> Value {
    json!({ "v": VERSION, "type": "bye", "sid": sid })
}

/// 解出来宾的结果帧；结论、会话号与几何逐项核对。
pub fn decode_result(frame: &Value, id: u64, sid: &str) -> PlatformResult<Reply> {
    if frame.get("id").and_then(Value::as_u64) != Some(id) {
        return Err(PlatformError::parse(
            "the carrier result belongs to another request",
        ));
    }
    if frame.get("sid").and_then(Value::as_str) != Some(sid) {
        return Err(PlatformError::parse(
            "the carrier result has a wrong session id",
        ));
    }
    match frame.get("outcome").and_then(Value::as_str) {
        Some("applied") => Ok(Reply::Applied(OnlineReport {
            blocks: frame.get("blocks").and_then(Value::as_u64),
            block_size: frame.get("block_size").and_then(Value::as_u64),
        })),
        Some("failed") => {
            let cause = frame
                .get("cause")
                .and_then(Value::as_str)
                .ok_or_else(|| PlatformError::parse("a failed carrier result has no cause"))?;
            Ok(Reply::Failed(cause.to_owned()))
        }
        _ => Err(PlatformError::parse(
            "the carrier result declares no known outcome",
        )),
    }
}

/// 请求帧的字段。
pub struct Request {
    pub id: u64,
    pub partition: u32,
    pub filesystem: FsKind,
    pub bytes: u64,
}

pub fn decode_request(frame: &Value, sid: &str) -> PlatformResult<Request> {
    if frame.get("sid").and_then(Value::as_str) != Some(sid) {
        return Err(PlatformError::parse("a request carries a wrong session id"));
    }
    let id = frame
        .get("id")
        .and_then(Value::as_u64)
        .ok_or_else(|| PlatformError::parse("a request has no id"))?;
    let partition = frame
        .get("partition")
        .and_then(Value::as_u64)
        .and_then(|value| u32::try_from(value).ok())
        .ok_or_else(|| PlatformError::parse("a request has no partition number"))?;
    let filesystem = frame
        .get("filesystem")
        .and_then(Value::as_str)
        .and_then(FsKind::from_name)
        .ok_or_else(|| PlatformError::parse("a request names no known filesystem"))?;
    let bytes = frame
        .get("bytes")
        .and_then(Value::as_u64)
        .ok_or_else(|| PlatformError::parse("a request has no target size"))?;
    Ok(Request {
        id,
        partition,
        filesystem,
        bytes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_disk_round_trips_absolute_segments() {
        let frame = write_disk(
            7,
            "sid",
            &[(512, &[0xde, 0xad, 0xbe, 0xef]), (4096, &[0x01])],
        );
        let (id, writes) = decode_write_disk(&frame, "sid").expect("帧可解");
        assert_eq!(id, 7);
        assert_eq!(writes.len(), 2);
        assert_eq!(writes[0].offset, 512);
        assert_eq!(writes[0].bytes, vec![0xde, 0xad, 0xbe, 0xef]);
        assert_eq!(writes[1].offset, 4096);
        assert_eq!(writes[1].bytes, vec![0x01]);
    }

    #[test]
    fn write_disk_rejects_a_foreign_session_or_bad_hex() {
        let frame = write_disk(1, "sid", &[(0, &[0x00])]);
        assert!(decode_write_disk(&frame, "other").is_err());

        let mut bad = write_disk(1, "sid", &[(0, &[0x00])]);
        bad["writes"][0]["bytes"] = json!("0g");
        assert!(decode_write_disk(&bad, "sid").is_err());

        let mut odd = write_disk(1, "sid", &[(0, &[0x00])]);
        odd["writes"][0]["bytes"] = json!("abc");
        assert!(decode_write_disk(&odd, "sid").is_err());

        let empty = write_disk(1, "sid", &[]);
        assert!(decode_write_disk(&empty, "sid").is_err());
    }
}
