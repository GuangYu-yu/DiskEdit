#!/usr/bin/env python3
"""从固定版本的参考工具源码枚举逐站点账本（审计辅助，不参与构建）。

用法：
    REF_SRC=/tmp/refsrc/src python3 plan/baseline/reference/mine_reference.py

只读 REF_SRC（默认 /tmp/refsrc/src），只写本目录。口径见 README.md：
断言、异常、失败 goto、中止、致命退出、错误报告、拒绝消息；另采隐式默认/同步/
加锁/确认、文档警示、变更日志事故、规范条款。
"""
import hashlib
import os
import pathlib
import re
import sys

SRC = pathlib.Path(os.environ.get("REF_SRC", "/tmp/refsrc/src"))
OUT = pathlib.Path(__file__).resolve().parent
SITES = OUT / "sites"

TOOLS = {
    "parted": {
        "layer": "T1", "version": "3.6", "root": "parted-3.6",
        "code": ["libparted/**/*.c", "libparted/**/*.h", "parted/**/*.c", "parted/**/*.h"],
        "docs": ["doc/parted.texi", "doc/*.8", "doc/*.8.in"],
        "logs": ["NEWS"],
    },
    "libfdisk": {
        "layer": "T1", "version": "2.39.3", "root": "util-linux-2.39.3",
        "code": ["libfdisk/**/*.c", "libfdisk/**/*.h", "disk-utils/sfdisk.c",
                 "disk-utils/fdisk.c", "disk-utils/cfdisk.c"],
        "docs": ["disk-utils/sfdisk.8", "disk-utils/fdisk.8", "disk-utils/cfdisk.8"],
        "logs": ["NEWS", "Documentation/releases/v2.39-ReleaseNotes"],
    },
    "gdisk": {
        "layer": "T1", "version": "1.0.10", "root": "gptfdisk-1.0.10",
        "code": ["*.cc", "*.h"],
        "docs": ["*.8", "*.8.in"],
        "logs": ["NEWS"],
    },
    "e2fsprogs": {
        "layer": "T2", "version": "1.47.0", "root": "e2fsprogs-1.47.0",
        "code": ["resize/resize2fs.c", "misc/tune2fs.c", "misc/mke2fs.c",
                 "misc/dumpe2fs.c", "e2fsck/*.c", "lib/ext2fs/resize.c",
                 "lib/ext2fs/initialize.c", "lib/ext2fs/openfs.c",
                 "lib/ext2fs/closefs.c", "lib/ext2fs/super.c",
                 "lib/ext2fs/ext2fs.h", "lib/ext2fs/ext2_fs.h",
                 "lib/support/*.c"],
        "docs": ["resize/resize2fs.8.in", "misc/tune2fs.8.in", "misc/mke2fs.8.in",
                 "misc/dumpe2fs.8.in", "misc/e2fsck.8.in", "misc/*.8.in"],
        "logs": ["RELEASE-NOTES", "doc/RelNotes/*"],
    },
    "xfsprogs": {
        "layer": "T2", "version": "6.6.0", "root": "xfsprogs-6.6.0",
        "code": ["growfs/*.c", "mkfs/*.c", "admin/*.sh", "db/sb.c", "db/init.c",
                 "db/command.c"],
        "docs": ["man/man8/xfs_growfs.8", "man/man8/xfs_admin.8",
                 "man/man8/mkfs.xfs.8.in"],
        "logs": ["doc/CHANGES"],
    },
    "btrfs-progs": {
        "layer": "T2", "version": "6.6.3", "root": "btrfs-progs-v6.6.3",
        "code": ["cmds/filesystem.c", "cmds/filesystem-usage.c", "cmds/property.c",
                 "cmds/device.c", "mkfs/*.c", "tune/*.c", "check/*.c"],
        "docs": ["Documentation/btrfs-filesystem.rst", "Documentation/btrfs-check.rst",
                 "Documentation/btrfs-man5.rst", "Documentation/mkfs.btrfs.rst",
                 "Documentation/btrfstune.rst"],
        "logs": ["CHANGES"],
    },
    "ntfs-3g": {
        "layer": "T2", "version": "2022.10.3", "root": "ntfs-3g-2022.10.3",
        "code": ["ntfsprogs/ntfsresize.c", "ntfsprogs/ntfslabel.c",
                 "ntfsprogs/ntfsinfo.c", "ntfsprogs/ntfsfix.c",
                 "ntfsprogs/ntfscluster.c", "libntfs-3g/volume.c",
                 "libntfs-3g/bootsect.c", "libntfs-3g/mst.c", "libntfs-3g/logfile.c",
                 "libntfs-3g/bitmap.c", "libntfs-3g/attrib.c", "libntfs-3g/inode.c"],
        "docs": ["ntfsprogs/ntfsresize.8.in", "ntfsprogs/ntfslabel.8.in",
                 "ntfsprogs/ntfsinfo.8.in", "ntfsprogs/ntfsfix.8.in"],
        "logs": ["ChangeLog", "NEWS"],
    },
    "f2fs-tools": {
        "layer": "T2", "version": "1.16.0", "root": "f2fs-tools-v1.16.0",
        "code": ["mkfs/*.c", "fsck/*.c", "lib/*.c"],
        "docs": ["man/*.8"],
        "logs": ["ChangeLog"],
    },
    "dosfstools": {
        "layer": "T2", "version": "4.2", "root": "dosfstools-4.2",
        "code": ["src/*.c", "src/blkdev/*.c"],
        "docs": ["manpages/*.8", "manpages/*.8.in"],
        "logs": ["ChangeLog", "doc/ChangeLog.*"],
    },
    "lvm2": {
        "layer": "T2", "version": "2.03.16", "root": "LVM2.2.03.16",
        "code": ["tools/pvresize.c", "tools/lvextend.c", "tools/lvresize.c",
                 "tools/pvchange.c", "tools/pvs.c", "tools/lvs.c", "tools/vgs.c",
                 "tools/vgchange.c", "tools/lvmcmdline.c", "tools/toollib.c",
                 "tools/reporter.c", "lib/metadata/pv_manip.c",
                 "lib/metadata/lv_manip.c", "lib/metadata/vg_manip.c",
                 "lib/metadata/metadata.c", "lib/device/dev-io.c"],
        "docs": ["man/pvresize.8_des", "man/pvresize.8_end", "man/lvextend.8_des",
                 "man/lvextend.8_end", "man/lvresize.8_des", "man/lvresize.8_end",
                 "man/pvchange.8_des", "man/pvchange.8_end", "man/pvs.8_end",
                 "man/lvs.8_end", "man/vgs.8_end", "man/lvm.8_main"],
        "logs": ["WHATS_NEW"],
    },
    "testdisk": {
        "layer": "T3", "version": "7.2", "root": "testdisk-7.2",
        "code": ["src/diskacc.c", "src/analyse.c", "src/part*.c", "src/tmbrcode.c",
                 "src/chgtype*.c", "src/addpart*.c", "src/autoset.c",
                 "src/intrface.c"],
        "docs": ["man/*.8", "man/*.8.in"],
        "logs": ["NEWS", "ChangeLog"],
    },
}

