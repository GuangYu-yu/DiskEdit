#![allow(clippy::let_underscore_must_use)] // 测试的清理步骤有意忽略失败（临时目录/文件）
//! 端到端：gptman 造 GPT 镜像 → 真实二进制 `info` 读回 → JSON 含预期字段。

use std::io::Cursor;
use std::process::Command;

/// 落盘伴随文件名的词表：与 `src/dev.rs` 的同名常量同值。集成测试是独立 crate，
/// 取不到 crate 内的常量，故在这里复写一份——改拼法时两处一起改（src 侧有
/// `dev::tests::artifact_names_are_pinned_as_documented` 会提醒）。
/// 命名空间也只在宏体里写一次；跨 crate 无法共享宏，故宏同样复写一份
#[cfg(unix)]
macro_rules! sidecar_suffix {
    ($stem:literal) => {
        concat!(".diskedit.", $stem)
    };
}

#[cfg(unix)]
const JOURNAL_SUFFIX: &str = sidecar_suffix!("journal");
#[cfg(unix)]
const CHECKPOINT_SUFFIX: &str = sidecar_suffix!("ckpt");
#[cfg(unix)]
const LOCK_SUFFIX: &str = sidecar_suffix!("lock");
/// abandon 的改写名（同 `src/cmd/abandon.rs` 的 ABANDONED_SUFFIX）
#[cfg(unix)]
const ABANDONED_SUFFIX: &str = ".abandoned";

/// 目标旁边的伴随文件：`dir` 下的 `<name><suffix>`
#[cfg(unix)]
fn sidecar(dir: &std::path::Path, name: &str, suffix: &str) -> std::path::PathBuf {
    dir.join(format!("{name}{suffix}"))
}

/// 目录条目名（排序后），不含只增、不参与恢复的审计日志：
/// 拒绝路径不得新增 journal/ckpt 等恢复伴随物
#[cfg(unix)]
fn dir_entries(dir: &std::path::Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| !n.ends_with(".log"))
        .collect();
    names.sort();
    names
}

/// journal 头长 = magic(5) + 身份段(dev/ino/size/设备 ID 哈希各 8 字节 + 有无 loop 映射
/// 1 字节 + offset/sizelimit 各 8 字节 + 身份种类 1 字节 = 50) + 身份段的 CRC32(4)
/// + 磁盘身份凭据段(状态 1 字节 + Disk GUID 16 字节) + 凭据段的 CRC32(4)。
///   与 `src/dev/journal.rs` 的布局同值——集成测试是独立 crate 取不到那里的常量，故与上方
///   后缀词表一样复写一份（src 侧 `dev::tests` 有用例逐字段钉住布局，改格式时这里会一起暴露）
#[cfg(unix)]
const JOURNAL_HEADER_LEN: usize = 5 + 50 + 4 + 17 + 4;
/// 身份段 CRC 的偏移（它只覆盖 magic + 身份段；凭据段另有自己的 CRC）
#[cfg(unix)]
const IDENTITY_CRC_AT: usize = 5 + 50;
/// 身份段里"有无 loop 映射"标志的偏移（dev/ino/size/id 各 8 字节之后）
#[cfg(unix)]
const MAP_FLAG_AT: usize = 5 + 8 * 4;
/// 磁盘身份凭据段起点（状态 1 字节 + GUID 16 字节）
#[cfg(unix)]
const CREDENTIAL_AT: usize = IDENTITY_CRC_AT + 4;
#[cfg(unix)]
const CREDENTIAL_LEN: usize = 17;
/// 记录帧头 `[len u32][crc u32]` 的字节数（同 `src/dev/journal.rs` 的 `REC_HDR`）
#[cfg(unix)]
const REC_HDR: usize = 8;

/// 重算头里身份段的 CRC。头**自带**这个 CRC，故测试改过身份字段（或只造一个头）之后
/// 必须同步重算——否则先报出来的是"头校验失败"，身份核对那条路径就测不到
#[cfg(unix)]
fn resign_header(bytes: &mut [u8]) {
    let crc = crc::Crc::<u32>::new(&crc::CRC_32_ISO_HDLC);
    let mut d = crc.digest();
    d.update(&bytes[..IDENTITY_CRC_AT]);
    bytes[IDENTITY_CRC_AT..IDENTITY_CRC_AT + 4].copy_from_slice(&d.finalize().to_le_bytes());
}

