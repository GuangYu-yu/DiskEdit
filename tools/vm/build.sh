#!/usr/bin/env bash
# 组装 VM 载体 bundle：kernel + initramfs + manifest（含 sha256）。
#
# 用法：
#   tools/vm/build.sh                      # 用环境变量里的默认值
#   BIN=target/x86_64-unknown-linux-musl/release/DiskEdit \
#   KERNEL=/boot/vmlinuz-carrier \
#   tools/vm/build.sh
#
# 必填/可选项：
#   BIN           宿主提供的 DiskEdit 可执行文件（会被原样放进 guest 的 /bin/diskedit）
#   OUT           bundle 输出目录，默认 dist/vm-bundle
#   KERNEL        已有内核。设置它就不再编内核
#   KERNEL_SRC    内核源码目录（配合 KERNEL 缺省时使用：defconfig + kernel.config → bzImage）
#   ARCH          x86_64（默认）或 aarch64
#   TOOLS_DIR     工具目录；不设时按 PATH 查找
#
# 设计约束：
# * `manifest.json` 的 `tools` 数组**不由本脚本生成**——它由 `fsops::tests::manifest_derived_from_tool_enum`
#   与工具注册表对齐，手改会让那条守卫红。脚本只填 arch / binary_version / 两个制品的 sha256 / worker.sha256。
# * 缺工具就地失败，不静默跳过：缺一个工具，`manifest.json` 与实际 initramfs 就会不同源。

set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)

: "${BIN:?set BIN to the DiskEdit executable for the guest (usually the musl release build)}"
: "${OUT:=$root/dist/vm-bundle}"
: "${ARCH:=x86_64}"
: "${TOOLS_DIR:=}"

case "$ARCH" in
  x86_64) machine=bzImage; karch=x86 ;;
  aarch64) machine=Image; karch=arm64 ;;
  *) echo "unsupported ARCH=$ARCH (expected x86_64 or aarch64)" >&2; exit 2 ;;
esac

[ -x "$BIN" ] || { echo "BIN is not executable: $BIN" >&2; exit 2; }
for tool in python3 sha256sum; do
  command -v "$tool" >/dev/null || { echo "missing build dependency: $tool" >&2; exit 2; }
done

version=$(sed -n 's/^version *= *"\([^"]*\)".*/\1/p' "$root/Cargo.toml" | head -1)
[ -n "$version" ] || { echo "cannot read the package version from Cargo.toml" >&2; exit 2; }
version=${BINARY_VERSION:-$version}

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

# ---- 内核 ----------------------------------------------------------------
if [ -n "${KERNEL:-}" ]; then
  [ -r "$KERNEL" ] || { echo "KERNEL is not readable: $KERNEL" >&2; exit 2; }
  cp "$KERNEL" "$work/kernel"
else
  [ -n "${KERNEL_SRC:-}" ] || {
    echo "set KERNEL to a prebuilt kernel, or KERNEL_SRC to a Linux source tree" >&2
    exit 2
  }
  [ -x "$KERNEL_SRC/scripts/kconfig/merge_config.sh" ] || {
    echo "KERNEL_SRC does not look like a Linux source tree: $KERNEL_SRC" >&2
    exit 2
  }
  echo "building the guest kernel from $KERNEL_SRC ($machine)"
  make -C "$KERNEL_SRC" ARCH="$karch" defconfig >/dev/null
  "$KERNEL_SRC/scripts/kconfig/merge_config.sh" -m -O "$KERNEL_SRC" \
      "$KERNEL_SRC/.config" "$here/kernel.config" >/dev/null
  make -C "$KERNEL_SRC" ARCH="$karch" olddefconfig >/dev/null
  make -C "$KERNEL_SRC" ARCH="$karch" -j"$(nproc)" "$machine"
  cp "$KERNEL_SRC/arch/$karch/boot/$machine" "$work/kernel"
fi

# ---- initramfs -----------------------------------------------------------
guest="$work/root"
mkdir -p "$guest/bin" "$guest/dev" "$guest/proc" "$guest/sys" "$guest/tmp"
install -m 0755 "$here/init" "$guest/init"
install -m 0755 "$BIN" "$guest/bin/diskedit"

# /init 是一个 shell 脚本，它用到的外部命令只有 `mount`、`cat` 与 `stty`（`echo`/`exec`
# 是 shell 内建），这三个因此是基础设施，不列进 `manifest.json` 的 `tools`（那张表只列
# 文件系统工具，由工具注册表守卫）；但仍要进 initramfs，否则 guest 起不来
for infra in sh cat stty; do
  path=$(command -v "$infra" || true)
  [ -n "$path" ] || { echo "missing initramfs infrastructure command: $infra" >&2; exit 2; }
  install -m 0755 "$path" "$guest/bin/$infra"
