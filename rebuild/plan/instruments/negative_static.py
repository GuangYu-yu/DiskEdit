#!/usr/bin/env python3
"""负向声明静态检查：扫源码里的禁用记号，命中即红。

读 `plan/baseline/negative-declarations.tsv` 中 checker=static 的行，按
`检查对象` 列里逗号分隔的记号，扫 `src/` 与 `crates/` 下的 `.rs` 文件。
新增一条静态声明只需在 TSV 加一行，无需改本脚本。
"""

from __future__ import annotations

import pathlib
import sys

ROOT = pathlib.Path(__file__).resolve().parents[2]
TSV = ROOT / "plan/baseline/negative-declarations.tsv"
SCAN_DIRS = (ROOT / "src", ROOT / "crates")


def rows() -> list[list[str]]:
    out: list[list[str]] = []
    for raw in TSV.read_text(encoding="utf-8").splitlines():
        line = raw.strip()
        if not line or line.startswith("#"):
            continue
        cols = [c.strip() for c in line.split("\t")]
        if len(cols) != 5:
            print(f"负向声明行应为 5 列（decl_id/SA/声明/checker/target）：{line}", file=sys.stderr)
            sys.exit(2)
        out.append(cols)
    if not out:
        print("负向声明账本为空", file=sys.stderr)
        sys.exit(2)
    return out


def main() -> int:
    bad = 0
    checked = 0
    for decl, sa, _text, checker, target in rows():
        if checker != "static":
            continue
        tokens = [t for t in target.split(",") if t]
        if not tokens:
            print(f"{decl}({sa}) 是 static 却未给禁用记号", file=sys.stderr)
            return 2
        for directory in SCAN_DIRS:
            if not directory.exists():
                continue
            for path in directory.rglob("*.rs"):
                try:
                    body = path.read_text(encoding="utf-8").lower()
                except OSError:
                    continue
                checked += 1
                for token in tokens:
                    if token.lower() in body:
                        print(f"{decl}({sa}) 命中禁用记号 {token!r}：{path.relative_to(ROOT)}")
                        bad += 1
    print(f"负向声明静态检查：扫 {checked} 个文件，命中 {bad} 处")
    return 1 if bad else 0


if __name__ == "__main__":
    raise SystemExit(main())
