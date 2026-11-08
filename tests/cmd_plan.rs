//! `plan` 的字段契约。
//!
//! `plan` 不带 `--grow` 时与 `info` 是同一份只读报告；`--grow` 追加联动搬移预览，
//! 目标字节与目录条目的零写检查由既有 `no_write_probe`（Linux，递归 sha256 快照）
//! 与 `cli/platform_gate`（非 Linux）承担。
//! 目标目录由 `TempDir` 在作用域结束时清理。

mod guards;

use guards::run;

fn fixture() -> tempfile::TempDir {
    let dir = tempfile::Builder::new()
        .prefix("diskedit_cmd_plan_")
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
fn plan_without_grow_is_the_same_report_as_info() {
    let dir = fixture();
    let img = dir.path().join("disk.img");
    let arg = img.to_str().expect("utf-8 path");
    let (code_info, out_info, err) = run(&["info", arg]);
    assert_eq!(code_info, 0, "{err}");
    let (code_plan, out_plan, err) = run(&["plan", arg]);
    assert_eq!(code_plan, 0, "{err}");
    assert_eq!(
        json(&out_info),
        json(&out_plan),
        "plan without --grow must reuse the info report field-for-field"
    );
}

#[test]
fn plan_grow_requires_a_gpt_table() {
    let dir = fixture();
    let img = dir.path().join("disk.img");
    let arg = format!("{}:1", img.display());
    let (code, _out, err) = run(&["plan", &arg, "--grow"]);
    assert_eq!(code, 3, "a table-less image cannot produce a relocation plan: {err}");
    assert!(
        err.contains("GPT partition table"),
        "the refusal must name the actual precondition: {err}"
    );
}
