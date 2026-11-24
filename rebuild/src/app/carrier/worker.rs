//! 来宾侧工作进程：接收宿主步骤，只写目标，回报结果。
//!
//! 它不是用户入口；只有引导脚本以 `__worker` 调用时才会进入。记录、锁与退出码
//! 都留在宿主，这里不读写记录目录，也不决定放置。

use std::ffi::OsString;
use std::io::{IsTerminal, stdin, stdout};

use serde_json::Value;

use super::protocol::{self, Lines, StepResult};
use crate::app::execute::{self, ExecError};
use crate::platform::{self, PlatformError, TargetHandle};

/// 引导脚本用来标记"已从 init 启动"的环境变量。
const FROM_INIT: &str = "DISKEDIT_WORKER_FROM_INIT";
/// 引导脚本转交的令牌。
const TOKEN: &str = "DISKEDIT_WORKER_TOKEN";

/// 参数是否为工作进程入口。
pub fn invoked(args: &[OsString]) -> bool {
    args.get(1).is_some_and(|arg| arg == "__worker")
}

/// 工作进程主循环；返回退出码。
pub fn run() -> u8 {
    if stdin().is_terminal() && std::env::var_os(FROM_INIT).is_none() {
        eprintln!("__worker 不是用户入口；请经虚拟机载体或引导脚本调用");
        return 2;
    }
    let token = match std::env::var(TOKEN) {
        Ok(value) if !value.is_empty() => value,
        _ => {
            eprintln!("缺少环境变量 {TOKEN}");
            return 3;
        }
    };
    let platform = match platform::open() {
        Ok(platform) => platform,
        Err(error) => {
            eprintln!("工作进程打开平台失败：{error}");
            return 3;
        }
    };
    let mut input = Lines::new(stdin());
    let mut output = stdout();

    if protocol::emit(&mut output, &protocol::boot()).is_err() {
        return 5;
    }

    let hello = match next(&mut input) {
        Ok(Some(frame)) => frame,
        Ok(None) => return 5,
        Err(why) => {
            eprintln!("{why}");
            return 5;
        }
    };
    if hello.get("type").and_then(Value::as_str) != Some("hello")
        || hello.get("v").and_then(Value::as_u64) != Some(protocol::VERSION)
    {
        eprintln!("期待握手帧，收到 {:?}", hello.get("type"));
        return 5;
    }
    if hello.get("token").and_then(Value::as_str) != Some(token.as_str()) {
        eprintln!("令牌不匹配");
        return 3;
    }
    if hello.get("binary_version").and_then(Value::as_str) != Some(env!("CARGO_PKG_VERSION")) {
        eprintln!("二进制版本与宿主不一致");
        return 3;
    }
    let target = match hello.get("target").and_then(Value::as_str) {
        Some(value) => value,
        None => {
            eprintln!("握手帧缺少 target");
            return 5;
        }
    };
    let disk_size = match hello.get("disk_size").and_then(Value::as_u64) {
        Some(value) => value,
        None => return 5,
    };
    let sector = match hello.get("sector_size").and_then(Value::as_u64) {
        Some(value) if value > 0 => value,
        _ => return 5,
    };
    let plan_digest = match hello
        .get("plan_digest")
        .and_then(Value::as_str)
        .and_then(parse_hex32)
    {
        Some(value) => value,
        None => {
            eprintln!("握手帧的计划摘要不可用");
            return 5;
        }
    };
    let mut handle = match platform.open_target(std::path::Path::new(target)) {
        Ok(handle) => handle,
        Err(error) => {
            eprintln!("打开目标 {target} 失败：{error}");
            return 3;
        }
    };
    if let Err(code) = reconcile(&*handle, disk_size, sector) {
        return code;
    }

    let sid = super::token();
    if protocol::emit(
        &mut output,
        &protocol::ready(env!("CARGO_PKG_VERSION"), disk_size, sector, &sid),
    )
    .is_err()
    {
        return 5;
    }

    loop {
        let frame = match next(&mut input) {
            Ok(Some(frame)) => frame,
            Ok(None) => return 5,
            Err(why) => {
                eprintln!("{why}");
                return 5;
            }
        };
        if frame.get("v").and_then(Value::as_u64) != Some(protocol::VERSION)
            || frame.get("sid").and_then(Value::as_str) != Some(sid.as_str())
        {
            eprintln!("帧的版本或会话号不匹配");
            return 5;
        }
        match frame.get("type").and_then(Value::as_str) {
            Some("step") => {
                let id = match frame.get("id").and_then(Value::as_u64) {
                    Some(value) => u32::try_from(value).unwrap_or(u32::MAX),
                    None => return 5,
                };
                let result = match protocol::decode_step(&frame) {
                    Ok((kind, regions)) => {
                        let ctx = execute::steps::Context {
                            plan_digest: &plan_digest,
                            sector,
                        };
                        match execute::apply_delegated(
                            &*platform,
                            std::path::Path::new(target),
                            &mut *handle,
                            &kind,
                            &regions,
                            &ctx,
                        ) {
                            Ok(()) => StepResult {
                                applied: true,
                                class: None,
                                cause: None,
                            },
                            Err(error) => StepResult {
                                applied: false,
                                class: Some(class(&error)),
                                cause: Some(error.to_string()),
                            },
                        }
                    }
                    Err(why) => StepResult {
                        applied: false,
                        class: Some(5),
                        cause: Some(why),
                    },
                };
                if protocol::emit(&mut output, &protocol::result(id, &sid, &result)).is_err() {
                    return 5;
                }
            }
            Some("shutdown") => {
                drop(protocol::emit(&mut output, &protocol::bye(&sid)));
                return 0;
            }
            Some(other) => {
                eprintln!("未知的帧类型 {other}");
                return 5;
            }
            None => return 5,
        }
    }
}

