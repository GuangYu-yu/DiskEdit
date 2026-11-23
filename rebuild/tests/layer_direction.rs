//! 结构仪器：依赖方向由 crate 图与可见性共同保证，此处核对 crate 图。

use std::process::Command;

fn metadata() -> serde_json::Value {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_owned());
    let output = Command::new(cargo)
        .args(["metadata", "--no-deps", "--format-version", "1"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("cargo metadata 应可执行");
    assert!(
        output.status.success(),
        "cargo metadata 失败: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("cargo metadata 输出应为 JSON")
}

#[test]
fn core_does_not_depend_on_any_package() {
    let meta = metadata();
    let packages = meta["packages"].as_array().expect("packages 应为数组");
    let core = packages
        .iter()
        .find(|pkg| pkg["name"] == "diskedit_core")
        .expect("工作区应包含 diskedit_core");
    let deps = core["dependencies"]
        .as_array()
        .expect("dependencies 应为数组");
    assert!(deps.is_empty(), "纯核不得依赖任何包: {deps:?}");
}
