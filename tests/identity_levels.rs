#![allow(clippy::let_underscore_must_use)] // 测试的清理步骤有意忽略失败（临时目录 / 进程 / 设备）
//! 身份强度分级对命令的影响。
//!
//! 镜像的 `(dev, ino)`、loop 的后备文件身份、分区的载体身份列为 Strong，
//! dm/md 等只给主次设备号的设备列为 Unusable：读命令照常，写命令必须被拒。
//!
//! 需要 Linux：块设备探测与记录后端只在 Linux 编译。root、loop 设备、dm 控制节点
//! 都在运行期探测，条件不满足时提前返回；对真实磁盘只做只读 `info`，绝不写盘。

#![cfg(target_os = "linux")]

use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn run(args: &[&str]) -> (i32, String, String) {
    run_with_env(args, &[])
}

fn run_with_env(args: &[&str], envs: &[(&str, &Path)]) -> (i32, String, String) {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_DiskEdit"));
    cmd.args(args);
    for (key, value) in envs {
        cmd.env(key, value);
    }
    let out = cmd.output().expect("spawn DiskEdit");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("diskedit_identity_{tag}_{}", std::process::id()));
    fs::remove_dir_all(&dir).ok();
    fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

fn image_with(dir: &Path, size: u64) -> PathBuf {
    let image = dir.join("disk.img");
    fs::write(&image, vec![0u8; size as usize]).expect("create image");
    image
}

fn is_root() -> bool {
    unsafe { libc::geteuid() == 0 }
}

fn attach_loop(image: &Path, partscan: bool) -> Option<String> {
    let mut cmd = Command::new("losetup");
    if partscan {
        cmd.arg("-P");
    }
    let out = cmd.args(["--find", "--show"]).arg(image).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let device = String::from_utf8(out.stdout).ok()?.trim().to_string();
    (!device.is_empty()).then_some(device)
}

fn detach_loop(device: &str) {
    let _ = Command::new("losetup").args(["--detach", device]).status();
}

fn snapshot(path: &Path) -> Vec<u8> {
    fs::read(path).expect("read target snapshot")
}

