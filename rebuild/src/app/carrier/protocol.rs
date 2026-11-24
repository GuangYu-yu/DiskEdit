//! 宿主与来宾之间的帧编码。
//!
//! 一行一帧，`{` 开头的行是帧，其余行作为诊断输出转到标准错误。帧有长度上限，
//! 未知或缺字段一律判为协议错误，不做默认填充。

use std::io::{BufRead, BufReader, Read, Write};

use diskedit_core::fs::{ContentKind, FsKind, MkfsKind};
use diskedit_core::region::Region;
use diskedit_core::table::Guid;
use serde_json::{Value, json};

use crate::app::decide::step::StepKind;
use crate::app::observe::{SetProperty, TableFormat};

/// 协议版本。搬移步骤携带 `ntfs_hidden` 后升到 4。
pub const VERSION: u64 = 4;
/// 单帧最大字节数。
pub const MAX_FRAME: usize = 1 << 20;

/// 有界逐行读取器。
pub struct Lines<R> {
    reader: BufReader<R>,
    buffer: Vec<u8>,
}

impl<R: Read> Lines<R> {
    pub fn new(reader: R) -> Self {
        Self {
            reader: BufReader::new(reader),
            buffer: Vec::new(),
        }
    }

    /// 读下一帧；非帧行转到标准错误。流结束返回 `None`。
    pub fn next_frame(&mut self) -> std::io::Result<Option<Value>> {
        loop {
            self.buffer.clear();
            let read = self.reader.read_until(b'\n', &mut self.buffer)?;
            if read == 0 {
                return Ok(None);
            }
            if self.buffer.len() > MAX_FRAME {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("单帧超过 {MAX_FRAME} 字节"),
                ));
            }
            let text = String::from_utf8_lossy(&self.buffer);
            let line = text.trim_end_matches(['\r', '\n']);
            if line.trim().is_empty() {
                continue;
            }
            if !line.starts_with('{') {
                eprintln!("{line}");
                continue;
            }
            let value: Value = serde_json::from_str(line).map_err(|error| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("帧不是合法 JSON：{error}"),
                )
            })?;
            return Ok(Some(value));
        }
    }
}

/// 写出一帧。
pub fn emit<W: Write>(writer: &mut W, frame: &Value) -> std::io::Result<()> {
    let text = serde_json::to_string(frame)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    if text.len() > MAX_FRAME {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("单帧超过 {MAX_FRAME} 字节"),
        ));
    }
    writer.write_all(text.as_bytes())?;
    writer.write_all(b"\n")?;
    writer.flush()
}

/// 来宾启动后的第一帧。
pub fn boot() -> Value {
    json!({ "v": VERSION, "type": "boot" })
}

/// 宿主交由来宾执行的一份作业。
#[allow(clippy::too_many_arguments)]
pub fn hello(
    token: &str,
    binary_version: &str,
    target: &str,
    sector_size: u64,
    disk_size: u64,
    plan_digest: &[u8; 32],
    job: &str,
) -> Value {
    json!({
        "v": VERSION,
        "type": "hello",
        "token": token,
        "binary_version": binary_version,
        "target": target,
        "sector_size": sector_size,
        "disk_size": disk_size,
        "plan_digest": hex(plan_digest),
        "job": job,
    })
}

/// 来宾确认接手。
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

/// 一步作业。
pub fn step(id: u32, sid: &str, kind: &StepKind, regions: &[Region]) -> Result<Value, String> {
    let mut body = encode_kind(kind)?;
    let fields = body
        .as_object_mut()
        .ok_or_else(|| "步骤编码必须是对象".to_owned())?;
    fields.insert("v".into(), json!(VERSION));
    fields.insert("type".into(), json!("step"));
    fields.insert("id".into(), json!(id));
    fields.insert("sid".into(), json!(sid));
    fields.insert(
        "regions".into(),
        json!(
            regions
                .iter()
                .map(|region| json!([region.start(), region.len()]))
                .collect::<Vec<_>>()
        ),
    );
    Ok(body)
}

/// 一步的结果。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StepResult {
    pub applied: bool,
    pub class: Option<u8>,
    pub cause: Option<String>,
}

