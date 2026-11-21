//! 载体会话的帧编码：一行一个 JSON 对象，握手、步骤、检查点、结果与关闭。
//!
//! 解析是严格的：每帧必须有 `v: 2` 与已知 `type`；尺寸超过 [`MAX_FRAME`] 的帧按
//! 协议错误结束，不以非 `{` 开头的行当噪声跳过并转交 stderr。结果帧必须带
//! `outcome`、`pending`、`probe`、`class`、`cause` 五个键：结论与成因一起过线，
//! 缺任何一项都按协议错误结束，而不是按缺省值补成某一类

use std::io::{self, BufRead, Write};

use serde_json::{Value, json};

use crate::dev::RegionStamp;
use crate::domain::ContentKind;
use crate::execution::copy::CopyPartition;
use crate::execution::fs::FsResize;
use crate::execution::mkfs::FsCreate;
use crate::execution::relocate::Relocate;
use crate::execution::set::FsSet;
use crate::execution::swap::SwapRebuild;
use crate::outcome::{FailureClass, Pending, PendingKind, RunOutcome};
use crate::planner::{FsSetAttr, UuidValue};
use crate::table::TableStep;

use super::{CarrierStep, FsStep, ProbeReport, StepOutcome, TransferStep};

/// 帧版本。结果帧携带失败类别之后从 1 升到 2（旧 worker 的结果没有 `class`，按协议错误
/// 结束；握手阶段的 `binary_version` 复核会更早地以 6 拒掉旧 bundle）；握手应答签发会话号、
/// 其后每一帧都必须回带它，从 2 升到 3——旧宿主发的帧没有 `sid`，必须在这里明确失败，
/// 而不是被当成"字段缺失的 step"混过去
pub(crate) const VERSION: u64 = 3;
/// 一行帧的上限：1 MiB（取值来自 `support::MIB_BYTES`，不在这里手写乘积）
pub(crate) const MAX_FRAME: usize = crate::support::MIB_BYTES as usize;
pub(crate) const BINARY_VERSION: &str = env!("CARGO_PKG_VERSION");

/// 读出的一帧或一行噪声
pub(crate) enum Read {
    Frame(Value),
    /// 不以 `{` 开头：外部工具的输出，转交 stderr
    Noise(String),
    Oversize,
    Closed,
}

/// 有界行读取器。超过上限的行只记账不保留，由调用方按协议错误处理
pub(crate) struct Lines<R> {
    reader: R,
}

impl<R: BufRead> Lines<R> {
    pub(crate) fn new(reader: R) -> Self {
        Self { reader }
    }

    pub(crate) fn read(&mut self) -> io::Result<Read> {
        let mut bytes = Vec::new();
        let mut oversize = false;
        let mut saw = false;
        loop {
            let (chunk_len, newline) = {
                let buf = self.reader.fill_buf()?;
                if buf.is_empty() {
                    if !saw {
                        return Ok(Read::Closed);
                    }
                    break;
                }
                let newline = buf.iter().position(|&b| b == b'\n');
                let len = newline.map_or(buf.len(), |i| i + 1);
                if !oversize {
                    if bytes.len() + len > MAX_FRAME {
                        oversize = true;
                        bytes.clear();
                    } else {
                        bytes.extend_from_slice(&buf[..len]);
                    }
                }
                (len, newline.is_some())
            };
            saw = true;
            self.reader.consume(chunk_len);
            if newline {
                break;
            }
        }
        if oversize {
            return Ok(Read::Oversize);
        }
        let text = String::from_utf8(bytes).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidData, "a frame is not valid utf-8")
        })?;
        let text = text.trim_end_matches(['\r', '\n']);
        if !text.starts_with('{') {
            return Ok(Read::Noise(text.to_string()));
        }
        let value: Value = serde_json::from_str(text).map_err(|e| {
            io::Error::new(io::ErrorKind::InvalidData, format!("a frame is not JSON: {e}"))
        })?;
        if value.get("v").and_then(Value::as_u64) != Some(VERSION) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "a frame declares an unsupported protocol version",
            ));
        }
        if value.get("type").and_then(Value::as_str).is_none() {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "a frame has no type"));
        }
        Ok(Read::Frame(value))
    }
}

/// 一次性会话令牌：128 位随机值的十六进制文本
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) fn random_token() -> Option<String> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).ok()?;
    Some(hex(&bytes))
}

pub(crate) fn hex_digest(digest: &[u8; 32]) -> String {
    hex(digest)
}

pub(crate) fn digest_from_hex(text: &str) -> Option<[u8; 32]> {
    unhex(text, 32)
}

