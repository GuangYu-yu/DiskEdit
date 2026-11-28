//! 载体清单校验。
//!
//! 清单声明架构、二进制版本、内核与 initramfs 的相对路径及 SHA-256，以及 worker
//! 映像摘要。任何一项不符都在启动来宾之前拒绝——宿主的目标几何不能被一个来路
//! 不明的来宾接走。

use std::fs;
use std::io::Read;
use std::path::{Component, Path, PathBuf};

use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::platform::{PlatformError, PlatformResult};

use super::BUNDLE_ENV;

/// 清单格式版本。
const SCHEMA: u64 = 1;

/// 通过校验的包。
pub struct Bundle {
    pub kernel: PathBuf,
    pub initramfs: PathBuf,
    pub worker: [u8; 32],
}

impl Bundle {
    /// 读取环境变量指向的包；未配置时为 `None`。
    pub fn resolve() -> PlatformResult<Option<Self>> {
        let Some(root) = std::env::var_os(BUNDLE_ENV) else {
            return Ok(None);
        };
        let root = PathBuf::from(root);
        let manifest = root.join("manifest.json");
        let text = fs::read_to_string(&manifest)
            .map_err(|error| bad(format!("cannot read {}: {error}", manifest.display())))?;
        let value: Value = serde_json::from_str(&text)
            .map_err(|error| bad(format!("{} is not JSON: {error}", manifest.display())))?;
        Ok(Some(Self::load(&root, &value)?))
    }

    fn load(root: &Path, value: &Value) -> PlatformResult<Self> {
        let schema = value.get("schema").and_then(Value::as_u64);
        if schema != Some(SCHEMA) {
            return Err(bad(format!(
                "{BUNDLE_ENV}: manifest schema is {schema:?}, this build reads {SCHEMA}"
            )));
        }
        let arch = field(value, "arch")?;
        if arch != std::env::consts::ARCH {
            return Err(bad(format!(
                "the VM bundle was built for {arch}, this process is {}",
                std::env::consts::ARCH
            )));
        }
        let version = field(value, "binary_version")?;
        if version != env!("CARGO_PKG_VERSION") {
            return Err(bad(format!(
                "the VM bundle carries worker {version}, this host speaks {}",
                env!("CARGO_PKG_VERSION")
            )));
        }
        let canonical = fs::canonicalize(root)
            .map_err(|error| bad(format!("cannot resolve {}: {error}", root.display())))?;
        let kernel = artifact(value, "kernel", &canonical)?;
        let initramfs = artifact(value, "initramfs", &canonical)?;
        let worker_entry = value
            .get("worker")
            .ok_or_else(|| bad("manifest has no `worker` entry".to_owned()))?;
        // worker 摘要只解析登记：它是"这份 bundle 带的是哪一份 worker"的审计事实，
        // 与宿主进程是否同一文件无关（测试宿主与用户宿主都读同一个 bundle）。
        let worker = hex32(field(worker_entry, "sha256")?, "manifest.worker.sha256")?;
        Ok(Self {
            kernel,
            initramfs,
            worker,
        })
    }
}

fn bad(why: impl Into<String>) -> PlatformError {
    PlatformError::not_supported(why.into())
}

fn artifact(value: &Value, name: &str, root: &Path) -> PlatformResult<PathBuf> {
    let entry = value
        .get(name)
        .ok_or_else(|| bad(format!("manifest has no `{name}` entry")))?;
    let relative = field(entry, "path")?;
    let path = inside(root, relative, name)?;
    let declared = hex32(field(entry, "sha256")?, &format!("manifest.{name}.sha256"))?;
    let actual = digest_file(&path)?;
    if declared != actual {
        return Err(bad(format!(
            "the bundle's {name} does not match manifest.{name}.sha256 (declared {}, read {})",
            show(&declared),
            show(&actual)
        )));
    }
    Ok(path)
}

/// 只接受落在包目录内的纯相对路径。
fn inside(root: &Path, relative: &str, name: &str) -> PlatformResult<PathBuf> {
    let path = Path::new(relative);
    let mut normal = false;
    for component in path.components() {
        match component {
            Component::Normal(_) => normal = true,
            Component::CurDir
            | Component::ParentDir
            | Component::RootDir
            | Component::Prefix(_) => {
                return Err(bad(format!(
                    "manifest.{name}.path `{relative}` is not a plain relative path"
                )));
            }
        }
    }
    if !normal {
        return Err(bad(format!("manifest.{name}.path must not be empty")));
    }
    let joined = root.join(path);
    let canonical = fs::canonicalize(&joined)
        .map_err(|error| bad(format!("cannot resolve {}: {error}", joined.display())))?;
    if !canonical.starts_with(root) {
        return Err(bad(format!(
            "manifest.{name}.path escapes the bundle: {relative}"
        )));
    }
    Ok(canonical)
}

