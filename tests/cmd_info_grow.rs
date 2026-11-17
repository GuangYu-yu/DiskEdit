//! `info` 的字段契约。
//!
//! `info` 不带 `--grow` 时只报告当前布局；`--grow` 追加联动搬移预览，
//! 目标字节与目录条目的零写检查由既有 `no_write_probe`（Linux，递归 sha256 快照）
//! 与 `cli/platform_gate`（非 Linux）承担。
//! 目标目录由 `TempDir` 在作用域结束时清理。

mod guards;

use guards::run;

fn fixture() -> tempfile::TempDir {
    let dir = tempfile::Builder::new()
        .prefix("diskedit_cmd_info_grow_")
        .tempdir()
        .expect("create the fixture directory");
    std::fs::write(dir.path().join("disk.img"), vec![0u8; 4 * 1024 * 1024])
        .expect("create the image");
    dir
}

fn json(out: &str) -> serde_json::Value {
    serde_json::from_str(out.trim())
        .unwrap_or_else(|e| panic!("stdout is not one JSON document: {e}\n{out}"))
}

#[test]
fn info_on_a_table_less_image_is_one_json_document() {
    let dir = fixture();
    let img = dir.path().join("disk.img");
    let arg = img.to_str().expect("utf-8 path");
    let (code, out, err) = run(&["info", arg]);
    assert_eq!(code, 0, "{err}");
    let v = json(&out);
    assert_eq!(v["label"], "none", "{out}");
    assert_eq!(v["size_bytes"], 4 * 1024 * 1024, "{out}");
    assert!(
        v["partitions"].as_array().is_some_and(|p| p.is_empty()),
        "a table-less image must report an empty partition array: {out}"
    );
}

#[test]
fn info_grow_requires_a_gpt_table() {
    let dir = fixture();
    let img = dir.path().join("disk.img");
    let arg = format!("{}:1", img.display());
    let (code, _out, err) = run(&["info", &arg, "--grow"]);
    assert_eq!(code, 3, "a table-less image cannot produce a relocation preview: {err}");
    assert!(
        err.contains("GPT partition table"),
        "the refusal must name the actual precondition: {err}"
    );
}
