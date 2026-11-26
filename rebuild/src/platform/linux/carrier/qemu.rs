//! QEMU 启动：把目标作为 virtio-blk 交给来宾，串口承载协议帧。
//!
//! 目标以继承的文件描述符进入虚拟机（`-add-fd` + `/dev/fdset`）；宿主已持有的
//! 句柄在会话期间不再写目标。逻辑扇区尺寸只接受 512 或 4096，其他几何在启动前
//! 拒绝，避免来宾看到与宿主不同的块尺寸。

use std::fs::{File, OpenOptions};
use std::os::fd::AsRawFd;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use crate::platform::{PlatformError, PlatformResult};

use super::protocol;

/// 来宾看到的整盘设备。
pub const GUEST_DISK: &str = "/dev/vda";

/// 一台已启动的来宾。
pub struct Guest {
    child: Child,
    stdin: Option<std::process::ChildStdin>,
    stdout: Option<std::io::BufReader<std::process::ChildStdout>>,
    /// 目标描述符必须保持存活；QEMU 经继承的副本访问同一文件。
    _target: File,
}

impl Guest {
    /// 启动来宾。`target` 是宿主上的整盘文件或块设备。
    pub fn start(
        kernel: &Path,
        initramfs: &Path,
        target: &Path,
        sector_size: u64,
        token: &str,
    ) -> PlatformResult<Self> {
        check_sector(sector_size)?;
        let (program, machine, console) = emulator(std::env::consts::ARCH).ok_or_else(|| {
            PlatformError::not_supported(format!(
                "no virtual machine carrier is available for architecture {}",
                std::env::consts::ARCH
            ))
        })?;
        let executable = locate(program)?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(target)
            .map_err(|error| PlatformError::io(target, error))?;
        let fd = file.as_raw_fd();
        let (accel, cpu) = acceleration();
        let args = arguments(
            kernel,
            initramfs,
            sector_size,
            token,
            program,
            machine,
            console,
            fd,
            accel,
            cpu,
        );
        let mut command = Command::new(&executable);
        command
            .args(&args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        // SAFETY: pre_exec 只做 fcntl/prctl 两个 async-signal-safe 调用。
        unsafe {
            command.pre_exec(move || {
                if libc::fcntl(fd, libc::F_SETFD, 0) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
                Ok(())
            });
        }
        let mut child = command.spawn().map_err(|error| {
            PlatformError::not_supported(format!("cannot start {program}: {error}"))
        })?;
        let stdin = child.stdin.take();
        let stdout = child.stdout.take().map(std::io::BufReader::new);
        Ok(Self {
            child,
            stdin,
            stdout,
            _target: file,
        })
    }

    pub fn write(&mut self, frame: &serde_json::Value) -> PlatformResult<()> {
        let stdin = self
            .stdin
            .as_mut()
            .ok_or_else(|| PlatformError::parse("the guest has no protocol input"))?;
        protocol::emit(stdin, frame)
            .map_err(|error| PlatformError::io(Path::new(GUEST_DISK), error))
    }

    pub fn read(&mut self) -> PlatformResult<serde_json::Value> {
        let stdout = self
            .stdout
            .as_mut()
            .ok_or_else(|| PlatformError::parse("the guest has no protocol output"))?;
        protocol::read_frame(stdout)
    }

    pub fn terminate(&mut self) {
        drop(self.child.kill());
        drop(self.child.wait());
    }
}

impl Drop for Guest {
    fn drop(&mut self) {
        self.terminate();
    }
}

/// QEMU 程序名、机器类型与控制台名。
fn emulator(arch: &str) -> Option<(&'static str, &'static str, &'static str)> {
    match arch {
        "x86_64" => Some(("qemu-system-x86_64", "q35", "ttyS0")),
        "aarch64" => Some(("qemu-system-aarch64", "virt", "ttyAMA0")),
        _ => None,
    }
}

/// 在 `PATH` 中查找 QEMU。
fn locate(name: &str) -> PlatformResult<PathBuf> {
    let path = std::env::var_os("PATH").unwrap_or_default();
    for directory in std::env::split_paths(&path) {
        let candidate = directory.join(name);
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    Err(PlatformError::not_supported(format!(
        "cannot find {name} in PATH; install QEMU or point DISKEDIT_VM_BUNDLE at a usable carrier"
    )))
}

/// 有 `/dev/kvm` 就用硬件加速，否则退回纯软件模拟。
fn acceleration() -> (&'static str, &'static str) {
    if Path::new("/dev/kvm").exists() {
        ("kvm", "host")
    } else {
        ("tcg", "max")
    }
}

/// 只接受 512 或 4096 字节逻辑扇区。
fn check_sector(sector: u64) -> PlatformResult<()> {
    if sector == 512 || sector == 4096 {
        Ok(())
    } else {
        Err(PlatformError::not_supported(format!(
            "target logical sector size {sector} is not supported by the virtual machine carrier; \
             only 512 or 4096 is accepted"
        )))
    }
}

// 命令行参数按入口顺序一一列出；拆成结构体只会把同一组参数再抄一遍。
#[allow(clippy::too_many_arguments)]
fn arguments(
    kernel: &Path,
    initramfs: &Path,
    sector: u64,
    token: &str,
    program: &str,
    machine: &str,
    console: &str,
    fd: i32,
    accel: &str,
    cpu: &str,
) -> Vec<String> {
    let mut args: Vec<String> = vec![
        "-nodefaults".into(),
        "-no-user-config".into(),
        "-m".into(),
        "1024".into(),
        "-smp".into(),
        "1".into(),
        "-display".into(),
        "none".into(),
        "-monitor".into(),
        "none".into(),
        "-serial".into(),
        "stdio".into(),
        "-no-reboot".into(),
        "-add-fd".into(),
        format!("fd={fd},set=1"),
        "-blockdev".into(),
        "driver=file,filename=/dev/fdset/1,node-name=target-file,cache.direct=on,aio=threads"
            .into(),
        "-blockdev".into(),
        "driver=raw,file=target-file,node-name=target".into(),
        "-device".into(),
        format!(
            "virtio-blk-pci,drive=target,logical_block_size={sector},physical_block_size={sector}"
        ),
        "-object".into(),
        "rng-random,filename=/dev/urandom,id=rng0".into(),
        "-device".into(),
        "virtio-rng-pci,rng=rng0".into(),
        "-kernel".into(),
        kernel.display().to_string(),
        "-initrd".into(),
        initramfs.display().to_string(),
        "-append".into(),
        format!("console={console},115200 quiet loglevel=3 diskedit.token={token}"),
        "-machine".into(),
        format!("{machine},accel={accel}"),
        "-cpu".into(),
        cpu.into(),
    ];
    if program == "qemu-system-x86_64" {
        args.push("-device".into());
        args.push("isa-debug-exit,iobase=0xf4,iosize=0x04".into());
    }
    args
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(sector: u64) -> Vec<String> {
        arguments(
            Path::new("/bundle/kernel"),
            Path::new("/bundle/initramfs"),
            sector,
            "token",
            "qemu-system-x86_64",
            "q35",
            "ttyS0",
            7,
            "tcg",
            "max",
        )
    }

    // covers: D7
    #[test]
    fn the_target_is_attached_as_virtio_blk_with_the_declared_sector() {
        let text = args(4096).join(" ");
        assert!(text.contains("-add-fd fd=7,set=1"), "{text}");
        assert!(text.contains("cache.direct=on"), "{text}");
        assert!(
            text.contains("logical_block_size=4096,physical_block_size=4096"),
            "{text}"
        );
        assert!(text.contains("-machine q35,accel=tcg"), "{text}");
        assert!(text.contains("diskedit.token=token"), "{text}");
    }

    // covers: D7
    #[test]
    fn an_unsupported_sector_size_is_refused_before_launch() {
        assert!(check_sector(1024).is_err());
        assert!(check_sector(512).is_ok());
        assert!(check_sector(4096).is_ok());
    }

    // covers: D7
    #[test]
    fn an_unknown_architecture_has_no_emulator() {
        assert!(emulator("riscv64").is_none());
    }
}