ASSERT = re.compile(r"\b(?:PED_ASSERT|assert|ASSERT|g_assert|Q_ASSERT|BUG_ON)\s*\(")
ABORT = re.compile(r"\b(?:abort|panic)\s*\(")
EXC = re.compile(r"\b(?:ped_exception_throw|throw)\b")
GOTO = re.compile(r"\bgoto\s+(?:fail|error|err|out|done|cleanup)\w*\s*;")
FATAL = re.compile(r"\b(?:err|errx|die|fatal|Exit|exit|_exit|usage)\s*\(")
ERRSITE = re.compile(
    r"(?:\berr\s*\(|\berrx\s*\(|\bwarn\s*\(|\bwarnx\s*\(|\bdie\s*\(|\bperror\s*\("
    r"|fprintf\s*\(\s*stderr|fputs\s*\([^,]+,\s*stderr|ped_exception_throw|\bthrow\b"
    r"|\bExit\s*\(|\berror\s*\(|\bcerr\s*<<|std::cerr"
    r"|\blog_(?:error|warn|warning|critical)\s*\("
    r"|\b(?:display_message|screen_buffer_add)\s*\()"
)
ACTION = re.compile(
    r"cannot|can not|invalid|unsupported|not supported|refus|too (?:small|large|many)"
    r"|must (?:be|not)|requires?|no (?:space|room|free)|exceed|overflow|underflow"
    r"|corrupt|truncat|misalign|overlap|out of range|not enough|destroy|data loss"
    r"|overwrit|irreversible|read.?only|mounted|busy|conflict",
    re.I,
)
FUNC = re.compile(
    r"^\s*(?:static\s+|inline\s+|extern\s+|const\s+|unsigned\s+|signed\s+|struct\s+\w+\s+)*"
    r"[A-Za-z_][\w \*><:]*?\b([A-Za-z_]\w*)\s*\([^;]*$"
)
CONTROL = {"if", "for", "while", "switch", "return", "sizeof", "do", "else", "case"}