fn field<'a>(value: &'a Value, name: &str) -> PlatformResult<&'a str> {
    value
        .get(name)
        .and_then(Value::as_str)
        .ok_or_else(|| bad(format!("manifest field {name} is missing or not a string")))
}

fn hex32(text: &str, name: &str) -> PlatformResult<[u8; 32]> {
    if text.len() != 64 {
        return Err(bad(format!("{name} must be a 64-digit hex digest")));
    }
    let mut bytes = [0u8; 32];
    for (index, chunk) in text.as_bytes().chunks(2).enumerate() {
        let pair =
            std::str::from_utf8(chunk).map_err(|_| bad(format!("{name} is not ASCII hex")))?;
        bytes[index] =
            u8::from_str_radix(pair, 16).map_err(|_| bad(format!("{name} is not hex")))?;
    }
    Ok(bytes)
}

fn show(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

pub(crate) fn digest_file(path: &Path) -> PlatformResult<[u8; 32]> {
    let mut file = fs::File::open(path).map_err(|error| PlatformError::io(path, error))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 1 << 16];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|error| PlatformError::io(path, error))?;
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

    static ENV: Mutex<()> = Mutex::new(());

    fn scratch(name: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|value| value.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!("diskedit-{name}-{nanos}"));
        fs::create_dir_all(&dir).expect("建立临时目录");
        dir
    }

    fn digest_of(bytes: &[u8]) -> [u8; 32] {
        let mut hasher = Sha256::new();
        hasher.update(bytes);
        hasher.finalize().into()
    }

    fn manifest(_root: &Path, kernel: &str) -> Value {
        serde_json::json!({
            "schema": 1,
            "arch": std::env::consts::ARCH,
            "binary_version": env!("CARGO_PKG_VERSION"),
            "kernel": { "path": kernel, "sha256": show(&digest_of(b"kernel")) },
            "initramfs": { "path": "initramfs", "sha256": show(&digest_of(b"initramfs")) },
            "worker": { "sha256": "00".repeat(32) },
        })
    }

    fn prepare(root: &Path) -> Value {
        fs::write(root.join("kernel"), b"kernel").expect("写内核");
        fs::write(root.join("initramfs"), b"initramfs").expect("写 initramfs");
        let value = manifest(root, "kernel");
        fs::write(
            root.join("manifest.json"),
            serde_json::to_vec(&value).expect("序列化清单"),
        )
        .expect("写清单");
        value
    }

    fn with_bundle<T>(root: &Path, body: impl FnOnce() -> T) -> T {
        let _guard = ENV.lock().expect("环境锁");
        let previous = std::env::var_os(BUNDLE_ENV);
        // SAFETY: 测试用同一把锁串行化进程环境。
        unsafe { std::env::set_var(BUNDLE_ENV, root) };
        let result = body();
        match previous {
            Some(value) => unsafe { std::env::set_var(BUNDLE_ENV, value) },
            None => unsafe { std::env::remove_var(BUNDLE_ENV) },
        }
        result
    }

    #[test]
    fn a_matching_bundle_is_accepted() {
        let root = scratch("bundle-ok");
        prepare(&root);
        let result = with_bundle(&root, || Bundle::resolve().map(|bundle| bundle.is_some()));
        assert!(matches!(result, Ok(true)));
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_digest_mismatch_is_refused_before_boot() {
        let root = scratch("bundle-digest");
        let mut value = prepare(&root);
        value["kernel"]["sha256"] = Value::String("00".repeat(32));
        fs::write(
            root.join("manifest.json"),
            serde_json::to_vec(&value).unwrap(),
        )
        .unwrap();
        assert!(with_bundle(&root, Bundle::resolve).is_err());
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_path_leaving_the_bundle_is_refused() {
        let root = scratch("bundle-escape");
        let value = prepare(&root);
        let mut escape = value;
        escape["kernel"]["path"] = Value::String("../kernel".to_owned());
        fs::write(
            root.join("manifest.json"),
            serde_json::to_vec(&escape).unwrap(),
        )
        .unwrap();
        assert!(with_bundle(&root, Bundle::resolve).is_err());
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_malformed_worker_digest_or_a_different_version_is_refused() {
        let root = scratch("bundle-worker");
        let mut value = prepare(&root);
        value["worker"]["sha256"] = Value::String("not-a-digest".to_owned());
        fs::write(
            root.join("manifest.json"),
            serde_json::to_vec(&value).unwrap(),
        )
        .unwrap();
        assert!(with_bundle(&root, Bundle::resolve).is_err());

        let mut value = prepare(&root);
        value["binary_version"] = Value::String("9.9.9".to_owned());
        fs::write(
            root.join("manifest.json"),
            serde_json::to_vec(&value).unwrap(),
        )
        .unwrap();
        assert!(with_bundle(&root, Bundle::resolve).is_err());
        fs::remove_dir_all(&root).ok();
    }
}