/// 来宾回报的结果帧。
pub fn result(id: u32, sid: &str, result: &StepResult) -> Value {
    let mut frame = json!({
        "v": VERSION,
        "type": "result",
        "id": id,
        "sid": sid,
        "outcome": if result.applied { "applied" } else { "failed" },
    });
    if !result.applied {
        if let Some(class) = result.class {
            frame["class"] = json!(class);
        }
        if let Some(cause) = &result.cause {
            frame["cause"] = json!(cause);
        }
    }
    frame
}

/// 解码结果帧，返回步骤号与结果。
pub fn decode_result(frame: &Value) -> Result<(u32, StepResult), String> {
    let id = number_u64(frame, "id")?;
    let id = u32::try_from(id).map_err(|_| "结果帧的步骤号超出范围".to_owned())?;
    let applied = match text(frame, "outcome")? {
        "applied" => true,
        "failed" => false,
        other => return Err(format!("未知的结果 {other}")),
    };
    let class = frame
        .get("class")
        .and_then(Value::as_u64)
        .map(|value| u8::try_from(value).unwrap_or(u8::MAX));
    let cause = frame
        .get("cause")
        .and_then(Value::as_str)
        .map(str::to_owned);
    Ok((
        id,
        StepResult {
            applied,
            class,
            cause,
        },
    ))
}

/// 收尾帧。
pub fn shutdown(sid: &str) -> Value {
    json!({ "v": VERSION, "type": "shutdown", "sid": sid })
}

/// 收尾确认。
pub fn bye(sid: &str) -> Value {
    json!({ "v": VERSION, "type": "bye", "sid": sid })
}

/// 解码一步。
pub fn decode_step(frame: &Value) -> Result<(StepKind, Vec<Region>), String> {
    let kind = decode_kind(frame)?;
    let regions = frame
        .get("regions")
        .and_then(Value::as_array)
        .ok_or_else(|| "步骤缺少 regions".to_owned())?;
    let mut decoded = Vec::with_capacity(regions.len());
    for region in regions {
        let pair = region
            .as_array()
            .filter(|pair| pair.len() == 2)
            .ok_or_else(|| "区间必须是 [起点, 长度]".to_owned())?;
        let start = pair[0]
            .as_u64()
            .ok_or_else(|| "区间起点必须是整数".to_owned())?;
        let len = pair[1]
            .as_u64()
            .ok_or_else(|| "区间长度必须是整数".to_owned())?;
        let end = start
            .checked_add(len)
            .ok_or_else(|| "区间越过 64 位上界".to_owned())?;
        decoded.push(Region::new(start, end).map_err(|error| format!("区间无效：{error:?}"))?);
    }
    Ok((kind, decoded))
}