DOC_HEAD = re.compile(
    r"^(?:caveats?|bugs?|warnings?|notes?|security|limitations?|known (?:bugs?|issues?)"
    r"|practical use)$",
    re.I,
)


def files_for(root, globs):
    seen, out = set(), []
    for pattern in globs:
        for path in sorted(root.glob(pattern)):
            if path.is_file() and path not in seen:
                seen.add(path)
                out.append(path)
    return out


def area_of(tool, rel):
    s = rel.lower()
    table = [
        ("label-uuid", ["label", "uuid", "tune2fs", "bfstune"]),
        ("swap", ["swap"]),
        ("resize", ["resize", "grow", "shrink", "lvextend", "pvresize"]),
        ("mkfs", ["mkfs", "mke2fs", "initialize", "mkntfs"]),
        ("check", ["fsck", "e2fsck", "check", "repair"]),
        ("probe", ["dumpe2fs", "info", "show", "probe"]),
        ("recovery", ["recover", "analyse", "partgpt", "partmbr", "partnone", "diskacc"]),
        ("lvm", ["lvm", "pv_", "lv_", "vg_", "metadata", "pvresize", "lvextend"]),
        ("mbr", ["dos", "mbr"]),
        ("gpt", ["gpt"]),
        ("align", ["align", "constraint", "nat"]),
        ("table-edit", ["disk", "label", "table"]),
        ("frontend", ["parted/", "disk-utils/", "cmds/", "tools/", "ntfsprogs/", "src/"]),
    ]
    for name, keys in table:
        if any(k in s for k in keys):
            return name
    return "misc"


def risk_for(category, text):
    if category in {"assert", "abort", "exception", "fail-goto", "fatal-exit"}:
        return "high"
    if re.search(r"corrupt|data loss|destroy|overwrit|truncat|irreversible", text, re.I):
        return "high"
    if category == "refusal":
        return "high" if re.search(r"invalid|unsupported|cannot|refus|exceed", text, re.I) else "medium"
    return "medium"


def read_lines(path):
    return path.read_text(encoding="utf-8", errors="ignore").splitlines()


def code_only(lines):
    """去掉块注释与行注释，保留行号对齐；返回与 lines 等长的代码视图。"""
    text = "\n".join(lines)
    text = re.sub(
        r"/\*.*?\*/",
        lambda m: re.sub(r"[^\n]", " ", m.group(0)),
        text,
        flags=re.S,
    )
    out = []
    for line in text.split("\n"):
        quote = None
        escaped = False
        cut = len(line)
        for i, ch in enumerate(line):
            if escaped:
                escaped = False
                continue
            if ch == "\\" and quote:
                escaped = True
                continue
            if quote:
                if ch == quote:
                    quote = None
                continue
            if ch in {'"', "'"}:
                quote = ch
                continue
            if ch == "/" and i + 1 < len(line) and line[i + 1] == "/":
                cut = i
                break
        out.append(line[:cut])
    return out


def window(lines, i, count=3):
    parts = []
    for line in lines[i:i + count]:
        text = line.strip()
        if text:
            parts.append(re.sub(r"\s+", " ", text))
        if len(" ".join(parts)) > 240:
            break
    return " ".join(parts)[:240]