/// 写出一帧并立即刷新：宿主与 guest 都用同一份行格式
pub(crate) fn emit(out: &mut impl Write, value: &Value) -> io::Result<()> {
    serde_json::to_writer(&mut *out, value)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
    out.write_all(b"\n")?;
    out.flush()
}

/// 线上步骤：与 [`CarrierStep`] 一一对应，但拥有自己的数据，可以跨进程传递
pub(crate) enum WireStep {
    Table(TableStep),
    Relocate(Relocate),
    Copy { step: CopyPartition, chunk: u64 },
    FsResize(FsResize),
    FsCreate(FsCreate),
    FsSet(FsSet),
    RebuildSwap(SwapRebuild),
    HiddenSectors(Relocate),
}

impl WireStep {
    pub(crate) fn carrier(&self) -> CarrierStep<'_> {
        match self {
            Self::Table(step) => CarrierStep::TableCommit(step),
            Self::Relocate(step) => CarrierStep::Transfer(TransferStep::Relocate(step)),
            Self::Copy { step, chunk } => {
                CarrierStep::Transfer(TransferStep::Copy { step, chunk: *chunk })
            }
            Self::FsResize(step) => CarrierStep::Fs(FsStep::Resize(step)),
            Self::FsCreate(step) => CarrierStep::Fs(FsStep::Create(step)),
            Self::FsSet(step) => CarrierStep::Fs(FsStep::Set(step)),
            Self::RebuildSwap(step) => CarrierStep::Fs(FsStep::RebuildSwap(step)),
            Self::HiddenSectors(step) => CarrierStep::Fs(FsStep::HiddenSectors(step)),
        }
    }
}

impl TryFrom<CarrierStep<'_>> for WireStep {
    type Error = RunOutcome;

    fn try_from(step: CarrierStep<'_>) -> Result<Self, Self::Error> {
        Ok(match step {
            CarrierStep::TableCommit(step) => Self::Table(step.clone()),
            CarrierStep::Transfer(TransferStep::Relocate(step)) => Self::Relocate(*step),
            CarrierStep::Transfer(TransferStep::Copy { step, chunk }) => {
                Self::Copy { step: step.clone(), chunk }
            }
            CarrierStep::Fs(FsStep::Resize(step)) => Self::FsResize(step.clone()),
            CarrierStep::Fs(FsStep::Create(step)) => Self::FsCreate(step.clone()),
            CarrierStep::Fs(FsStep::Set(step)) => Self::FsSet(step.clone()),
            CarrierStep::Fs(FsStep::RebuildSwap(step)) => Self::RebuildSwap(step.clone()),
            CarrierStep::Fs(FsStep::HiddenSectors(step)) => Self::HiddenSectors(*step),
            CarrierStep::Probe(_) => {
                return Err(RunOutcome::unsupported(
                    "a probe is not a step of a plan; the session issues it after an applied step",
                ));
            }
        })
    }
}

/// 步骤帧的 `step` 字段
pub(crate) fn encode_step(step: &WireStep) -> Value {
    match step {
        WireStep::Table(step) => json!({
            "kind": "table",
            "table_kind": step.kind.tag(),
            "regions": step.regions.iter().map(|r| json!([r.offset, r.len])).collect::<Vec<_>>(),
            "expect_regions": step.expect_regions.iter().map(encode_stamp).collect::<Vec<_>>(),
            "writes": step.writes.iter().map(|w| json!([w.region.offset, hex(&w.bytes)])).collect::<Vec<_>>(),
            "before": step.before.iter().map(encode_stamp).collect::<Vec<_>>(),
            "expect_before": step.expect_before.map(|s| hex(&s.bytes())),
            "expect_after": step.expect_after.map(|s| hex(&s.bytes())),
        }),
        WireStep::Relocate(step) => json!({
            "kind": "relocate",
            "part": step.part,
            "src_first": step.src_first,
            "dst_first": step.dst_first,
            "sectors": step.sectors,
            "chunk_bytes": step.chunk_bytes,
            "chunk": step.chunk,
            "chunks": step.chunks,
        }),
        WireStep::Copy { step, chunk } => json!({
            "kind": "copy",
            "part": step.part,
            "src_first": step.src_first,
            "dst_first": step.dst_first,
            "sectors": step.sectors,
            "chunk": chunk,
        }),
        WireStep::FsResize(step) => json!({
            "kind": "fs_resize",
            "part": step.part,
            "first_lba": step.first_lba,
            "sectors": step.sectors,
            "target_bytes": step.target_bytes,
            "fstype": step.fstype.map(|k| k.as_str()),
        }),
        WireStep::FsCreate(step) => json!({
            "kind": "fs_create",
            "part": step.part,
            "first_lba": step.first_lba,
            "sectors": step.sectors,
            "fstype": step.fstype.as_str(),
            "label": step.label,
        }),
        WireStep::FsSet(step) => json!({
            "kind": "fs_set",
            "part": step.part,
            "fstype": step.fstype.map(|k| k.as_str()),
            "attr": encode_attr(&step.attr),
        }),
        WireStep::RebuildSwap(step) => json!({
            "kind": "rebuild_swap",
            "part": step.part,
            "src_first": step.src_first,
            "dst_first": step.dst_first,
            "sectors": step.sectors,
        }),
        WireStep::HiddenSectors(step) => json!({
            "kind": "hidden_sectors",
            "part": step.part,
            "src_first": step.src_first,
            "dst_first": step.dst_first,
            "sectors": step.sectors,
        }),
    }
}

