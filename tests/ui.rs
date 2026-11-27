//! 编译失败用例：类型约束无法在运行期检查的部分由编译器拒绝。

#[test]
fn compile_failures() {
    let cases = trybuild::TestCases::new();
    cases.compile_fail("tests/ui/*.rs");
}
