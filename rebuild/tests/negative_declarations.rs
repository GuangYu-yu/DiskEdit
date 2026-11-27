//! 负向声明行为检查：把 `plan/baseline/negative-declarations.tsv` 里 checker=behavioral
//! 的声明逐条驱动成断言。
//!
//! 前缀分派：`cli:` 查命令面枚举，`probe:` 查内容识别对负样本一律返回 Unknown。
//! 新增一条行为声明只需在 TSV 加一行，本文件按前缀复用现有入口。

use std::path::Path;
use std::sync::{Arc, Mutex};

use diskedit::app::cli::declare::COMMANDS;
use diskedit::platform::probe::identify;
use diskedit::platform::{PlatformError, PlatformResult, TargetHandle, TargetKind};
use diskedit_core::fs::ContentKind;

/// 一条负向声明。
struct Declaration {
    id: String,
    sa: String,
    checker: String,
    target: String,
}

fn declarations() -> Vec<Declaration> {
    let path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("plan/baseline/negative-declarations.tsv");
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("读取 {} 失败: {error}", path.display()));
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(|line| {
            let cols: Vec<&str> = line.split('\t').map(str::trim).collect();
            assert_eq!(cols.len(), 5, "负向声明行应为 5 列：{line}");
            Declaration {
                id: cols[0].to_owned(),
                sa: cols[1].to_owned(),
                checker: cols[3].to_owned(),
                target: cols[4].to_owned(),
            }
        })
        .collect()
}

#[test]
fn every_behavioral_declaration_holds() {
    let rows = declarations();
    assert!(!rows.is_empty(), "负向声明账本为空");
    let names: Vec<&str> = COMMANDS.iter().map(|command| command.name).collect();
    for declaration in rows.iter().filter(|row| row.checker == "behavioral") {
        let label = format!("{}({})", declaration.id, declaration.sa);
        match declaration.target.as_str() {
            "cli:import" => {
                for forbidden in ["import", "load", "sfdisk"] {
                    assert!(
                        !names.iter().any(|name| name.contains(forbidden)),
                        "{label}: 命令面不得出现 {forbidden:?}：{names:?}"
                    );
                }
            }
            "cli:backup-menu" => {
                for forbidden in ["backup", "menu", "restore"] {
                    assert!(
                        !names.iter().any(|name| name.contains(forbidden)),
                        "{label}: 命令面不得出现 {forbidden:?}：{names:?}"
                    );
                }
            }
            "probe:old-label" => {
                // Mac 分区图（'ER' @0）与 Sun disklabel（0xDABE @508）都不在档案表内。
                assert_unknown(&label, &mac_partition_map());
                assert_unknown(&label, &sun_disklabel());
            }
            "probe:unknown-fs" => {
                assert_unknown(&label, &unrecognized());
            }
            other => panic!("{label}: 未知的 behavioral 检查对象 {other:?}"),
        }
    }
}

fn assert_unknown(label: &str, image: &[u8]) {
    let handle = MemHandle::new(image.to_vec());
    let kind = identify(&handle, 0, image.len() as u64)
        .unwrap_or_else(|error| panic!("{label}: identify 失败: {error}"));
    assert_eq!(
        kind,
        ContentKind::Unknown,
        "{label}: 未登记内容必须按未知拒绝，实得 {kind:?}"
    );
}

fn mac_partition_map() -> Vec<u8> {
    let mut bytes = vec![0u8; 1 << 20];
    bytes[0] = b'E';
    bytes[1] = b'R';
    bytes
}

fn sun_disklabel() -> Vec<u8> {
    let mut bytes = vec![0u8; 1 << 20];
    bytes[508] = 0xDA;
    bytes[509] = 0xBE;
    bytes
}

fn unrecognized() -> Vec<u8> {
    vec![0xABu8; 1 << 20]
}

struct MemHandle {
    bytes: Arc<Mutex<Vec<u8>>>,
}

impl MemHandle {
    fn new(bytes: Vec<u8>) -> Self {
        Self {
            bytes: Arc::new(Mutex::new(bytes)),
        }
    }
}

impl TargetHandle for MemHandle {
    fn path(&self) -> &Path {
        Path::new("memory")
    }
    fn len(&self) -> u64 {
        self.bytes.lock().unwrap().len() as u64
    }
    fn kind(&self) -> TargetKind {
        TargetKind::File
    }
    fn sector_size(&self) -> Option<u64> {
        Some(512)
    }
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> PlatformResult<usize> {
        let bytes = self.bytes.lock().unwrap();
        let start = offset as usize;
        if start >= bytes.len() {
            return Ok(0);
        }
        let end = (start + buf.len()).min(bytes.len());
        let len = end - start;
        buf[..len].copy_from_slice(&bytes[start..end]);
        Ok(len)
    }
    fn write_at(&mut self, offset: u64, buf: &[u8]) -> PlatformResult<usize> {
        let mut bytes = self.bytes.lock().unwrap();
        let start = offset as usize;
        let end = start + buf.len();
        if end > bytes.len() {
            return Err(PlatformError::parse("write past the memory image"));
        }
        bytes[start..end].copy_from_slice(buf);
        Ok(buf.len())
    }
    fn barrier(&mut self) -> PlatformResult<()> {
        Ok(())
    }
    fn refresh_partitions(&mut self) -> PlatformResult<()> {
        Ok(())
    }
}