fn encode_kind(kind: &StepKind) -> Result<Value, String> {
    let frame = match kind {
        StepKind::WriteTable { format } => json!({
            "kind": "write-table",
            "format": format_name(*format),
        }),
        StepKind::AddPartition {
            format,
            start_lba,
            sectors,
            type_guid,
            name,
            part_number,
            unique,
        } => json!({
            "kind": "add-partition",
            "format": format_name(*format),
            "start_lba": start_lba,
            "sectors": sectors,
            "type_guid": hex(&type_guid.0),
            "name": name,
            "part_number": part_number,
            "unique": unique.map(|guid| hex(&guid.0)),
        }),
        StepKind::DeletePartition { format, partition } => json!({
            "kind": "delete-partition",
            "format": format_name(*format),
            "partition": partition,
        }),
        StepKind::ResizePartition {
            format,
            partition,
            start_lba,
            sectors,
        } => json!({
            "kind": "resize-partition",
            "format": format_name(*format),
            "partition": partition,
            "start_lba": start_lba,
            "sectors": sectors,
        }),
        StepKind::SetProperty {
            partition,
            property,
            value,
            content,
        } => json!({
            "kind": "set-property",
            "partition": partition,
            "property": property_name(*property),
            "value": value,
            "content": content.map(ContentKind::as_str),
        }),
        StepKind::MakeFilesystem {
            partition,
            filesystem,
            label,
        } => json!({
            "kind": "make-filesystem",
            "partition": partition,
            "filesystem": filesystem.as_str(),
            "label": label,
        }),
        StepKind::GrowFilesystem {
            partition,
            filesystem,
            bytes,
        } => json!({
            "kind": "grow-filesystem",
            "partition": partition,
            "filesystem": filesystem.as_str(),
            "bytes": bytes,
        }),
        StepKind::ShrinkFilesystem {
            partition,
            filesystem,
            bytes,
        } => json!({
            "kind": "shrink-filesystem",
            "partition": partition,
            "filesystem": filesystem.as_str(),
            "bytes": bytes,
        }),
        StepKind::CopyPartition {
            source,
            start_lba,
            sectors,
            chunk,
        } => json!({
            "kind": "copy-partition",
            "source": source,
            "start_lba": start_lba,
            "sectors": sectors,
            "chunk": chunk,
        }),
        StepKind::MovePartition {
            partition,
            offset_sectors,
            chunk,
            ntfs_hidden,
        } => json!({
            "kind": "move-partition",
            "partition": partition,
            "offset_sectors": offset_sectors,
            "chunk": chunk,
            "ntfs_hidden": ntfs_hidden,
        }),
        StepKind::RefreshKernelView => json!({
            "kind": "refresh-kernel-view",
        }),
        StepKind::Inspect
        | StepKind::Check { .. }
        | StepKind::ListRecords
        | StepKind::VerifyRecord { .. }
        | StepKind::ScanRecords
        | StepKind::RecoverRecords { .. } => {
            return Err("该步骤不写目标字节，不进载体协议".to_owned());
        }
    };
    Ok(frame)
}

fn decode_kind(frame: &Value) -> Result<StepKind, String> {
    let kind = text(frame, "kind")?;
    let decoded = match kind {
        "write-table" => StepKind::WriteTable {
            format: parse_format(text(frame, "format")?)?,
        },
        "add-partition" => StepKind::AddPartition {
            format: parse_format(text(frame, "format")?)?,
            start_lba: number_u64(frame, "start_lba")?,
            sectors: number_u64(frame, "sectors")?,
            type_guid: guid(frame, "type_guid")?,
            name: text(frame, "name")?.to_owned(),
            part_number: optional_u32(frame, "part_number")?,
            unique: optional_guid(frame, "unique")?,
        },
        "delete-partition" => StepKind::DeletePartition {
            format: parse_format(text(frame, "format")?)?,
            partition: number_u32(frame, "partition")?,
        },
        "resize-partition" => StepKind::ResizePartition {
            format: parse_format(text(frame, "format")?)?,
            partition: number_u32(frame, "partition")?,
            start_lba: number_u64(frame, "start_lba")?,
            sectors: number_u64(frame, "sectors")?,
        },
        "set-property" => StepKind::SetProperty {
            partition: number_u32(frame, "partition")?,
            property: parse_property(text(frame, "property")?)?,
            value: text(frame, "value")?.to_owned(),
            content: optional_content(frame)?,
        },
        "make-filesystem" => StepKind::MakeFilesystem {
            partition: number_u32(frame, "partition")?,
            filesystem: MkfsKind::parse(text(frame, "filesystem")?)
                .ok_or_else(|| "未知的文件系统类型".to_owned())?,
            label: optional_text(frame, "label")?,
        },
        "grow-filesystem" => StepKind::GrowFilesystem {
            partition: number_u32(frame, "partition")?,
            filesystem: FsKind::from_name(text(frame, "filesystem")?)
                .ok_or_else(|| "未知的文件系统族".to_owned())?,
            bytes: number_u64(frame, "bytes")?,
        },
        "shrink-filesystem" => StepKind::ShrinkFilesystem {
            partition: number_u32(frame, "partition")?,
            filesystem: FsKind::from_name(text(frame, "filesystem")?)
                .ok_or_else(|| "未知的文件系统族".to_owned())?,
            bytes: number_u64(frame, "bytes")?,
        },
        "copy-partition" => StepKind::CopyPartition {
            source: number_u32(frame, "source")?,
            start_lba: number_u64(frame, "start_lba")?,
            sectors: number_u64(frame, "sectors")?,
            chunk: number_u64(frame, "chunk")?,
        },
        "move-partition" => StepKind::MovePartition {
            partition: number_u32(frame, "partition")?,
            offset_sectors: number_i64(frame, "offset_sectors")?,
            chunk: number_u64(frame, "chunk")?,
            ntfs_hidden: boolean(frame, "ntfs_hidden")?,
        },
        "refresh-kernel-view" => StepKind::RefreshKernelView,
        other => return Err(format!("未知的步骤 {other}")),
    };
    Ok(decoded)
}