def mine_tool(slug, spec):
    root = SRC / spec["root"]
    rows, counters = [], {"files": 0, "assert": 0, "abort": 0, "exception": 0,
                          "fail-goto": 0, "fatal-exit": 0, "refusal": 0,
                          "error-report": 0}
    seq = 0
    for path in files_for(root, spec["code"]):
        rel = str(path.relative_to(root))
        lines = read_lines(path)
        codes = code_only(lines)
        counters["files"] += 1
        symbol = "-"
        for i, line in enumerate(lines):
            code = codes[i]
            hit = FUNC.match(code)
            if hit and hit.group(1) not in CONTROL:
                symbol = hit.group(1)
            tags, text = [], window(lines, i)
            n_assert = len(ASSERT.findall(code))
            if n_assert:
                tags.append("assert")
                counters["assert"] += n_assert
            if EXC.search(code):
                tags.append("exception")
                counters["exception"] += len(EXC.findall(code))
            if ABORT.search(code):
                tags.append("abort")
                counters["abort"] += 1
            n_goto = len(GOTO.findall(code))
            if n_goto:
                tags.append("fail-goto")
                counters["fail-goto"] += n_goto
            if FATAL.search(code) and not re.fullmatch(r"\s*exit\s*\(\s*0\s*\)\s*;?\s*", code):
                tags.append("fatal-exit")
                counters["fatal-exit"] += 1
            if ACTION.search(text) and (ERRSITE.search(code) or re.search(r"_\s*\(\s*\"", code)):
                tags.append("refusal")
                counters["refusal"] += 1
            elif ERRSITE.search(code):
                tags.append("error-report")
                counters["error-report"] += 1
            if not tags:
                continue
            category = next(
                (c for c in ["assert", "exception", "abort", "fail-goto",
                             "fatal-exit", "refusal", "error-report"] if c in tags),
                tags[0],
            )
            seq += 1
            rows.append({
                "site_id": f"{slug}-{seq:05d}",
                "tool": slug,
                "version": spec["version"],
                "layer": spec["layer"],
                "area": area_of(slug, rel),
                "category": category,
                "tags": ",".join(tags),
                "file": rel,
                "line": str(i + 1),
                "symbol": symbol,
                "finding": text,
                "risk": risk_for(category, text),
                "disposition": "待判",
                "note": "",
            })
    return rows, counters