fn reconcile(handle: &dyn TargetHandle, disk_size: u64, sector: u64) -> Result<(), u8> {
    if handle.len() != disk_size {
        eprintln!("目标容量 {} 与宿主声明的 {disk_size} 不一致", handle.len());
        return Err(3);
    }
    if handle.sector_size() != Some(sector) {
        // 普通文件没有设备级扇区，宿主按镜像的逻辑块宽度布局。
        let image = handle.kind() == crate::platform::TargetKind::File
            && sector == crate::app::observe::disk::IMAGE_LOGICAL_BLOCK;
        if !image {
            eprintln!(
                "目标扇区尺寸 {:?} 与宿主声明的 {sector} 不一致",
                handle.sector_size()
            );
            return Err(3);
        }
    }
    Ok(())
}

fn next(input: &mut Lines<std::io::Stdin>) -> Result<Option<Value>, String> {
    input
        .next_frame()
        .map_err(|error| format!("读取宿主帧失败：{error}"))
}

fn class(error: &ExecError) -> u8 {
    match error {
        ExecError::Unsupported(_) => 6,
        ExecError::InvalidPlan(_) | ExecError::Record(_) => 3,
        ExecError::Platform(PlatformError::NotSupported { .. }) => 6,
        ExecError::Platform(_)
        | ExecError::IdentityChanged(_)
        | ExecError::Io(_)
        | ExecError::Postcondition(_)
        | ExecError::Check(_)
        | ExecError::Injected(_) => 5,
    }
}

fn parse_hex32(text: &str) -> Option<[u8; 32]> {
    let bytes = text.as_bytes();
    if bytes.len() != 64 {
        return None;
    }
    let mut digest = [0u8; 32];
    for (index, chunk) in bytes.chunks(2).enumerate() {
        let pair = std::str::from_utf8(chunk).ok()?;
        digest[index] = u8::from_str_radix(pair, 16).ok()?;
    }
    Some(digest)
}
