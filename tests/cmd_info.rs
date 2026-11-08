//! `info` 的字段契约。
//!
//! 按字段解析断言，不比较整段输出字节：允许新增键，但基线键集与类型不得漂移。
//! 失败路径也走同一份信封——`label`/`damaged`/`sector_size`/`size_bytes`/
//! `disk_guid`/`partitions` 恒在，`error` 只在失败时出现。
//! 目标目录由 `TempDir` 在作用域结束时清理。

mod guards;

use guards::run;

fn image(bytes: usize) -> tempfile::TempDir {
    let dir = tempfile::Builder::new()
        .prefix("diskedit_cmd_info_")
        .tempdir()
        .expect("create the fixture directory");
    std::fs::write(dir.path().join("disk.img"), vec![0u8; bytes]).expect("create the image");
    dir
}

fn json(out: &str) -> serde_json::Value {
    serde_json::from_str(out.trim())
        .unwrap_or_else(|e| panic!("stdout is not one JSON document: {e}\n{out}"))
}

#[test]
fn info_reports_the_stable_field_set() {
    let dir = image(4 * 1024 * 1024);
    let img = dir.path().join("disk.img");
    let (code, out, err) = run(&["info", img.to_str().expect("utf-8 path")]);
    assert_eq!(code, 0, "`info` must be admitted: {err}");
    let v = json(&out);
    let map = v.as_object().expect("the envelope must be an object");
    for key in [
        "label",
        "damaged",
        "sector_size",
        "size_bytes",
        "disk_guid",
        "partitions",
    ] {
        assert!(
            map.contains_key(key),
            "the stable info field `{key}` is missing: {out}"
        );
    }
    assert_eq!(
        v["label"].as_str(),
        Some("none"),
        "a blank raw image carries no partition table"
    );
    assert_eq!(v["damaged"].as_bool(), Some(false));
    assert_eq!(v["sector_size"].as_u64(), Some(512));
    assert_eq!(v["size_bytes"].as_u64(), Some(4 * 1024 * 1024));
    assert!(v["partitions"].is_array(), "partitions must always be an array");
    assert!(v["disk_guid"].is_null(), "a table-less image has no disk GUID");
}

#[test]
fn info_refuses_a_partition_selector_and_keeps_the_envelope() {
    let dir = image(2 * 1024 * 1024);
    let img = dir.path().join("disk.img");
    let arg = format!("{}:1", img.display());
    let (code, out, err) = run(&["info", &arg]);
    assert_eq!(code, 3, "`info` reports the whole target — :N is refused: {err}");
    let v = json(&out);
    assert!(
        v["error"].is_string(),
        "the failure envelope must still carry `error`: {out}"
    );
    assert!(v["label"].is_null());
    assert!(v["partitions"].is_null());
}

#[test]
fn info_reports_an_unopenable_target_as_no_mutation() {
    let dir = tempfile::Builder::new()
        .prefix("diskedit_cmd_info_missing_")
        .tempdir()
        .expect("create the fixture directory");
    let absent = dir.path().join("not-there.img");
    let (code, out, err) = run(&["info", absent.to_str().expect("utf-8 path")]);
    assert_eq!(code, 4, "an open failure wrote nothing: {err}");
    let v = json(&out);
    assert!(v["error"].is_string(), "{out}");
    assert!(v["sector_size"].is_null() && v["size_bytes"].is_null());
}
