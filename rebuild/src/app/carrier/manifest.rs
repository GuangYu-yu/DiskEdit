//! 虚拟机载体的清单校验。
//!
//! 清单是一个 JSON 文件，声明架构、二进制版本，以及内核与 initramfs 的相对
//! 路径和摘要。只接受与当前进程同架构、同版本、摘要吻合且不越出包目录的条目；
//! 任何一项不符都在写入之前拒绝。

use std::fs;
use std::io::Read;
use std::path::{Component, Path, PathBuf};

use serde_json::Value;
use sha2::{Digest, Sha256};

/// 环境变量：包目录。
pub const VARIABLE: &str = "DISKEDIT_VM_BUNDLE";
/// 清单格式版本。
const SCHEMA: u64 = 1;

/// 通过校验的包。
pub struct Bundle {
    pub kernel: PathBuf,
    pub initramfs: PathBuf,
    /// 清单声明的 worker 摘要；省略表示不核对。
    pub worker: Option<[u8; 32]>,
}

/// 读取并校验包；未配置时返回 `None`。
pub fn resolve() -> Result<Option<Bundle>, String> {
    let root = match std::env::var_os(VARIABLE) {
        Some(value) if value.is_empty() => {
            return Err(format!("{VARIABLE}= is empty"));
        }
        Some(value) => PathBuf::from(value),
        None => return Ok(None),
    };
    let manifest = root.join("manifest.json");
    let text = fs::read_to_string(&manifest)
        .map_err(|error| format!("failed to read {}: {error}", manifest.display()))?;
    let value: Value = serde_json::from_str(&text)
        .map_err(|error| format!("failed to parse {}: {error}", manifest.display()))?;

    let schema = value.get("schema").and_then(Value::as_u64);
    if schema != Some(SCHEMA) {
        return Err(format!("schema of {} must be {SCHEMA}", manifest.display()));
    }
    let arch = field(&value, "arch")?;
    if arch != std::env::consts::ARCH {
        return Err(format!(
            "bundle arch {arch} does not match host {}",
            std::env::consts::ARCH
        ));
    }
    let version = field(&value, "binary_version")?;
    if version != env!("CARGO_PKG_VERSION") {
        return Err(format!(
            "bundle binary version {version} does not match host {}",
            env!("CARGO_PKG_VERSION")
        ));
    }

    let canonical_root = fs::canonicalize(&root).map_err(|error| {
        format!(
            "failed to resolve bundle directory {}: {error}",
            root.display()
        )
    })?;
    let kernel = artifact(&value, "kernel", &canonical_root)?;
    let initramfs = artifact(&value, "initramfs", &canonical_root)?;
    let worker = match value.get("worker") {
        None => None,
        Some(entry) => {
            let digest = hex32(field(entry, "sha256")?, "worker.sha256")?;
            let current = std::env::current_exe()
                .map_err(|error| format!("cannot locate current executable: {error}"))?;
            let actual = digest_file(&current)?;
            if digest != actual {
                return Err(format!(
                    "bundle worker digest does not match current executable: declared {}, actual {}",
                    show(&digest),
                    show(&actual)
                ));
            }
            Some(digest)
        }
    };
    Ok(Some(Bundle {
        kernel,
        initramfs,
        worker,
    }))
}

/// 校验一项产物：路径必须是包内的纯相对路径，摘要必须与文件内容一致。
fn artifact(value: &Value, name: &str, root: &Path) -> Result<PathBuf, String> {
    let entry = value
        .get(name)
        .ok_or_else(|| format!("manifest is missing {name}"))?;
    let relative = field(entry, "path")?;
    let path = inside(root, relative, name)?;
    let declared = hex32(field(entry, "sha256")?, &format!("{name}.sha256"))?;
    let actual = digest_file(&path)?;
    if declared != actual {
        return Err(format!(
            "digest mismatch for {name}: declared {}, actual {}",
            show(&declared),
            show(&actual)
        ));
    }
    Ok(path)
}

/// 把纯相对路径解析到包目录内；绝对路径、`.`、`..` 都拒绝。
fn inside(root: &Path, relative: &str, name: &str) -> Result<PathBuf, String> {
    let path = Path::new(relative);
    let mut normal = false;
    for component in path.components() {
        match component {
            Component::Normal(_) => normal = true,
            Component::CurDir
            | Component::ParentDir
            | Component::RootDir
            | Component::Prefix(_) => {
                return Err(format!(
                    "{name}.path must be a plain relative path inside the bundle: {relative}"
                ));
            }
        }
    }
    if !normal {
        return Err(format!("{name}.path must not be empty"));
    }
    let joined = root.join(path);
    let canonical = fs::canonicalize(&joined).map_err(|error| {
        format!(
            "failed to resolve {name} path {}: {error}",
            joined.display()
        )
    })?;
    if !canonical.starts_with(root) {
        return Err(format!(
            "{name}.path escapes the bundle directory: {relative}"
        ));
    }
    Ok(canonical)
}

