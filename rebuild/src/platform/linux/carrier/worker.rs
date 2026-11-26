//! 来宾工作进程：接收宿主的在线扩容请求，只写目标，回报几何。
//!
//! 它不是用户入口；只有引导脚本以 `__worker` 调用时才会进入。记录、锁与退出码
//! 都留在宿主，这里不读写记录目录，也不决定放置。

use std::ffi::OsString;
use std::io::{BufReader, IsTerminal, stdin, stdout};
use std::path::Path;

use diskedit_core::fs::{FsKind, Program};
use serde_json::Value;

use crate::platform::{Platform, host};

use super::protocol::{self, Reply, Request};
use super::qemu::GUEST_DISK;

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
        eprintln!("__worker is not a user entry point; boot it through the VM carrier");
        return 2;
    }
    let token = match std::env::var(TOKEN) {
        Ok(value) if !value.is_empty() => value,
        _ => {
            eprintln!("missing environment variable {TOKEN}");
            return 3;
        }
    };
    let platform = host();
    let mut input = BufReader::new(stdin());
    let mut output = stdout();

    if protocol::emit(&mut output, &protocol::boot()).is_err() {
        return 5;
    }
    let hello = match protocol::read_frame(&mut input) {
        Ok(frame) if frame.get("type").and_then(Value::as_str) == Some("hello") => frame,
        Ok(frame) => {
            eprintln!("expected a hello frame, got {:?}", frame.get("type"));
            return 5;
        }
        Err(error) => {
            eprintln!("{error}");
            return 5;
        }
    };
    if hello.get("token").and_then(Value::as_str) != Some(token.as_str()) {
        eprintln!("token mismatch");
        return 3;
    }
    if hello.get("binary_version").and_then(Value::as_str) != Some(env!("CARGO_PKG_VERSION")) {
        eprintln!("binary version does not match the host");
        return 3;
    }
    let disk_size = match hello.get("disk_size").and_then(Value::as_u64) {
        Some(value) => value,
        None => return 5,
    };
    let sector = match hello.get("sector_size").and_then(Value::as_u64) {
        Some(value) if value > 0 => value,
        _ => return 5,
    };
    let handle = match platform.open_target(Path::new(GUEST_DISK)) {
        Ok(handle) => handle,
        Err(error) => {
            eprintln!("cannot open {GUEST_DISK}: {error}");
            return 3;
        }
    };
    if handle.len() != disk_size || handle.sector_size() != Some(sector) {
        eprintln!(
            "the guest target geometry ({} bytes, {:?}-byte sectors) differs from the host \
             declaration ({disk_size} bytes, {sector}-byte sectors)",
            handle.len(),
            handle.sector_size()
        );
        return 3;
    }

    let sid = session_id();
    if protocol::emit(
        &mut output,
        &protocol::ready(env!("CARGO_PKG_VERSION"), disk_size, sector, &sid),
    )
    .is_err()
    {
        return 5;
    }

    loop {
        let frame = match protocol::read_frame(&mut input) {
            Ok(frame) => frame,
            Err(error) => {
                eprintln!("{error}");
                return 5;
            }
        };
        if frame.get("sid").and_then(Value::as_str) != Some(sid.as_str()) {
            eprintln!("frame session id mismatch");
            return 5;
        }
        match frame.get("type").and_then(Value::as_str) {
            Some("online_grow") => {
                let reply = match protocol::decode_request(&frame, &sid) {
                    Ok(request) => grow(&platform, &request),
                    Err(error) => Reply::Failed(error.to_string()),
                };
                let id = frame.get("id").and_then(Value::as_u64).unwrap_or(0);
                if protocol::emit(&mut output, &protocol::result(id, &sid, &reply)).is_err() {
                    return 5;
                }
            }
            Some("shutdown") => {
                drop(protocol::emit(&mut output, &protocol::bye(&sid)));
                return 0;
            }
            Some(other) => {
                eprintln!("unknown frame type {other}");
                return 5;
            }
            None => return 5,
        }
    }
}

