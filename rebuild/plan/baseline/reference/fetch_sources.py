#!/usr/bin/env python3
"""按 sources.tsv 取回参考工具源码树：下载 → sha256 验证 → 解包到 REF_SRC。

用法：
    python3 plan/baseline/reference/fetch_sources.py [--dest /tmp/refsrc/src] [--only parted,libfdisk]

规则：
1. 只信 sources.tsv 里的 URL 与 sha256；URL 失效或 sha 不符一律退出非零，不静默换源。
2. 升级版本时：先在 latest_check 列的官方索引页确认最新稳定版，改 sources.tsv 的
   version/upstream/sha256，再跑本脚本——不许直接从第三方镜像"拿个最新"。
3. 解包目标目录名 = sources.tsv scope 约定的 root（mine_reference.py 的 root 字段）。
"""
import argparse
import csv
import hashlib
import os
import pathlib
import subprocess
import sys
import tarfile

SOURCES = pathlib.Path(__file__).resolve().parent / "sources.tsv"
# sources.tsv tool -> mine_reference.py root
ROOTS = {
    "parted": "parted-{v}", "libfdisk": "util-linux-{v}", "gdisk": "gptfdisk-{v}",
    "e2fsprogs": "e2fsprogs-{v}", "xfsprogs": "xfsprogs-{v}", "btrfs-progs": "btrfs-progs-v{v}",
    "ntfs-3g": "ntfs-3g-{v}", "f2fs-tools": "f2fs-tools-v{v}", "dosfstools": "dosfstools-{v}",
    "lvm2": "lvm2-{v}", "testdisk": "testdisk-{v}",
}


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--dest", default=os.environ.get("REF_SRC", "/tmp/refsrc/src"))
    ap.add_argument("--only", default="")
    a = ap.parse_args()
    dest = pathlib.Path(a.dest)
    dest.mkdir(parents=True, exist_ok=True)
    only = {x.strip() for x in a.only.split(",") if x.strip()}
    bad = []
    with SOURCES.open(encoding="utf-8") as f:
        for row in csv.DictReader(f, delimiter="\t"):
            tool = row["tool"]
            if only and tool not in only:
                continue
            url, sha, version = row["upstream"], row["sha256"], row["version"]
            if not url or url.startswith(("https://uefi.org", "https://learn.microsoft.com")):
                continue  # 规范/文档条目不走源码树
            if not sha:
                bad.append(f"{tool}: sources.tsv 缺 sha256，拒绝下载")
                continue
            tarball = dest.parent / "dl" / f"{tool}.tarball"
            tarball.parent.mkdir(parents=True, exist_ok=True)
            if not tarball.exists() or hashlib.sha256(tarball.read_bytes()).hexdigest() != sha:
                proc = subprocess.run(["curl", "-sSL", "--fail", "-o", str(tarball), url])
                if proc.returncode != 0:
                    bad.append(f"{tool}: 下载失败 {url}")
                    continue
            got = hashlib.sha256(tarball.read_bytes()).hexdigest()
            if got != sha:
                bad.append(f"{tool}: sha256 不符（期望 {sha[:12]}，实得 {got[:12]}）——上游已变动，先人工确认")
                continue
            root_tmpl = ROOTS.get(tool)
            if not root_tmpl:
                continue
            root = dest / root_tmpl.format(v=version)
            if root.is_dir() and any(root.iterdir()):
                print(f"fetch: {tool} {version} 已就位 {root}")
                continue
            root.mkdir(parents=True, exist_ok=True)
            mode = "r:xz" if tarball.read_bytes()[:6] == b"\xfd7zX" else "r:gz"
            try:
                with tarfile.open(tarball, mode) as t:
                    members = t.getmembers()
                    strip = len(members[0].name.split("/")) - 1 if members else 0
                    for m in members:
                        parts = m.name.split("/")[strip:]
                        if not parts or parts[-1] in ("", None) and m.isdir():
                            continue
                        m.name = "/".join(p for p in parts if p)
                        if not m.name:
                            continue
                        t.extract(m, root, filter="data")
            except (tarfile.TarError, OSError) as exc:
                bad.append(f"{tool}: 解包失败 {exc}")
                continue
            print(f"fetch: {tool} {version} -> {root}")
    if bad:
        for b in bad:
            print(f"fetch: {b}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