fn field<'a>(value: &'a Value, name: &str) -> Result<&'a str, String> {
    value
        .get(name)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("manifest is missing string field {name}"))
}

fn hex32(text: &str, name: &str) -> Result<[u8; 32], String> {
    if text.len() != 64 {
        return Err(format!("{name} must be a 64-digit hex digest"));
    }
    let mut bytes = [0u8; 32];
    for (index, chunk) in text.as_bytes().chunks(2).enumerate() {
        let pair = std::str::from_utf8(chunk)
            .map_err(|_| format!("{name} contains non-ASCII characters"))?;
        bytes[index] = u8::from_str_radix(pair, 16).map_err(|_| format!("{name} is not hex"))?;
    }
    Ok(bytes)
}

fn show(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn digest_file(path: &Path) -> Result<[u8; 32], String> {
    let mut file = fs::File::open(path)
        .map_err(|error| format!("failed to open {}: {error}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 1 << 16];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|error| format!("failed to read {}: {error}", path.display()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hasher.finalize().into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// 测试并行读写进程环境，用同一把锁串行化。
    static ENV: Mutex<()> = Mutex::new(());

    fn scratch(name: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|value| value.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!("diskedit-{name}-{nanos}"));
        fs::create_dir_all(&dir).expect("创建临时目录");
        dir
    }

    fn write_manifest(root: &Path, manifest: &Value) {
        fs::write(
            root.join("manifest.json"),
            serde_json::to_vec(manifest).expect("序列化清单"),
        )
        .expect("写清单");
    }

    fn base(kernel: &str, initramfs: &str) -> Value {
        serde_json::json!({
            "schema": 1,
            "arch": std::env::consts::ARCH,
            "binary_version": env!("CARGO_PKG_VERSION"),
            "kernel": { "path": kernel, "sha256": show(&digest_of("kernel")) },
            "initramfs": { "path": initramfs, "sha256": show(&digest_of("initramfs")) },
        })
    }

    fn digest_of(name: &str) -> [u8; 32] {
        let mut hasher = Sha256::new();
        hasher.update(name.as_bytes());
        hasher.finalize().into()
    }

    fn prepare(root: &Path) {
        fs::write(root.join("kernel"), b"kernel").expect("写内核");
        fs::write(root.join("initramfs"), b"initramfs").expect("写 initramfs");
    }

    #[test]
    fn accepts_manifest_without_worker() {
        let _guard = ENV.lock().expect("环境锁");
        let dir = scratch("manifest-ok");
        prepare(&dir);
        write_manifest(&dir, &base("kernel", "initramfs"));
        let previous = std::env::var_os(VARIABLE);
        unsafe { std::env::set_var(VARIABLE, &dir) };
        let bundle = resolve().expect("校验应通过").expect("包应存在");
        assert_eq!(bundle.kernel, fs::canonicalize(dir.join("kernel")).unwrap());
        assert!(bundle.worker.is_none());
        restore(previous);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn rejects_digest_mismatch() {
        let _guard = ENV.lock().expect("环境锁");
        let dir = scratch("manifest-digest");
        prepare(&dir);
        let mut manifest = base("kernel", "initramfs");
        manifest["kernel"]["sha256"] = Value::String("00".repeat(32));
        write_manifest(&dir, &manifest);
        let previous = std::env::var_os(VARIABLE);
        unsafe { std::env::set_var(VARIABLE, &dir) };
        assert!(resolve().is_err());
        restore(previous);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn rejects_path_that_leaves_the_bundle() {
        let _guard = ENV.lock().expect("环境锁");
        let dir = scratch("manifest-escape");
        prepare(&dir);
        write_manifest(&dir, &base("../kernel", "initramfs"));
        let previous = std::env::var_os(VARIABLE);
        unsafe { std::env::set_var(VARIABLE, &dir) };
        assert!(resolve().is_err());
        restore(previous);
        fs::remove_dir_all(&dir).ok();
    }

    fn restore(previous: Option<std::ffi::OsString>) {
        match previous {
            Some(value) => unsafe { std::env::set_var(VARIABLE, value) },
            None => unsafe { std::env::remove_var(VARIABLE) },
        }
    }
}