/// 严格解出步骤；缺字段、类型不符或未知类别都返回 `None`。
///
/// `sector_size` 是本次会话的逻辑块大小：搬移步骤的块布局在这里复核一次，只接受
/// [`crate::planner::chunk::move_chunk_layout`] 算得出的那一组。计划执行器已经算过一次，
/// 线上传输的那份是它的副本；副本与算式的任何漂移（例如自重叠搬移按大于位移的块）
/// 都会让续跑读到已被自己覆盖的源，故在这一道边界上拒绝，而不是照单执行
pub(crate) fn decode_step(value: &Value, sector_size: u64) -> Option<WireStep> {
    Some(match field(value, "kind")? {
        "table" => WireStep::Table(decode_table(value)?),
        "relocate" => {
            let step = Relocate {
                part: u32_of(value, "part")?,
                src_first: u64_of(value, "src_first")?,
                dst_first: u64_of(value, "dst_first")?,
                sectors: u64_of(value, "sectors")?,
                chunk_bytes: u64_of(value, "chunk_bytes")?,
                chunk: u64_of(value, "chunk")?,
                chunks: u64_of(value, "chunks")?,
            };
            let shift = step.dst_first.abs_diff(step.src_first);
            let (chunk, chunks) = crate::planner::chunk::move_chunk_layout(
                step.sectors,
                sector_size,
                shift,
                step.sectors,
                step.chunk_bytes,
            )?;
            if (chunk, chunks) != (step.chunk, step.chunks) {
                return None;
            }
            WireStep::Relocate(step)
        }
        "copy" => WireStep::Copy {
            step: CopyPartition {
                part: u32_of(value, "part")?,
                src_first: u64_of(value, "src_first")?,
                dst_first: u64_of(value, "dst_first")?,
                sectors: u64_of(value, "sectors")?,
            },
            chunk: u64_of(value, "chunk")?,
        },
        "fs_resize" => WireStep::FsResize(FsResize {
            part: u32_of(value, "part")?,
            first_lba: u64_of(value, "first_lba")?,
            sectors: u64_of(value, "sectors")?,
            target_bytes: u64_of(value, "target_bytes")?,
            fstype: optional_content(value.get("fstype")?)?,
        }),
        "fs_create" => WireStep::FsCreate(FsCreate {
            part: u32_of(value, "part")?,
            first_lba: u64_of(value, "first_lba")?,
            sectors: u64_of(value, "sectors")?,
            fstype: crate::fsops::parse_fstype(field(value, "fstype")?).ok()?,
            label: optional_string(value.get("label")?)?,
        }),
        "fs_set" => WireStep::FsSet(FsSet {
            part: u32_of(value, "part")?,
            fstype: optional_content(value.get("fstype")?)?,
            attr: decode_attr(value.get("attr")?)?,
        }),
        "rebuild_swap" => WireStep::RebuildSwap(SwapRebuild {
            part: u32_of(value, "part")?,
            src_first: u64_of(value, "src_first")?,
            dst_first: u64_of(value, "dst_first")?,
            sectors: u64_of(value, "sectors")?,
        }),
        "hidden_sectors" => WireStep::HiddenSectors(Relocate {
            part: u32_of(value, "part")?,
            src_first: u64_of(value, "src_first")?,
            dst_first: u64_of(value, "dst_first")?,
            sectors: u64_of(value, "sectors")?,
            chunk_bytes: 0,
            chunk: 0,
            chunks: 0,
        }),
        _ => return None,
    })
}