/// 十六进制编码。
pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn parse_hex16(text: &str) -> Option<Guid> {
    let bytes = text.as_bytes();
    if bytes.len() != 32 {
        return None;
    }
    let mut guid = [0u8; 16];
    for (index, chunk) in bytes.chunks(2).enumerate() {
        let pair = std::str::from_utf8(chunk).ok()?;
        guid[index] = u8::from_str_radix(pair, 16).ok()?;
    }
    Some(Guid(guid))
}

fn format_name(format: TableFormat) -> &'static str {
    match format {
        TableFormat::Gpt => "gpt",
        TableFormat::Mbr => "mbr",
    }
}

fn parse_format(name: &str) -> Result<TableFormat, String> {
    match name {
        "gpt" => Ok(TableFormat::Gpt),
        "mbr" => Ok(TableFormat::Mbr),
        other => Err(format!("未知的表格式 {other}")),
    }
}

fn property_name(property: SetProperty) -> &'static str {
    match property {
        SetProperty::Name => "name",
        SetProperty::Label => "label",
        SetProperty::Uuid => "uuid",
        SetProperty::Flag => "flag",
        SetProperty::Type => "type",
    }
}

fn parse_property(name: &str) -> Result<SetProperty, String> {
    match name {
        "name" => Ok(SetProperty::Name),
        "label" => Ok(SetProperty::Label),
        "uuid" => Ok(SetProperty::Uuid),
        "flag" => Ok(SetProperty::Flag),
        "type" => Ok(SetProperty::Type),
        other => Err(format!("未知的属性 {other}")),
    }
}

fn text<'a>(frame: &'a Value, name: &str) -> Result<&'a str, String> {
    frame
        .get(name)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("缺少字符串字段 {name}"))
}

fn optional_text(frame: &Value, name: &str) -> Result<Option<String>, String> {
    match frame.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value.clone())),
        Some(_) => Err(format!("{name} 必须是字符串或 null")),
    }
}

fn boolean(frame: &Value, name: &str) -> Result<bool, String> {
    frame
        .get(name)
        .and_then(Value::as_bool)
        .ok_or_else(|| format!("缺少布尔字段 {name}"))
}

fn number_u64(frame: &Value, name: &str) -> Result<u64, String> {
    frame
        .get(name)
        .and_then(Value::as_u64)
        .ok_or_else(|| format!("缺少整数字段 {name}"))
}

fn number_u32(frame: &Value, name: &str) -> Result<u32, String> {
    let value = number_u64(frame, name)?;
    u32::try_from(value).map_err(|_| format!("{name} 超出范围"))
}

fn number_i64(frame: &Value, name: &str) -> Result<i64, String> {
    frame
        .get(name)
        .and_then(Value::as_i64)
        .ok_or_else(|| format!("缺少整数字段 {name}"))
}

fn optional_u32(frame: &Value, name: &str) -> Result<Option<u32>, String> {
    match frame.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(_) => number_u32(frame, name).map(Some),
    }
}

fn guid(frame: &Value, name: &str) -> Result<Guid, String> {
    parse_hex16(text(frame, name)?).ok_or_else(|| format!("{name} 必须是 32 位十六进制 GUID"))
}

fn optional_guid(frame: &Value, name: &str) -> Result<Option<Guid>, String> {
    match frame.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(_) => guid(frame, name).map(Some),
    }
}