/// 写凭据段并重算本段 CRC。`state` 0 = 不可得、1 = Present（带 GUID）、2 = 本次生成
#[cfg(unix)]
fn write_credential(bytes: &mut [u8], state: u8, guid: [u8; 16]) {
    bytes[CREDENTIAL_AT] = state;
    bytes[CREDENTIAL_AT + 1..CREDENTIAL_AT + 1 + 16].copy_from_slice(&guid);
    let crc = crc::Crc::<u32>::new(&crc::CRC_32_ISO_HDLC);
    let mut d = crc.digest();
    d.update(&bytes[CREDENTIAL_AT..CREDENTIAL_AT + CREDENTIAL_LEN]);
    bytes[CREDENTIAL_AT + CREDENTIAL_LEN..CREDENTIAL_AT + CREDENTIAL_LEN + 4]
        .copy_from_slice(&d.finalize().to_le_bytes());
}

/// 一份**零记录**的 journal：头 + 全零身份段 + 全零凭据段（"没有 GPT"）+ 两段各自的 CRC。
/// 身份种类记为 inode，其余字段全零即"没有可比证据"（身份比对只在两侧都有证据时参与），
/// 故这份壳与任何镜像目标相容。它描述的是"零次写入"——`ensure` 写完头、第一条记录还没
/// 落盘就中断，留下的就是这个形态
#[cfg(unix)]
fn header_only() -> Vec<u8> {
    let mut b = b"DEJL\x07".to_vec();
    b.resize(JOURNAL_HEADER_LEN, 0);
    b[5 + 50 - 1] = 2;
    resign_header(&mut b);
    write_credential(&mut b, 0, [0; 16]);
    b
}

fn fixture_gpt_image(path: &std::path::Path) {
    let ss = 512u64;
    let data = vec![0u8; 100 * ss as usize];
    let mut cur = Cursor::new(data);
    let mut gpt = gptman::GPT::new_from(&mut cur, ss, [0xAB; 16]).unwrap();
    gpt[1] = gptman::GPTPartitionEntry {
        partition_type_guid: [
            0x6D, 0xFD, 0x57, 0x06, 0xAB, 0xA4, 0xC4, 0x43, 0x84, 0xE5, 0x09, 0x33, 0xC8, 0x4B, 0x4F, 0x4F,
        ], // linux swap（混合端磁盘序）
        unique_partition_guid: [0x02; 16],
        starting_lba: 34,
        ending_lba: 60,
        attribute_bits: 0,
        partition_name: "smoke".into(),
    };
    gpt.write_into(&mut cur).unwrap();
    std::fs::write(path, with_protective_mbr(cur.into_inner())).unwrap();
}

/// 补保护 MBR（LBA0 槽位 1 = 0xEE、StartLBA = 1，签名 0x55AA）：
/// gptman 只写 GPT 结构，而 load_gpt 以保护 MBR 为判定前置（UEFI 2.10 §5.2.3）
fn with_protective_mbr(mut data: Vec<u8>) -> Vec<u8> {
    let total = data.len() as u64 / 512;
    let size: u32 = if total > u32::MAX as u64 { u32::MAX } else { (total - 1) as u32 };
    data[446 + 4] = 0xEE;
    data[446 + 8..446 + 12].copy_from_slice(&1u32.to_le_bytes());
    data[446 + 12..446 + 16].copy_from_slice(&size.to_le_bytes());
    data[510] = 0x55;
    data[511] = 0xAA;
    data
}

/// 崩溃恢复。注入点只在 `test-faults` 构建下存在，故整块按 feature 隔离：
/// **默认 `cargo test` 不会跑这里**，要跑得显式 `cargo test --features test-faults`。
///
/// 这两条补的是"共用执行入口"覆盖不到的那一半：`move` 已被 Linux 冒烟验过，
/// 而 `resize-part` / `copy` 各有自己的收尾形状——一个留下可续跑的 ckpt，
/// 一个什么都不留、且已越过不可回滚点。断言必须落在它们**各自的**后效上
#[cfg(all(unix, feature = "test-faults"))]
mod crash_recovery;

#[cfg(not(target_os = "linux"))]
mod platform_gate;
// 写入型用例的目标平台是 Linux：本条命令在不承诺安全写盘的平台上由统一能力门
// 直接拒绝（见 src/cmd/mod.rs），这些用例在那里没有可观察的成功路径
#[cfg(unix)]
mod commands;
#[cfg(unix)]
mod confirm_gate;
mod guards;
mod info;
#[cfg(unix)]
mod journal;
#[cfg(unix)]
mod resize;
#[cfg(unix)]
mod table;