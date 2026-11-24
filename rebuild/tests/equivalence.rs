//! 等价对照：同一镜像、同一命令，两个二进制逐项比对。
//!
//! 基线二进制由 `DISKEDIT_BASELINE` 指定；未提供时回报 `NotApplicable`，不静默跳过。
//! 按 D9，只读命令的人读文本与 JSON 字段集不是契约，这里不逐字比对输出；比对的是
//! 退出码、两枚镜像的结构投影（分区起止、区间长度、内容判定），以及 D8 的类型
//! GUID 不被几何变化改写。

use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::Value;

/// 一个对照场景：前置命令与待比对的命令都以占位符写。
struct Scenario {
    name: &'static str,
    arrange: &'static [&'static [&'static str]],
    argv: &'static [&'static str],
    /// 待比对命令是否输出 `info` 形状的 JSON，需要附加顶层事实对照。
    info_shape: bool,
    /// 场景额外需要的现场设置，两个镜像各自施加。
    setup: Setup,
    /// 是否对照分区内容识别结果（基线 `info` 的 `fs` 与本实现 `inspect` 的 `content.value`）。
    identification: bool,
}

/// 前置命令之外的现场设置。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Setup {
    None,
    /// 在第一个分区（从 2048 扇区起）内写入一份 16 MiB 的 swap 签名。
    SwapSignature,
}