fn encode_attr(attr: &FsSetAttr) -> Value {
    match attr {
        FsSetAttr::Label(text) => json!({ "kind": "label", "value": text }),
        FsSetAttr::Uuid(UuidValue::Explicit(text)) => json!({ "kind": "uuid", "value": text }),
        FsSetAttr::Uuid(UuidValue::Generate) => json!({ "kind": "uuid", "value": null }),
    }
}

fn decode_attr(value: &Value) -> Option<FsSetAttr> {
    match field(value, "kind")? {
        "label" => Some(FsSetAttr::Label(field(value, "value")?.to_string())),
        "uuid" => Some(FsSetAttr::Uuid(match value.get("value")? {
            Value::String(text) => UuidValue::Explicit(text.clone()),
            Value::Null => UuidValue::Generate,
            _ => return None,
        })),
        _ => None,
    }
}

fn encode_stamp(stamp: &RegionStamp) -> Value {
    let (offset, len, digest) = stamp.parts();
    json!([offset, len, digest.map(|d| hex(&d))])
}

fn decode_stamp(value: &Value) -> Option<RegionStamp> {
    let items = value.as_array()?;
    if items.len() != 3 {
        return None;
    }
    let offset = items[0].as_u64()?;
    let len = items[1].as_u64()?;
    let digest = match &items[2] {
        Value::Null => None,
        Value::String(text) => Some(unhex(text, 32)?),
        _ => return None,
    };
    Some(RegionStamp::from_parts(offset, len, digest))
}

fn decode_table(value: &Value) -> Option<TableStep> {
    let kind = crate::table::TableWriteKind::from_tag(u8::try_from(u64_of(value, "table_kind")?).ok()?)?;
    let regions = decode_vec(value.get("regions")?, |item| {
        let pair = item.as_array()?;
        if pair.len() != 2 {
            return None;
        }
        Some(crate::table::TableRegion { offset: pair[0].as_u64()?, len: pair[1].as_u64()? })
    })?;
    let expect_regions = decode_vec(value.get("expect_regions")?, decode_stamp)?;
    let writes = decode_vec(value.get("writes")?, |item| {
        let pair = item.as_array()?;
        if pair.len() != 2 {
            return None;
        }
        let bytes = unhex_len(pair[1].as_str()?)?;
        Some(crate::table::StepWrite {
            region: crate::table::TableRegion {
                offset: pair[0].as_u64()?,
                len: bytes.len() as u64,
            },
            bytes,
        })
    })?;
    let before = decode_vec(value.get("before")?, decode_stamp)?;
    let expect_before = optional_table_stamp(value.get("expect_before")?)?;
    let expect_after = optional_table_stamp(value.get("expect_after")?)?;
    Some(TableStep { kind, regions, expect_regions, writes, before, expect_before, expect_after })
}

fn optional_table_stamp(value: &Value) -> Option<Option<crate::table::TableStamp>> {
    match value {
        Value::Null => Some(None),
        Value::String(text) => Some(Some(crate::table::TableStamp::from_bytes(unhex(text, 32)?))),
        _ => None,
    }
}

/// 宿主读到的一步结果：要么是载体交回的结论，要么是载体判定过的失败。
///
/// 失败的**类别**过线，宿主据此重建 `RunOutcome`——不按文案反推、也不折成
/// "未知"。这让"没做成"在宿主侧仍然只有一种表示：`Err`
pub(crate) enum Decoded {
    Run(super::StepRun),
    Failed(RunOutcome),
}

/// 步骤的执行结果帧。`Applied`/`Unknown` 之外没有第三种结论：没做成由
/// [`encode_failure`] 走失败帧，两边各只有一个方向
pub(crate) fn encode_result(run: &super::StepRun) -> Value {
    json!({
        "type": "result",
        "outcome": outcome_name(run.outcome),
        "pending": run.pending.iter().map(encode_pending).collect::<Vec<_>>(),
        "probe": run.probe.map(|p| json!({ "part": p.part, "content": p.content.as_str() })),
        "cause": run.unfinished,
    })
}

pub(crate) fn decode_result(value: &Value) -> Option<Decoded> {
    let pending = decode_vec(value.get("pending")?, decode_pending)?;
    let probe = match value.get("probe")? {
        Value::Null => None,
        item => Some(ProbeReport {
            part: u32_of(item, "part")?,
            content: ContentKind::from_name(field(item, "content")?)?,
        }),
    };
    let cause = optional_string(value.get("cause")?)?;
    let outcome = field(value, "outcome")?;
    if outcome != "applied" && outcome != "unknown" {
        // 失败帧：类别与成因都必须有，否则无从重建一个具体的出口结论
        let class = FailureClass::from_code(u8::try_from(u64_of(value, "class")?).ok()?)?;
        return Some(Decoded::Failed(class.outcome(cause?)));
    }
    let outcome = if outcome == "applied" { StepOutcome::Applied } else { StepOutcome::Unknown };
    Some(Decoded::Run(super::StepRun::decoded(outcome, pending, probe, cause)))
}