fn optional_content(frame: &Value) -> Result<Option<ContentKind>, String> {
    match frame.get("content") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(name)) => ContentKind::from_name(name)
            .map(Some)
            .ok_or_else(|| format!("未知的内容类别 {name}")),
        Some(_) => Err("content 必须是字符串或 null".to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn roundtrip(kind: StepKind) {
        let regions = vec![
            Region::new(1024, 3072).expect("区间"),
            Region::new(0, 512).expect("区间"),
        ];
        let frame = step(7, "sid", &kind, &regions).expect("编码");
        let (decoded, decoded_regions) = decode_step(&frame).expect("解码");
        assert_eq!(decoded, kind);
        assert_eq!(decoded_regions, regions);
    }

    #[test]
    fn every_writable_step_roundtrips() {
        roundtrip(StepKind::WriteTable {
            format: TableFormat::Gpt,
        });
        roundtrip(StepKind::AddPartition {
            format: TableFormat::Mbr,
            start_lba: 2048,
            sectors: 4096,
            type_guid: Guid([1; 16]),
            name: "data".into(),
            part_number: Some(2),
            unique: None,
        });
        roundtrip(StepKind::DeletePartition {
            format: TableFormat::Gpt,
            partition: 3,
        });
        roundtrip(StepKind::ResizePartition {
            format: TableFormat::Gpt,
            partition: 1,
            start_lba: 2048,
            sectors: 8192,
        });
        roundtrip(StepKind::SetProperty {
            partition: 1,
            property: SetProperty::Label,
            value: "root".into(),
            content: Some(ContentKind::Filesystem(FsKind::Ext)),
        });
        roundtrip(StepKind::MakeFilesystem {
            partition: 1,
            filesystem: MkfsKind::Ext4,
            label: Some("root".into()),
        });
        roundtrip(StepKind::GrowFilesystem {
            partition: 1,
            filesystem: FsKind::Ext,
            bytes: 1 << 30,
        });
        roundtrip(StepKind::ShrinkFilesystem {
            partition: 1,
            filesystem: FsKind::Ntfs,
            bytes: 1 << 29,
        });
        roundtrip(StepKind::CopyPartition {
            source: 1,
            start_lba: 4096,
            sectors: 2048,
            chunk: 1 << 20,
        });
        roundtrip(StepKind::MovePartition {
            partition: 1,
            offset_sectors: -512,
            chunk: 1 << 20,
            ntfs_hidden: true,
        });
    }

    #[test]
    fn read_only_steps_do_not_enter_the_protocol() {
        assert!(step(1, "sid", &StepKind::Inspect, &[]).is_err());
    }

    #[test]
    fn kernel_view_refresh_roundtrips() {
        roundtrip(StepKind::RefreshKernelView);
    }

    #[test]
    fn missing_field_is_a_protocol_error() {
        let frame = json!({
            "v": VERSION,
            "type": "step",
            "id": 1,
            "sid": "sid",
            "kind": "resize-partition",
            "format": "gpt",
            "partition": 1,
            "start_lba": 2048,
            "regions": [],
        });
        assert!(decode_step(&frame).is_err());
    }

    #[test]
    fn unknown_kind_is_a_protocol_error() {
        let frame = json!({
            "v": VERSION,
            "type": "step",
            "id": 1,
            "sid": "sid",
            "kind": "teleport",
            "regions": [],
        });
        assert!(decode_step(&frame).is_err());
    }

    #[test]
    fn noise_lines_are_skipped_and_frames_decoded() {
        let input = b"kernel: hello\n{\"v\":3,\"type\":\"boot\"}\n\n";
        let mut lines = Lines::new(Cursor::new(input.to_vec()));
        let frame = lines.next_frame().expect("读取").expect("帧");
        assert_eq!(frame["type"], "boot");
        assert!(lines.next_frame().expect("读取").is_none());
    }

    #[test]
    fn oversized_line_is_rejected() {
        let mut input = vec![b'{'; MAX_FRAME + 2];
        input.push(b'\n');
        let mut lines = Lines::new(Cursor::new(input));
        assert!(lines.next_frame().is_err());
    }

    #[test]
    fn result_roundtrips() {
        let failed = StepResult {
            applied: false,
            class: Some(5),
            cause: Some("已写部分字节".into()),
        };
        let frame = result(9, "sid", &failed);
        let (id, decoded) = decode_result(&frame).expect("解码");
        assert_eq!(id, 9);
        assert_eq!(decoded, failed);
    }
}