/// 在来宾里把分区上的文件系统撑满分区；先挂载，工具运行后回读，再卸载。
fn grow(platform: &dyn Platform, request: &Request) -> Reply {
    let device = match platform.partition_device(Path::new(GUEST_DISK), request.partition) {
        Ok(device) => device,
        Err(error) => return Reply::Failed(error.to_string()),
    };
    let mount = match platform.temporary_directory("online") {
        Ok(path) => path,
        Err(error) => return Reply::Failed(error.to_string()),
    };
    let mounted = match platform.run(
        Program::Mount,
        &[device.display().to_string(), mount.display().to_string()],
    ) {
        Ok(run) if run.code == Some(0) => run,
        Ok(run) => {
            platform.remove_directory(&mount).ok();
            return Reply::Failed(format!(
                "mount exited {}: {}",
                run.code
                    .map_or("by a signal".to_owned(), |code| code.to_string()),
                run.stderr.trim()
            ));
        }
        Err(error) => {
            platform.remove_directory(&mount).ok();
            return Reply::Failed(error.to_string());
        }
    };
    drop(mounted);

    let mount_text = mount.display().to_string();
    let device_text = device.display().to_string();
    let invocation = match request.filesystem {
        FsKind::Ext => Some((Program::Resize2fs, vec![device_text.clone()])),
        FsKind::Xfs => Some((Program::XfsGrowfs, vec![mount_text.clone()])),
        FsKind::Btrfs => Some((
            Program::Btrfs,
            vec![
                "filesystem".to_owned(),
                "resize".to_owned(),
                "max".to_owned(),
                mount_text.clone(),
            ],
        )),
        FsKind::Squashfs
        | FsKind::Erofs
        | FsKind::F2fs
        | FsKind::Exfat
        | FsKind::Vfat
        | FsKind::Ntfs
        | FsKind::HfsPlus
        | FsKind::Apfs
        | FsKind::Bcachefs
        | FsKind::Jfs
        | FsKind::Reiserfs
        | FsKind::Udf => None,
    };
    let reply = match invocation {
        None => Reply::Failed(format!(
            "{} has no online grow program",
            request.filesystem.as_str()
        )),
        Some((program, args)) => match platform.run(program, &args) {
            Ok(run) if run.code == Some(0) => {
                if request.filesystem == FsKind::Ext {
                    match read_ext_geometry(platform, &device_text, request.bytes) {
                        Ok(report) => Reply::Applied(report),
                        Err(cause) => Reply::Failed(cause),
                    }
                } else {
                    Reply::Applied(crate::platform::OnlineReport {
                        blocks: None,
                        block_size: None,
                    })
                }
            }
            Ok(run) => Reply::Failed(format!(
                "{} exited {}: {}",
                platform.tool_name(program),
                run.code
                    .map_or("by a signal".to_owned(), |code| code.to_string()),
                run.stderr.trim()
            )),
            Err(error) => Reply::Failed(error.to_string()),
        },
    };

    match platform.run(Program::Umount, &[mount_text]) {
        Ok(run) if run.code == Some(0) => {
            platform.remove_directory(&mount).ok();
        }
        Ok(run) => {
            return Reply::Failed(format!(
                "umount exited {}: {}",
                run.code
                    .map_or("by a signal".to_owned(), |code| code.to_string()),
                run.stderr.trim()
            ));
        }
        Err(error) => return Reply::Failed(error.to_string()),
    }
    reply
}

/// 回读 ext 的块几何；没长到目标字节数时按失败报告。
fn read_ext_geometry(
    platform: &dyn Platform,
    device: &str,
    expected: u64,
) -> Result<crate::platform::OnlineReport, String> {
    let run = platform
        .run(Program::Dumpe2fs, &["-h".to_owned(), device.to_owned()])
        .map_err(|error| error.to_string())?;
    if run.code != Some(0) {
        return Err(format!(
            "dumpe2fs exited {}: {}",
            run.code
                .map_or("by a signal".to_owned(), |code| code.to_string()),
            run.stderr.trim()
        ));
    }
    let (blocks, block_size) = ext_geometry(&run.stdout)
        .ok_or_else(|| "dumpe2fs did not report the filesystem geometry".to_owned())?;
    let total = blocks
        .checked_mul(block_size)
        .ok_or_else(|| "the filesystem geometry overflows".to_owned())?;
    if total > expected || expected - total >= block_size {
        return Err(format!(
            "the filesystem is {total} bytes, not the expected {expected} bytes rounded to its \
             {block_size}-byte blocks"
        ));
    }
    Ok(crate::platform::OnlineReport {
        blocks: Some(blocks),
        block_size: Some(block_size),
    })
}

fn ext_geometry(stdout: &str) -> Option<(u64, u64)> {
    let mut count = None;
    let mut size = None;
    for line in stdout.lines() {
        if let Some(rest) = line.strip_prefix("Block count:") {
            count = rest
                .split_whitespace()
                .next()
                .and_then(|text| text.parse().ok());
        } else if let Some(rest) = line.strip_prefix("Block size:") {
            size = rest
                .split_whitespace()
                .next()
                .and_then(|text| text.parse().ok());
        }
    }
    count.zip(size)
}

/// 会话号：只用来发现宿主与来宾接错通道。
fn session_id() -> String {
    use std::hash::{BuildHasher, Hasher};
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_nanos())
        .unwrap_or(0);
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    hasher.write_u128(nanos);
    format!("{:016x}", hasher.finish())
}