/// 失败结果：`RunOutcome` 的结论、类别与成因按线格式表达
pub(crate) fn encode_failure(error: &RunOutcome) -> Value {
    json!({
        "type": "result",
        "outcome": failure_outcome(error),
        "pending": [],
        "probe": null,
        "class": FailureClass::from_code(error.exit_code()).map(|class| class.code()),
        "cause": failure_text(error),
    })
}

fn outcome_name(outcome: StepOutcome) -> &'static str {
    match outcome {
        StepOutcome::Applied => "applied",
        StepOutcome::Unknown => "unknown",
    }
}

fn encode_pending(pending: &Pending) -> Value {
    let kind = match pending.kind {
        PendingKind::Fs => "fs",
        PendingKind::Swap => "swap",
        PendingKind::RuntimeInitialization => "runtime",
        PendingKind::Other(label) => label,
    };
    json!({ "kind": kind, "part": pending.part, "detail": pending.detail, "hint": pending.hint })
}

fn decode_pending(value: &Value) -> Option<Pending> {
    let kind = match field(value, "kind")? {
        "fs" => PendingKind::Fs,
        "swap" => PendingKind::Swap,
        "runtime" => PendingKind::RuntimeInitialization,
        _ => PendingKind::Other("worker"),
    };
    Some(Pending {
        part: u32_of(value, "part")?,
        kind,
        detail: field(value, "detail")?.to_string(),
        hint: field(value, "hint")?.to_string(),
    })
}

/// 失败结论：5 与 6 之外都按未执行处理
fn failure_outcome(error: &RunOutcome) -> &'static str {
    match error.exit_code() {
        5 => "unknown",
        _ => "not_applied",
    }
}

/// 失败成因：`RunOutcome` 的文案，供宿主原样呈现
pub(crate) fn failure_text(error: &RunOutcome) -> String {
    match error {
        RunOutcome::Usage(text) | RunOutcome::Refused(text) | RunOutcome::Internal(text) => text.clone(),
        RunOutcome::NoMutation { cause }
        | RunOutcome::Unsupported { cause }
        | RunOutcome::Partial { cause, .. } => cause.clone(),
        RunOutcome::Applied { .. } => "the step reported success".to_string(),
        // 步骤是写盘的一段，它不会被"只读复核"代表；真收到就是协议里混进了别的结论
        RunOutcome::Checked => "the step reported a read-only verdict".to_string(),
    }
}

// —— JSON 取值 —— //

fn field<'a>(value: &'a Value, name: &str) -> Option<&'a str> {
    value.get(name)?.as_str()
}

fn u64_of(value: &Value, name: &str) -> Option<u64> {
    value.get(name)?.as_u64()
}

fn u32_of(value: &Value, name: &str) -> Option<u32> {
    u32::try_from(u64_of(value, name)?).ok()
}

fn optional_string(value: &Value) -> Option<Option<String>> {
    match value {
        Value::Null => Some(None),
        Value::String(text) => Some(Some(text.clone())),
        _ => None,
    }
}

fn optional_content(value: &Value) -> Option<Option<ContentKind>> {
    match value {
        Value::Null => Some(None),
        Value::String(text) => Some(Some(ContentKind::from_name(text)?)),
        _ => None,
    }
}

fn decode_vec<T>(value: &Value, mut item: impl FnMut(&Value) -> Option<T>) -> Option<Vec<T>> {
    value.as_array()?.iter().map(&mut item).collect()
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        out.push(DIGITS[(byte >> 4) as usize] as char);
        out.push(DIGITS[(byte & 0xf) as usize] as char);
    }
    out
}

fn unhex(text: &str, len: usize) -> Option<[u8; 32]> {
    let bytes = unhex_len(text)?;
    if bytes.len() != len {
        return None;
    }
    bytes.try_into().ok()
}

fn unhex_len(text: &str) -> Option<Vec<u8>> {
    if (text.len() & 1) != 0 {
        return None;
    }
    let mut out = Vec::with_capacity(text.len() / 2);
    for pair in text.as_bytes().chunks(2) {
        let hi = (pair[0] as char).to_digit(16)?;
        let lo = (pair[1] as char).to_digit(16)?;
        out.push((hi * 16 + lo) as u8);
    }
    Some(out)
}