done

# 工具清单只信任 manifest.json（它与工具注册表由守卫锁在一起）
mapfile -t tools < <(python3 - "$here/manifest.json" <<'PY'
import json, sys
print("\n".join(json.load(open(sys.argv[1]))["tools"]))
PY
)

copy_libs() {
  local binary=$1
  local line path
  # 静态链接的二进制 ldd 会说 "not a dynamic executable"（退出码非零），
  # 这不是错误：用 `|| true` 让管道成功，避免 `pipefail` 把"没有依赖"判成失败
  { ldd "$binary" 2>/dev/null || true; } | while read -r line; do
    case "$line" in
      *"=>"*) path=$(printf '%s\n' "$line" | awk '{print $3}') ;;
      /*) path=$(printf '%s\n' "$line" | awk '{print $1}') ;;
      *) continue ;;
    esac
    [ -e "$path" ] || continue
    mkdir -p "$guest$(dirname "$path")"
    cp -L "$path" "$guest$path"
  done
}

for tool in "${tools[@]}"; do
  if [ -n "$TOOLS_DIR" ]; then
    source_bin=$TOOLS_DIR/$tool
  else
    source_bin=$(command -v "$tool" || true)
  fi
  [ -n "$source_bin" ] && [ -x "$source_bin" ] || {
    echo "missing guest tool: $tool (install it or point TOOLS_DIR at a directory holding it)" >&2
    exit 2
  }
  install -m 0755 "$source_bin" "$guest/bin/$tool"
  copy_libs "$source_bin"
done
copy_libs "$BIN"
for infra in sh cat; do
  copy_libs "$guest/bin/$infra"
done

# newc 由本脚本自己写：`cpio` 不是每个宿主都装，而归档格式只有"头 + 名 + 数据"三段。
# 时间戳一律写 0，同样的输入产出同样的归档（bundle 的 sha256 才可比）。
python3 - "$guest" "$work/initramfs" <<'PY'
import gzip, os, stat, sys

root, out = sys.argv[1:3]

def entry(name, mode, data):
    fields = [0, mode, 0, 0, 1, 0, len(data), 0, 0, 0, 0, len(name) + 1, 0]
    header = "070701" + "".join(f"{f:08x}" for f in fields)
    blob = header.encode() + name.encode() + b"\0"
    blob += b"\0" * ((4 - len(blob) % 4) % 4)
    blob += data
    blob += b"\0" * ((4 - len(data) % 4) % 4)
    return blob

parts = []
parts.append(entry(".", 0o040755, b""))
for current, dirs, files in os.walk(root):
    dirs.sort()
    for name in dirs + files:
        path = os.path.join(current, name)
        rel = os.path.relpath(path, root)
        st = os.lstat(path)
        if stat.S_ISDIR(st.st_mode):
            parts.append(entry(rel, 0o040755, b""))
        elif stat.S_ISLNK(st.st_mode):
            parts.append(entry(rel, 0o120777, os.readlink(path).encode()))
        elif stat.S_ISREG(st.st_mode):
            mode = 0o100755 if st.st_mode & 0o111 else 0o100644
            with open(path, "rb") as f:
                parts.append(entry(rel, mode, f.read()))
        else:
            raise SystemExit(f"unsupported file in the initramfs: {path}")

parts.append(entry("TRAILER!!!", 0, b""))
archive = b"".join(parts)
with open(out, "wb") as f:
    with gzip.GzipFile(fileobj=f, mode="wb", compresslevel=9, mtime=0) as gz:
        gz.write(archive)
PY
# ---- manifest ------------------------------------------------------------
mkdir -p "$OUT"
cp "$work/kernel" "$OUT/kernel"
cp "$work/initramfs" "$OUT/initramfs"
python3 - "$here/manifest.json" "$OUT" "$ARCH" "$version" "$BIN" <<'PY'
import hashlib, json, os, sys

template, out, arch, version, worker = sys.argv[1:6]

def digest(path):
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()

manifest = json.load(open(template))
manifest["arch"] = arch
manifest["binary_version"] = version
manifest["kernel"] = {"path": "kernel", "sha256": digest(os.path.join(out, "kernel"))}
manifest["initramfs"] = {"path": "initramfs", "sha256": digest(os.path.join(out, "initramfs"))}
manifest["worker"] = {"sha256": digest(worker)}
json.dump(manifest, open(os.path.join(out, "manifest.json"), "w"), indent=2)
PY

echo "bundle: $OUT"
echo "  arch:           $ARCH"
echo "  binary_version: $version"
echo "  kernel         $(sha256sum "$OUT/kernel" | cut -d' ' -f1)"
echo "  initramfs      $(sha256sum "$OUT/initramfs" | cut -d' ' -f1)"
