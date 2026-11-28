//! 导出命令面：把声明的形状写到 `target/command-surface.json`，供门禁核对命令面。

#[test]
fn command_surface_is_exported() {
    let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let target = manifest.join("target");
    std::fs::create_dir_all(&target).expect("target 目录应可创建");
    let document = diskedit::app::cli::declare::surface_json();
    std::fs::write(target.join("command-surface.json"), document)
        .expect("命令面应可写出到 target/command-surface.json");
}