def mine_docs(slug, spec):
    root = SRC / spec["root"]
    rows = []
    for path in files_for(root, spec["docs"]):
        rel = str(path.relative_to(root))
        lines = read_lines(path)
        name = path.name.lower()
        roff = name.endswith((".8", ".8.in", ".5", ".5.in", ".1", ".1.in",
                              "_des", "_end", "_main"))
        texi = name.endswith(".texi") or name.endswith(".info")
        rst = name.endswith((".rst", ".md"))
        section = None
        started = False
        block, block_line = [], 0

        def flush():
            nonlocal block, block_line
            if block and started:
                text = re.sub(r"\\f[BIRP]|\\-|\\&|\\e", "", " ".join(block))
                rows.append((rel, block_line, section, text, "man" if roff else "doc"))
            block, block_line = [], 0

        for i, line in enumerate(lines):
            stripped = line.strip()
            if roff:
                m = re.match(r"^\.SH\s+\"?([^\"]+)\"?", stripped, re.I)
                if m:
                    flush()
                    section = m.group(1).strip()
                    started = bool(DOC_HEAD.match(section))
                    continue
                if stripped.startswith(".SH"):
                    flush()
                    section, started = None, False
                    continue
                if not stripped or stripped.startswith("."):
                    flush()
                    continue
                if started:
                    if not block:
                        block_line = i + 1
                    block.append(stripped)
            elif texi:
                m = re.match(r"^@(?:node|section|subsection|subsubsection|chapter)\s+(.+)", stripped, re.I)
                if m:
                    flush()
                    section = m.group(1).strip()
                    started = bool(DOC_HEAD.search(section))
                    continue
                if not stripped:
                    flush()
                    continue
                if stripped.startswith("@"):
                    flush()
                    continue
                if not started and re.search(r"warning|data loss|corrupt|unrecoverable|do not|cannot", stripped, re.I):
                    started = True
                    section = section or "-"
                if started:
                    if not block:
                        block_line = i + 1
                    block.append(stripped)
            elif rst:
                nxt = lines[i + 1].strip() if i + 1 < len(lines) else ""
                heading = bool(stripped) and bool(re.match(r"^[=\-~^\"']{3,}$", nxt))
                if heading:
                    flush()
                    section = stripped
                    started = bool(DOC_HEAD.match(section))
                    continue
                if stripped.startswith(".. "):
                    flush()
                    m = re.match(r"\.\.\s+(warning|note|caution|important)::\s*(.*)", stripped, re.I)
                    if m:
                        section = m.group(1)
                        started = True
                        rest = m.group(2).strip()
                        if rest:
                            block_line = i + 1
                            block = [rest]
                    continue
                if not stripped:
                    flush()
                    continue
                if started:
                    if not block:
                        block_line = i + 1
                    block.append(stripped)
        flush()
    out = []
    for n, (rel, line, section, text, kind) in enumerate(rows, 1):
        out.append({
            "doc_id": f"{slug}-doc-{n:05d}",
            "tool": slug,
            "version": spec["version"],
            "kind": kind,
            "file": rel,
            "line": str(line),
            "section": section,
            "finding": re.sub(r"\s+", " ", text).strip()[:400],
            "risk": "high" if re.search(r"corrupt|data loss|destroy|irrevers|not supported|cannot|truncat", text, re.I) else "medium",
            "disposition": "待判",
            "note": "",
        })
    return out


RISK_WORDS = re.compile(
    r"CVE-\d|crash|corrupt|data loss|destroy|segfault|overflow|underflow|truncat"
    r"|misalign|wrong|incorrect|lost|panic|race|deadlock",
    re.I,
)


def mine_incidents(slug, spec):
    root = SRC / spec["root"]
    rows = []
    for path in files_for(root, spec["logs"]):
        rel = str(path.relative_to(root))
        release = "-"
        block, block_line = [], 0
        for i, line in enumerate(read_lines(path)):
            stripped = line.strip()
            if re.match(r"^(?:v?\d+\.\d+(?:\.\d+)?(?:-\w+)?|\d{4}-\d{2}-\d{2}|Release\s+\S+|Version\s+\S+)", stripped):
                release = stripped[:60]
            bullet = bool(re.match(r"^(?:[*+-]\s|\d+[.)]\s)", stripped))
            if not stripped or bullet or re.match(r"^[-=]{3,}$", stripped):
                if block:
                    text = " ".join(block)
                    if RISK_WORDS.search(text):
                        rows.append({
                            "incident_id": f"{slug}-inc-{len(rows)+1:05d}",
                            "tool": slug,
                            "version": spec["version"],
                            "file": rel,
                            "line": str(block_line),
                            "release": release,
                            "finding": re.sub(r"\s+", " ", text)[:400],
                            "risk": "high" if re.search(r"CVE-\d|data loss|corrupt|crash|destroy|lost", text, re.I) else "medium",
                            "disposition": "待判",
                            "note": "",
                        })
                block, block_line = [], 0
            if not stripped or re.match(r"^[-=]{3,}$", stripped):
                continue
            if not block:
                block_line = i + 1
            block.append(stripped)
        if block:
            text = " ".join(block)
            if RISK_WORDS.search(text):
                rows.append({
                    "incident_id": f"{slug}-inc-{len(rows)+1:05d}",
                    "tool": slug, "version": spec["version"], "file": rel,
                    "line": str(block_line), "release": release,
                    "finding": re.sub(r"\s+", " ", text)[:400],
                    "risk": "high" if re.search(r"CVE-\d|data loss|corrupt|crash|destroy|lost", text, re.I) else "medium",
                    "disposition": "待判", "note": "",
                })
    return rows