#[test]
fn an_image_identity_permits_the_write_path() {
    // 镜像的 (dev, ino) 是 Strong，写命令照常执行。
    let dir = temp_dir("image");
    let image = image_with(&dir, 8 * 1024 * 1024);
    let path = image.to_str().expect("utf-8 path");

    let (code, _, err) = run(&["new", path, "--yes"]);
    assert_eq!(code, 0, "{err}");
    let (code, _, err) = run(&["add", path, "1M", "--yes"]);
    assert_eq!(code, 0, "{err}");

    let (code, out, err) = run(&["info", path]);
    assert_eq!(code, 0, "{err}");
    let json: serde_json::Value = serde_json::from_str(&out).expect("info JSON");
    assert_eq!(json["partitions"].as_array().expect("partition array").len(), 1, "{out}");
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_loop_backing_identity_permits_the_write_path() {
    // loop 的后备文件身份是 Strong。写盘落在后备文件上。
    // 持久状态已达成但内核拒绝重扫 → 5，不算身份拒绝。
    let dir = temp_dir("loop");
    let image = image_with(&dir, 32 * 1024 * 1024);
    let Some(device) = attach_loop(&image, false) else {
        eprintln!("skipping a_loop_backing_identity_permits_the_write_path: losetup could not attach the image");
        fs::remove_dir_all(&dir).ok();
        return;
    };

    let (code, out, err) = run(&["new", &device, "--yes"]);
    detach_loop(&device);
    assert!(
        code == 0 || code == 5,
        "a loop target must pass the identity gate, got {code}: {out}\n{err}"
    );

    let mut file = fs::File::open(&image).expect("open backing image");
    file.seek(SeekFrom::Start(512)).expect("seek to header");
    let mut signature = [0u8; 8];
    file.read_exact(&mut signature).expect("read header");
    assert_eq!(&signature, b"EFI PART", "the table must land on the backing file");
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_partition_node_reads_with_the_carrier_identity() {
    // 分区身份 = 载体 + 分区号；读命令可用。
    let dir = temp_dir("partition");
    let image = image_with(&dir, 32 * 1024 * 1024);
    let path = image.to_str().expect("utf-8 path");
    assert_eq!(run(&["new", path, "--yes"]).0, 0);
    assert_eq!(run(&["add", path, "4M", "--yes"]).0, 0);

    let Some(device) = attach_loop(&image, true) else {
        eprintln!("skipping a_partition_node_reads_with_the_carrier_identity: losetup could not attach the image");
        fs::remove_dir_all(&dir).ok();
        return;
    };
    let partition = format!("{device}p1");
    if !Path::new(&partition).exists() {
        eprintln!("skipping a_partition_node_reads_with_the_carrier_identity: the partition node {partition} did not appear");
        detach_loop(&device);
        fs::remove_dir_all(&dir).ok();
        return;
    }

    let (code, out, err) = run(&["info", &partition]);
    detach_loop(&device);
    assert_eq!(code, 0, "{out}\n{err}");
    let json: serde_json::Value = serde_json::from_str(&out).expect("info JSON");
    assert_eq!(json["size_bytes"], 4 * 1024 * 1024, "{out}");
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_dm_device_is_unusable_for_writes() {
    // dm 只有主次设备号，身份为 Unusable → 写被拒。
    // 身份不可得不阻断读命令。
    if !is_root() {
        eprintln!("skipping a_dm_device_is_unusable_for_writes: needs euid 0 to create the dm device");
        return;
    }
    if !Path::new("/dev/mapper/control").exists() {
        eprintln!("skipping a_dm_device_is_unusable_for_writes: needs /dev/mapper/control");
        return;
    }
    let dir = temp_dir("dm");
    let state = dir.join("state");
    fs::create_dir_all(&state).expect("create state dir");
    let image = image_with(&dir, 32 * 1024 * 1024);
    let Some(device) = attach_loop(&image, false) else {
        eprintln!("skipping a_dm_device_is_unusable_for_writes: losetup could not attach the image");
        fs::remove_dir_all(&dir).ok();
        return;
    };
    let sectors = Command::new("blockdev")
        .args(["--getsz", &device])
        .output()
        .ok()
        .and_then(|out| String::from_utf8(out.stdout).ok())
        .map(|text| text.trim().to_string());
    let Some(sectors) = sectors else {
        eprintln!("skipping a_dm_device_is_unusable_for_writes: blockdev --getsz gave no size for {device}");
        detach_loop(&device);
        fs::remove_dir_all(&dir).ok();
        return;
    };
    let name = format!("diskedit_id_{}", std::process::id());
    let table = format!("0 {sectors} linear {device} 0");
    let created = Command::new("dmsetup")
        .args(["create", &name, "--table", &table])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    if !matches!(created, Ok(status) if status.success()) {
        eprintln!("skipping a_dm_device_is_unusable_for_writes: dmsetup could not create the device");
        detach_loop(&device);
        fs::remove_dir_all(&dir).ok();
        return;
    }
    let dm = format!("/dev/mapper/{name}");
    let before = snapshot(&image);

    let (read_code, read_out, read_err) = run(&["info", &dm]);
    let (write_code, write_out, write_err) =
        run_with_env(&["add", &dm, "1M", "--yes"], &[("DISKEDIT_STATE_DIR", &state)]);

    let _ = Command::new("dmsetup").args(["remove", &name]).status();
    detach_loop(&device);

    assert_eq!(read_code, 0, "reads must not be gated by identity: {read_out}\n{read_err}");
    // 6 包含"运行期探测失败"与"身份不可证明"；dm 只有主次设备号，身份
    // 无法证明，重试或换参数都不会改变这一点，故必须是 6 而不是 3
    assert_eq!(
        write_code, 6,
        "a dm write must exit 6 (identity unprovable), got {write_code}: {write_out}\n{write_err}"
    );
    assert!(
        write_err.contains("stable identity") || write_out.contains("stable identity"),
        "{write_out}\n{write_err}"
    );
    assert_eq!(snapshot(&image), before, "a refused write must not touch the target");
    assert!(!state.join("recovery").exists(), "a refused write must not create a record directory");
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_device_with_a_serial_or_wwid_reads_as_a_strong_identity() {
    // 有 WWN/EUI 或一致 serial 的真实盘是 Strong。
    // 只发只读命令；测试不写真实磁盘。
    let Some(device) = device_with_serial_or_wwid() else {
        eprintln!("skipping a_device_with_a_serial_or_wwid_reads_as_a_strong_identity: no device with a WWN or serial");
        return;
    };
    // 读命令的前提是设备打得开：无读权限的环境没有这个现场，跳过
    if fs::File::open(&device).is_err() {
        eprintln!(
            "skipping a_device_with_a_serial_or_wwid_reads_as_a_strong_identity: cannot open {}",
            device.display()
        );
        return;
    }
    let (code, out, err) = run(&["info", device.to_str().expect("utf-8 device path")]);
    assert_eq!(code, 0, "{}: {out}\n{err}", device.display());
}

fn device_with_serial_or_wwid() -> Option<PathBuf> {
    for entry in fs::read_dir("/sys/block").ok()? {
        let entry = entry.ok()?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with("loop") || name.starts_with("ram") || name.starts_with("dm-") {
            continue;
        }
        let base = Path::new("/sys/block").join(&name);
        let has = |file: &str| {
            fs::read_to_string(base.join(file))
                .ok()
                .map(|text| !text.trim().is_empty())
                .unwrap_or(false)
        };
        if !has("device/wwid") && !has("device/serial") {
            continue;
        }
        let device = Path::new("/dev").join(&name);
        if device.exists() {
            return Some(device);
        }
    }
    None
}
