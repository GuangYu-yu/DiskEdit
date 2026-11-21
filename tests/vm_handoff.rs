//! fd 交棒：宿主核对过目标之后，路径被换掉也不改变写入的落点。
//!
//! 判据是端到端的：QEMU 一旦出现，说明宿主已经把已打开的句柄交给了它
//! （`-add-fd fd=N` + `/dev/fdset/1`），此后的路径变化与写入无关。因此本用例在
//! "QEMU 已出现"之后才动手替换路径——那是确定的事件边界，不是 sleep 猜的窗口。
//!
//! 作业必须带 FS 步（`mkfs`）：纯表与纯搬移在宿主永远做得成，放置恒为 Native，
//! 测不到载体。跑这套用例的二进制要用 `DISKEDIT_TOOL_PATH=/nonexistent` 编译，宿主
//! 因此没有 `mke2fs`，这一步只能交给载体（命令见 `tools/vm/README.md`）。
//!
//! 需要 QEMU 与真实 bundle，默认不进常规流水线：
//!
//! ```text
//! DISKEDIT_VM_TESTS=1 DISKEDIT_VM_BUNDLE=/path/to/bundle \
//! cargo test --test vm_handoff -- --test-threads=1
//! ```
//!
//! 不需要 QEMU 的两半事实各自有更靠近产生点的判据，本文件不重复："写入仍落在已打开的
//! inode" 见 `src/dev/tests.rs::a_swapped_path_does_not_redirect_writes`；"交给 QEMU 的是
//! 宿主核对过的那个描述符" 见 `src/exec/qemu.rs::tests::no_unsafe_cache_and_exit_device`；
//! "路径换了人就不再用它派生身份" 见
//! `src/dev/tests.rs::image_identity_follows_the_opened_handle_not_the_path`。

#![cfg(target_os = "linux")]

mod common;

use std::path::PathBuf;
use std::time::Duration;

const IMAGE_BYTES: u64 = 32 * 1024 * 1024;
const SECTOR_BYTES: u64 = 512;

/// ext4 超级块在分区内的偏移与它的魔数（`0x53EF`，小端）
const EXT4_MAGIC_OFFSET: usize = 0x438;
const EXT4_MAGIC: [u8; 2] = [0x53, 0xEF];

struct Fixture {
    dir: PathBuf,
    image: PathBuf,
    state: PathBuf,
    first_lba: u64,
}

impl Fixture {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("diskedit_handoff_{tag}_{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        common::private_dir(&dir);
        let state = dir.join("state");
        common::private_dir(&state);
        let image = dir.join("disk.img");
        std::fs::write(&image, vec![0u8; IMAGE_BYTES as usize]).expect("create the image");

        let p = image.to_str().expect("utf-8 path");
        for layout in [
            vec!["new".to_string(), p.to_string(), "--table".into(), "gpt".into(), "--yes".into()],
            vec!["add".to_string(), p.to_string(), "8M".into(), "--yes".into()],
        ] {
            let args: Vec<&str> = layout.iter().map(String::as_str).collect();
            let (code, out, err) = common::run(&state, &args);
            assert_eq!(code, 0, "{args:?} must succeed: {out}\n{err}");
        }

        let (code, out, err) = common::run(&state, &["info", p]);
        assert_eq!(code, 0, "info must succeed: {err}");
        let json: serde_json::Value = serde_json::from_str(&out).expect("info emits one JSON object");
        let first_lba = json["partitions"][0]["first_lba"].as_u64().expect("first_lba");

        Self { dir, image, state, first_lba }
    }

    fn mkfs_args(&self) -> Vec<String> {
        vec!["mkfs".into(), format!("{}:1", self.image.display()), "ext4".into(), "--yes".into()]
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.dir).ok();
    }
}

/// 宿主核对之后、QEMU 启动之前路径被替换：写入仍落在原文件
#[test]
fn replaced_path_still_writes_original() {
    if !common::gate("vm_handoff", "replaced_path_still_writes_original") {
        return;
    }
    let f = Fixture::new("swap");
    // 输出落文件：`-serial stdio` 把 guest 串口汇进 CLI 的 stdout，管道不读会把它卡住
    let out_log = std::fs::File::create(f.dir.join("run.stdout")).expect("create the log");
    let err_log = std::fs::File::create(f.dir.join("run.stderr")).expect("create the log");
    let mut child = common::disk_edit()
        .args(f.mkfs_args())
        .env("DISKEDIT_STATE_DIR", &f.state)
        .stdout(std::process::Stdio::from(out_log))
        .stderr(std::process::Stdio::from(err_log))
        .spawn()
        .expect("spawn DiskEdit");
    let pid = child.id();

    // QEMU 出现 = 描述符已经交出去了；这一刻之后再换路径才不落在竞态窗口里
    assert!(
        common::wait_until(Duration::from_secs(120), || common::qemu_child_of(pid).is_some()),
        "the VM session never started"
    );

    let moved = f.dir.join("disk-moved.img");
    std::fs::rename(&f.image, &moved).expect("swap the path away from the opened file");
    // 原路径放一份等长的空文件：若宿主重开路径，写入会落在这里
    std::fs::write(&f.image, vec![0u8; IMAGE_BYTES as usize]).expect("place the decoy");

    let status = child.wait().expect("the run ends");
    let stderr_log = std::fs::read_to_string(f.dir.join("run.stderr")).unwrap_or_default();
    assert_eq!(
        status.code().unwrap_or(-1),
        0,
        "the run must finish against the opened file: {stderr_log}"
    );

    // 原文件里拿到了 ext4 超级块
    let bytes = std::fs::read(&moved).unwrap();
    let magic_at = f.first_lba as usize * SECTOR_BYTES as usize + EXT4_MAGIC_OFFSET;
    assert_eq!(
        bytes[magic_at..magic_at + EXT4_MAGIC.len()],
        EXT4_MAGIC,
        "the filesystem was not written into the file that was opened"
    );

    // 路径上的新文件一个字节都没动
    assert_eq!(
        std::fs::read(&f.image).unwrap(),
        vec![0u8; IMAGE_BYTES as usize],
        "the decoy at the old path was written"
    );
}