IMPLICIT = [
    ("default", re.compile(r"#\s*define\s+\w*(?:ALIGN|GRAIN|DEFAULT|MAX_|MIN_|SECTOR|MIB|MB|CHUNK|TIMEOUT|RETRY|CONFIRM|FIRST|LAST)\w*", re.I)),
    ("sync", re.compile(r"\b(?:fsync|fdatasync|sync_file_range|syncfs)\s*\(")),
    ("kernel-reload", re.compile(r"\b(?:BLKRRPART|BLKPG|BLKPG_[A-Z_]+)\b")),
    ("lock", re.compile(r"\b(?:O_EXCL|O_DIRECT|flock|F_SETLK|LOCK_EX)\b")),
    ("confirm", re.compile(r"\b(?:confirm|yes_no|ask_yes|interactive|force)\w*\b", re.I)),
]


def mine_implicit(slug, spec):
    root = SRC / spec["root"]
    rows, seq = [], 0
    for path in files_for(root, spec["code"]):
        rel = str(path.relative_to(root))
        codes = code_only(read_lines(path))
        for i, code in enumerate(codes):
            for kind, pattern in IMPLICIT:
                if pattern.search(code):
                    seq += 1
                    text = re.sub(r"\s+", " ", code.strip())[:240]
                    rows.append({
                        "implicit_id": f"{slug}-imp-{seq:05d}",
                        "tool": slug,
                        "version": spec["version"],
                        "layer": spec["layer"],
                        "area": area_of(slug, rel),
                        "kind": kind,
                        "file": rel,
                        "line": str(i + 1),
                        "finding": text,
                        "risk": "medium",
                        "disposition": "待判",
                        "note": "",
                    })
                    break
    return rows


def write_tsv(path, columns, rows):
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("w", encoding="utf-8") as handle:
        handle.write("\t".join(columns) + "\n")
        for row in rows:
            handle.write("\t".join(str(row.get(c, "")).replace("\t", " ") for c in columns) + "\n")


def main():
    OUT.mkdir(parents=True, exist_ok=True)
    SITES.mkdir(parents=True, exist_ok=True)
    summary, all_docs, all_inc, all_imp = [], [], [], []
    site_cols = ["site_id", "tool", "version", "layer", "area", "category", "tags",
                 "file", "line", "symbol", "finding", "risk", "disposition", "note"]
    for slug, spec in TOOLS.items():
        rows, counters = mine_tool(slug, spec)
        write_tsv(SITES / f"{slug}.tsv", site_cols, rows)
        docs = mine_docs(slug, spec)
        inc = mine_incidents(slug, spec)
        imp = mine_implicit(slug, spec)
        all_docs.extend(docs)
        all_inc.extend(inc)
        all_imp.extend(imp)
        summary.append({
            "tool": slug, "layer": spec["layer"], "files": counters["files"],
            "sites": len(rows), "assert": counters["assert"], "abort": counters["abort"],
            "exception": counters["exception"], "fail_goto": counters["fail-goto"],
            "fatal_exit": counters["fatal-exit"], "error_report": counters["error-report"],
            "refusal": counters["refusal"], "docs": len(docs),
            "incidents": len(inc), "implicit": len(imp),
        })
        print(f"{slug}: files={counters['files']} sites={len(rows)} docs={len(docs)} "
              f"incidents={len(inc)} implicit={len(imp)}")
    write_tsv(OUT / "docs.tsv",
              ["doc_id", "tool", "version", "kind", "file", "line", "section",
               "finding", "risk", "disposition", "note"], all_docs)
    write_tsv(OUT / "incidents.tsv",
              ["incident_id", "tool", "version", "file", "line", "release",
               "finding", "risk", "disposition", "note"], all_inc)
    write_tsv(OUT / "implicit.tsv",
              ["implicit_id", "tool", "version", "layer", "area", "kind", "file",
               "line", "finding", "risk", "disposition", "note"], all_imp)
    write_tsv(OUT / "summary.tsv",
              ["tool", "layer", "files", "sites", "assert", "abort", "exception",
               "fail_goto", "fatal_exit", "error_report", "refusal", "docs",
               "incidents", "implicit"], summary)


if __name__ == "__main__":
    main()