const SCENARIOS: &[Scenario] = &[
    Scenario {
        name: "inspect after new and add",
        arrange: &[
            &["new", "{image}", "--yes"],
            &["add", "{image}", "16M", "--yes"],
        ],
        argv: &["inspect", "{image}"],
        info_shape: false,
        setup: Setup::None,
        identification: false,
    },
    Scenario {
        name: "info after new and add",
        arrange: &[
            &["new", "{image}", "--yes"],
            &["add", "{image}", "16M", "--yes"],
        ],
        argv: &["info", "{image}"],
        info_shape: true,
        setup: Setup::None,
        identification: false,
    },
    Scenario {
        name: "new on an empty image",
        arrange: &[],
        argv: &["new", "{image}", "--yes"],
        info_shape: false,
        setup: Setup::None,
        identification: false,
    },
    Scenario {
        name: "add a partition",
        arrange: &[&["new", "{image}", "--yes"]],
        argv: &["add", "{image}", "16M", "--yes"],
        info_shape: false,
        setup: Setup::None,
        identification: false,
    },
    Scenario {
        name: "delete a partition",
        arrange: &[
            &["new", "{image}", "--yes"],
            &["add", "{image}", "16M", "--yes"],
        ],
        argv: &["delete", "{image}:1", "--yes"],
        info_shape: false,
        setup: Setup::None,
        identification: false,
    },
    Scenario {
        name: "D8: the partition type GUID survives a resize",
        arrange: &[
            &["new", "{image}", "--yes"],
            &["add", "{image}", "16M", "--yes"],
        ],
        argv: &["resize", "{image}:1", "32M", "--no-fs", "--yes"],
        info_shape: false,
        setup: Setup::None,
        identification: false,
    },
    Scenario {
        name: "resize grows the filesystem with the partition",
        arrange: &[
            &["new", "{image}", "--yes"],
            &["add", "{image}", "16M", "--yes"],
            &["mkfs", "{image}:1", "ext4", "--yes"],
        ],
        argv: &["resize", "{image}:1", "32M", "--yes"],
        info_shape: false,
        setup: Setup::None,
        identification: false,
    },
    Scenario {
        name: "mkfs ext4 is identified the same way",
        arrange: &[
            &["new", "{image}", "--yes"],
            &["add", "{image}", "16M", "--yes"],
        ],
        argv: &["mkfs", "{image}:1", "ext4", "--yes"],
        info_shape: false,
        setup: Setup::None,
        identification: true,
    },
    Scenario {
        name: "mkfs btrfs is identified the same way",
        arrange: &[
            &["new", "{image}", "--yes"],
            &["add", "{image}", "32M", "--yes"],
        ],
        argv: &["mkfs", "{image}:1", "btrfs", "--yes"],
        info_shape: false,
        setup: Setup::None,
        identification: true,
    },
    Scenario {
        name: "set a GPT partition name",
        arrange: &[
            &["new", "{image}", "--yes"],
            &["add", "{image}", "16M", "--yes"],
        ],
        argv: &["set", "{image}:1", "name", "data", "--yes"],
        info_shape: false,
        setup: Setup::None,
        identification: false,
    },
    Scenario {
        name: "set an ext4 label",
        arrange: &[
            &["new", "{image}", "--yes"],
            &["add", "{image}", "16M", "--yes"],
            &["mkfs", "{image}:1", "ext4", "--yes"],
        ],
        argv: &["set", "{image}:1", "label", "DATA", "--yes"],
        info_shape: false,
        setup: Setup::None,
        identification: true,
    },
    Scenario {
        name: "set an ext4 UUID",
        arrange: &[
            &["new", "{image}", "--yes"],
            &["add", "{image}", "16M", "--yes"],
            &["mkfs", "{image}:1", "ext4", "--yes"],
        ],
        argv: &[
            "set",
            "{image}:1",
            "uuid",
            "0f0e0d0c-0b0a-4908-8706-050403020100",
            "--yes",
        ],
        info_shape: false,
        setup: Setup::None,
        identification: true,
    },
    Scenario {
        name: "set the boot flag",
        arrange: &[
            &["new", "{image}", "--yes"],
            &["add", "{image}", "16M", "--yes"],
        ],
        argv: &["set", "{image}:1", "flag", "boot", "on", "--yes"],
        info_shape: false,
        setup: Setup::None,
        identification: false,
    },
    Scenario {
        name: "set a GPT type GUID",
        arrange: &[
            &["new", "{image}", "--yes"],
            &["add", "{image}", "16M", "--yes"],
        ],
        argv: &[
            "set",
            "{image}:1",
            "type",
            "C12A7328-F81F-11D2-BA4B-00A0C93EC93B",
            "--yes",
        ],
        info_shape: false,
        setup: Setup::None,
        identification: false,
    },
    Scenario {
        name: "shrink an ext4 filesystem with its partition",
        arrange: &[
            &["new", "{image}", "--yes"],
            &["add", "{image}", "16M", "--yes"],
            &["mkfs", "{image}:1", "ext4", "--yes"],
        ],
        argv: &["resize", "{image}:1", "8M", "--yes"],
        info_shape: false,
        setup: Setup::None,
        identification: false,
    },
    Scenario {
        name: "check an ext4 filesystem",
        arrange: &[
            &["new", "{image}", "--yes"],
            &["add", "{image}", "16M", "--yes"],
            &["mkfs", "{image}:1", "ext4", "--yes"],
        ],
        argv: &["check", "{image}:1"],
        info_shape: false,
        setup: Setup::None,
        identification: false,
    },
    Scenario {
        name: "resize by end LBA without moving data",
        arrange: &[
            &["new", "{image}", "--yes"],
            &["add", "{image}", "16M", "--yes"],
        ],
        argv: &["resize", "{image}:1", "--end", "40959", "--no-fs", "--yes"],
        info_shape: false,
        setup: Setup::None,
        identification: false,
    },
    Scenario {
        name: "grow a partition to the end of the target",
        arrange: &[
            &["new", "{image}", "--yes"],
            &["add", "{image}", "16M", "--yes"],
        ],
        argv: &["resize", "{image}:1", "--grow-to-end", "--no-fs", "--yes"],
        info_shape: false,
        setup: Setup::None,
        identification: false,
    },
    Scenario {
        name: "move a partition to the right",
        arrange: &[
            &["new", "{image}", "--yes"],
            &["add", "{image}", "16M", "--yes"],
        ],
        argv: &["move", "{image}:1", "--start", "8192", "--yes"],
        info_shape: false,
        setup: Setup::None,
        identification: false,
    },
    Scenario {
        name: "copy a partition to a new location",
        arrange: &[
            &["new", "{image}", "--yes"],
            &["add", "{image}", "16M", "--yes"],
        ],
        argv: &["copy", "{image}:1", "--start", "65536", "--yes"],
        info_shape: false,
        setup: Setup::None,
        identification: false,
    },
    Scenario {
        name: "identify a swap signature in a partition",
        arrange: &[
            &["new", "{image}", "--yes"],
            &["add", "{image}", "16M", "--yes"],
        ],
        argv: &["info", "{image}"],
        info_shape: true,
        setup: Setup::SwapSignature,
        identification: true,
    },
    Scenario {
        name: "records list on an empty state root",
        arrange: &[],
        argv: &["records", "list"],
        info_shape: false,
        setup: Setup::None,
        identification: false,
    },
    Scenario {
        name: "records verify a missing record",
        arrange: &[],
        argv: &[
            "records",
            "verify",
            "{image}",
            "00000000000000000000000000000000",
        ],
        info_shape: false,
        setup: Setup::None,
        identification: false,
    },
    Scenario {
        name: "records scan a state root",
        arrange: &[],
        argv: &["records", "scan", "{state}"],
        info_shape: false,
        setup: Setup::None,
        identification: false,
    },
    Scenario {
        name: "recover with no record on target",
        arrange: &[],
        argv: &["recover", "{image}", "--yes"],
        info_shape: false,
        setup: Setup::None,
        identification: false,
    },
    Scenario {
        name: "recover --release with no record on target",
        arrange: &[],
        argv: &["recover", "{image}", "--release", "--yes"],
        info_shape: false,
        setup: Setup::None,
        identification: false,
    },
    Scenario {
        name: "recover --replay-table-only with no record on target",
        arrange: &[],
        argv: &["recover", "{image}", "--replay-table-only", "--yes"],
        info_shape: false,
        setup: Setup::None,
        identification: false,
    },
    Scenario {
        name: "help for one command",
        arrange: &[],
        argv: &["help", "set"],
        info_shape: false,
        setup: Setup::None,
        identification: false,
    },
    Scenario {
        name: "help for an unknown command",
        arrange: &[],
        argv: &["help", "nosuch"],
        info_shape: false,
        setup: Setup::None,
        identification: false,
    },
    Scenario {
        name: "toggle the GPT boot flag on",
        arrange: &[
            &["new", "{image}", "--yes"],
            &["add", "{image}", "16M", "--yes"],
        ],
        argv: &["set", "{image}:1", "flag", "boot", "on", "--yes"],
        info_shape: false,
        setup: Setup::None,
        identification: false,
    },
    Scenario {
        name: "toggle the GPT boot flag off",
        arrange: &[
            &["new", "{image}", "--yes"],
            &["add", "{image}", "16M", "--yes"],
            &["set", "{image}:1", "flag", "boot", "on", "--yes"],
        ],
        argv: &["set", "{image}:1", "flag", "boot", "off", "--yes"],
        info_shape: false,
        setup: Setup::None,
        identification: false,
    },
    Scenario {
        name: "set the GPT legacy flag",
        arrange: &[
            &["new", "{image}", "--yes"],
            &["add", "{image}", "16M", "--yes"],
        ],
        argv: &["set", "{image}:1", "flag", "legacy", "on", "--yes"],
        info_shape: false,
        setup: Setup::None,
        identification: false,
    },
    Scenario {
        name: "set the GPT hidden flag",
        arrange: &[
            &["new", "{image}", "--yes"],
            &["add", "{image}", "16M", "--yes"],
        ],
        argv: &["set", "{image}:1", "flag", "hidden", "on", "--yes"],
        info_shape: false,
        setup: Setup::None,
        identification: false,
    },
    Scenario {
        name: "set the GPT required flag",
        arrange: &[
            &["new", "{image}", "--yes"],
            &["add", "{image}", "16M", "--yes"],
        ],
        argv: &["set", "{image}:1", "flag", "required", "on", "--yes"],
        info_shape: false,
        setup: Setup::None,
        identification: false,
    },
    Scenario {
        name: "refuse an unknown GPT flag",
        arrange: &[
            &["new", "{image}", "--yes"],
            &["add", "{image}", "16M", "--yes"],
        ],
        argv: &["set", "{image}:1", "flag", "2", "on", "--yes"],
        info_shape: false,
        setup: Setup::None,
        identification: false,
    },
    Scenario {
        name: "set an MBR type byte",
        arrange: &[
            &["new", "{image}", "--table", "mbr", "--yes"],
            &["add", "{image}", "16M", "--yes"],
        ],
        argv: &["set", "{image}:1", "type", "0x0c", "--yes"],
        info_shape: false,
        setup: Setup::None,
        identification: true,
    },
    Scenario {
        name: "toggle the MBR boot flag",
        arrange: &[
            &["new", "{image}", "--table", "mbr", "--yes"],
            &["add", "{image}", "16M", "--yes"],
        ],
        argv: &["set", "{image}:1", "flag", "boot", "on", "--yes"],
        info_shape: false,
        setup: Setup::None,
        identification: false,
    },
    Scenario {
        name: "hide an MBR FAT partition",
        arrange: &[
            &["new", "{image}", "--table", "mbr", "--yes"],
            &["add", "{image}", "16M", "--yes"],
            &["set", "{image}:1", "type", "0x0c", "--yes"],
        ],
        argv: &["set", "{image}:1", "flag", "hidden", "on", "--yes"],
        info_shape: false,
        setup: Setup::None,
        identification: false,
    },
    Scenario {
        name: "refuse hidden on an MBR partition without a hidden counterpart",
        arrange: &[
            &["new", "{image}", "--table", "mbr", "--yes"],
            &["add", "{image}", "16M", "--yes"],
            &["set", "{image}:1", "type", "0x83", "--yes"],
        ],
        argv: &["set", "{image}:1", "flag", "hidden", "on", "--yes"],
        info_shape: false,
        setup: Setup::None,
        identification: false,
    },
];

#[test]
fn both_binaries_agree_on_the_same_image() {
    let Some(baseline) = std::env::var_os("DISKEDIT_BASELINE") else {
        eprintln!("NotApplicable: 未提供 DISKEDIT_BASELINE，等价对照需要旧二进制");
        return;
    };
    let baseline = PathBuf::from(baseline);
    assert!(
        baseline.is_file(),
        "DISKEDIT_BASELINE 指向的文件不存在：{}",
        baseline.display()
    );
    let new_binary = Path::new(env!("CARGO_BIN_EXE_diskedit"));

    let mut compared = 0;
    for scenario in SCENARIOS {
        compare(scenario, &baseline, new_binary);
        compared += 1;
    }
    assert_eq!(compared, SCENARIOS.len());
}

/// 已挂载分区的在线扩容：新旧二进制对同一挂载点执行，比较退出码、盘上表与文件
/// 系统块数。挂载需要 `CAP_SYS_ADMIN`；沙箱或裁剪过能力的宿主上回报
/// `NotApplicable`，不静默跳过。
#[test]
fn mounted_resizefs_agrees() {
    let Some(baseline) = std::env::var_os("DISKEDIT_BASELINE") else {
        eprintln!("NotApplicable: 未提供 DISKEDIT_BASELINE，等价对照需要旧二进制");
        return;
    };
    let baseline = PathBuf::from(baseline);
    let new_binary = Path::new(env!("CARGO_BIN_EXE_diskedit"));
    let dir = temp_dir("mounted resizefs");
    let old_image = dir.join("old.img");
    let new_image = dir.join("new.img");
    let state_dir = dir.join("state");
    for image in [&old_image, &new_image] {
        File::create(image)
            .and_then(|file| file.set_len(96 * 1024 * 1024))
            .expect("创建镜像失败");
    }
    for (binary, image) in [(baseline.as_path(), &old_image), (new_binary, &new_image)] {
        for argv in [
            &["new", "{image}", "--yes"][..],
            &["add", "{image}", "32M", "--yes"][..],
        ] {
            let output = run(binary, argv, image, &state_dir);
            assert!(
                output.status.success(),
                "前置命令失败：{}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }

    let Some(old_mount) = Mounted::attach(&old_image, &dir) else {
        eprintln!("NotApplicable: 宿主无法挂载 loop 分区，在线等价对照需要 CAP_SYS_ADMIN");
        fs::remove_dir_all(&dir).ok();
        return;
    };
    let Some(new_mount) = Mounted::attach(&new_image, &dir) else {
        drop(old_mount);
        eprintln!("NotApplicable: 宿主无法挂载 loop 分区，在线等价对照需要 CAP_SYS_ADMIN");
        fs::remove_dir_all(&dir).ok();
        return;
    };

    let old = run(
        baseline.as_path(),
        &["resizefs", old_mount.point_str()],
        &old_image,
        &state_dir,
    );
    let new = run(
        new_binary,
        &["resizefs", new_mount.point_str()],
        &new_image,
        &state_dir,
    );
    let old_blocks = old_mount.block_count();
    let new_blocks = new_mount.block_count();
    drop(old_mount);
    drop(new_mount);

    assert_eq!(
        old.status.code(),
        new.status.code(),
        "在线 resizefs 退出码不同（基线 stderr: {}）",
        String::from_utf8_lossy(&old.stderr)
    );
    assert_eq!(
        projection(new_binary, &old_image, &state_dir),
        projection(new_binary, &new_image, &state_dir),
        "在线 resizefs 后两枚镜像的分区表投影不同"
    );
    assert_eq!(
        old_blocks, new_blocks,
        "在线 resizefs 后两个文件系统的块数不同"
    );
    fs::remove_dir_all(&dir).ok();
}

/// 一个已挂载的 loop 分区；析构时卸载并释放 loop 设备。
struct Mounted {
    point: PathBuf,
    partition: String,
    loop_device: String,
}

impl Mounted {
    fn attach(image: &Path, dir: &Path) -> Option<Self> {
        let output = Command::new("losetup")
            .arg("-f")
            .arg("--show")
            .arg("-P")
            .arg(image)
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let device = String::from_utf8(output.stdout).ok()?.trim().to_owned();
        if device.is_empty() {
            return None;
        }
        let partition = format!("{device}p1");
        let mut ready = false;
        for _ in 0..40 {
            if Path::new(&partition).exists() {
                ready = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        let point = dir.join(format!(
            "mnt-{}",
            Path::new(&device)
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("loop")
        ));
        fs::create_dir_all(&point).ok()?;
        if !ready {
            Command::new("losetup").arg("-d").arg(&device).status().ok();
            return None;
        }
        // 分区 32M，文件系统只占 16M，在线扩容才有可扩的余量。
        let format = Command::new("mkfs.ext4")
            .arg("-F")
            .arg(&partition)
            .arg("16M")
            .status()
            .ok()?;
        if !format.success() {
            Command::new("losetup").arg("-d").arg(&device).status().ok();
            return None;
        }
        let mounted = Command::new("mount")
            .arg("-t")
            .arg("ext4")
            .arg(&partition)
            .arg(&point)
            .status()
            .ok()?;
        if !mounted.success() {
            Command::new("losetup").arg("-d").arg(&device).status().ok();
            return None;
        }
        Some(Self {
            point,
            partition,
            loop_device: device,
        })
    }

    fn point_str(&self) -> &str {
        self.point.to_str().expect("挂载点应为 UTF-8")
    }

    fn block_count(&self) -> Option<u64> {
        ext4_block_count(Path::new(&self.partition))
    }
}

impl Drop for Mounted {
    fn drop(&mut self) {
        Command::new("umount").arg(&self.point).status().ok();
        Command::new("losetup")
            .arg("-d")
            .arg(&self.loop_device)
            .status()
            .ok();
    }
}

/// 文件系统的 1KiB 块数；独立 oracle，不依赖两个二进制的输出。
fn ext4_block_count(image: &Path) -> Option<u64> {
    let output = Command::new("dumpe2fs")
        .arg("-h")
        .arg(image)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    for line in text.lines() {
        if let Some(value) = line.strip_prefix("Block count:") {
            return value.trim().parse().ok();
        }
    }
    None
}

fn compare(scenario: &Scenario, baseline: &Path, new_binary: &Path) {
    let dir = temp_dir(scenario.name);
    let old_image = dir.join("old.img");
    let new_image = dir.join("new.img");
    for image in [&old_image, &new_image] {
        File::create(image)
            .and_then(|file| file.set_len(64 * 1024 * 1024))
            .unwrap_or_else(|error| panic!("{}: 创建镜像失败：{error}", scenario.name));
    }
    let state_dir = dir.join("state");
    let binaries: [(&Path, &PathBuf); 2] = [(baseline, &old_image), (new_binary, &new_image)];
    for (binary, image) in &binaries {
        for argv in scenario.arrange {
            let output = run(binary, argv, image, &state_dir);
            assert!(
                output.status.success(),
                "{}: 前置命令失败（{}）\nstderr: {}",
                scenario.name,
                binary.display(),
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }
    if scenario.setup == Setup::SwapSignature {
        for (_, image) in &binaries {
            write_swap_signature(image, &dir);
        }
    }

    let old = run(baseline, scenario.argv, &old_image, &state_dir);
    let new = run(new_binary, scenario.argv, &new_image, &state_dir);
    assert_eq!(
        old.status.code(),
        new.status.code(),
        "{}: 退出码不同（基线 stderr: {}）",
        scenario.name,
        String::from_utf8_lossy(&old.stderr)
    );

    assert_eq!(
        projection(new_binary, &old_image, &state_dir),
        projection(new_binary, &new_image, &state_dir),
        "{}: 两枚镜像的结构投影不同",
        scenario.name
    );

    if scenario.name.starts_with("D8") {
        assert_eq!(
            gpt_type_guid(&old_image),
            gpt_type_guid(&new_image),
            "{}: 分区类型 GUID 被改写",
            scenario.name
        );
    }

    if scenario.info_shape {
        assert_eq!(
            info_facts(&old.stdout),
            info_facts(&new.stdout),
            "{}: info 顶层事实不同",
            scenario.name
        );
    }

    if scenario.identification {
        let baseline_info = run(baseline, &["info", "{image}"], &old_image, &state_dir);
        let new_inspect = run(new_binary, &["inspect", "{image}"], &new_image, &state_dir);
        assert!(
            baseline_info.status.success() && new_inspect.status.success(),
            "{}: 识别对照的只读命令失败",
            scenario.name
        );
        assert_eq!(
            identification_from_info(&baseline_info.stdout),
            identification_from_inspect(&new_inspect.stdout),
            "{}: 分区内容识别结果不同",
            scenario.name
        );
    }
    fs::remove_dir_all(&dir).ok();
}

/// 在镜像的分区 1（2048 扇区起）写入一份 16 MiB 的 swap 签名。
fn write_swap_signature(image: &Path, dir: &Path) {
    use std::io::{Seek, SeekFrom, Write};

    let scratch = dir.join("swap.bin");
    File::create(&scratch)
        .and_then(|file| file.set_len(16 * 1024 * 1024))
        .expect("创建 swap 暂存文件");
    let status = Command::new("mkswap")
        .arg(&scratch)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .expect("运行 mkswap");
    assert!(status.success(), "mkswap 失败");
    let bytes = fs::read(&scratch).expect("读取 swap 暂存文件");
    let mut file = File::options()
        .write(true)
        .open(image)
        .expect("打开镜像写入 swap");
    file.seek(SeekFrom::Start(2048 * 512)).expect("定位分区");
    file.write_all(&bytes).expect("写入 swap 签名");
    file.sync_all().expect("落盘 swap 签名");
    fs::remove_file(&scratch).ok();
}

/// 基线 `info` 的分区内容识别：`partitions[].fs`。
fn identification_from_info(stdout: &[u8]) -> Vec<String> {
    let document: Value = serde_json::from_slice(stdout).expect("基线 info 应为 JSON");
    document["partitions"]
        .as_array()
        .expect("基线 info 应有 partitions 数组")
        .iter()
        .map(|partition| match partition["fs"].as_str() {
            Some(kind) => kind.to_owned(),
            None => "unknown".to_owned(),
        })
        .collect()
}

/// 本实现 `inspect` 的分区内容识别：`partitions[].content.value`。
fn identification_from_inspect(stdout: &[u8]) -> Vec<String> {
    let document: Value = serde_json::from_slice(stdout).expect("inspect 输出应为 JSON");
    document["partitions"]
        .as_array()
        .expect("inspect 应有 partitions 数组")
        .iter()
        .map(|partition| match partition["content"]["value"].as_str() {
            Some(kind) => kind.to_owned(),
            None => "unknown".to_owned(),
        })
        .collect()
}

fn run(binary: &Path, argv: &[&str], image: &Path, state_dir: &Path) -> Output {
    let image = image.to_str().expect("临时路径应为 UTF-8");
    let state = state_dir.to_str().expect("临时路径应为 UTF-8");
    Command::new(binary)
        .env("DISKEDIT_STATE_DIR", state_dir)
        .args(
            argv.iter()
                .map(|token| token.replace("{image}", image).replace("{state}", state)),
        )
        .output()
        .unwrap_or_else(|error| panic!("{} 无法启动：{error}", binary.display()))
}

/// 两枚镜像的结构投影：用本实现的 `inspect` 读取，只保留契约事实，不比对路径与
/// 未知原因的措辞。
fn projection(binary: &Path, image: &Path, state_dir: &Path) -> Value {
    let output = run(binary, &["inspect", "{image}"], image, state_dir);
    assert!(
        output.status.success(),
        "inspect 失败（{}）：{}",
        image.display(),
        String::from_utf8_lossy(&output.stderr)
    );
    let document: Value = serde_json::from_slice(&output.stdout).expect("inspect 输出应为 JSON");
    let partitions: Vec<Value> = document["partitions"]
        .as_array()
        .expect("inspect 应有 partitions 数组")
        .iter()
        .map(|partition| {
            serde_json::json!({
                "number": partition["number"],
                "start_lba": partition["start_lba"],
                "sectors": partition["sectors"],
                "content": partition["content"]["state"],
            })
        })
        .collect();
    Value::Array(partitions)
}

/// `info` 的顶层事实：表类型、扇区、字节数。基线键名 `label`/`size_bytes`，
/// 本实现键名 `table`/`bytes.value`。
fn info_facts(stdout: &[u8]) -> (String, u64, u64) {
    let document: Value = serde_json::from_slice(stdout).expect("info 输出应为 JSON");
    let table = document["label"]
        .as_str()
        .or_else(|| document["table"].as_str())
        .expect("info 应给出表类型")
        .to_owned();
    let sector = document["sector_size"]
        .as_u64()
        .or_else(|| document["sector_size"]["value"].as_u64())
        .expect("info 应给出扇区大小");
    let bytes = document["size_bytes"]
        .as_u64()
        .or_else(|| document["bytes"]["value"].as_u64())
        .expect("info 应给出目标字节数");
    (table, sector, bytes)
}

/// GPT 分区 1 的类型 GUID 原始字节；没有表或没有条目时为 `None`。
fn gpt_type_guid(image: &Path) -> Option<[u8; 16]> {
    let bytes = fs::read(image).ok()?;
    let sector = 512usize;
    let header = bytes.get(sector..sector * 2)?;
    if &header[..8] != b"EFI PART" {
        return None;
    }
    let entry_lba = u64::from_le_bytes(header[72..80].try_into().ok()?) as usize;
    let entry = bytes.get(entry_lba * sector..entry_lba * sector + 16)?;
    entry.try_into().ok()
}

fn temp_dir(name: &str) -> PathBuf {
    let slug: String = name
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '-' })
        .collect();
    let dir = std::env::temp_dir().join(format!("diskedit-equiv-{}-{slug}", std::process::id()));
    fs::remove_dir_all(&dir).ok();
    fs::create_dir_all(&dir).expect("创建临时目录失败");
    dir
}
