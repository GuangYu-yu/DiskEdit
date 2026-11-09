#!/usr/bin/env bash
# QEMU 引导验证 v2：GRUB(ESP) + 内核(boot 分区，按 label 搜索)
# 修正 v1 的两个问题：(1) grub 配置与 prefix 不一致导致 GRUB 进交互 shell；
#                      (2) QEMU 等 stdin 造成"卡住"
source "$(dirname "$0")/lib.sh"
require_fault_bin

img T t23.img
ESP=$WSL_ROOT/mnt/t23esp
BOOT=$WSL_ROOT/mnt/t23boot
LOG=$WSL_ROOT/t23qlog
KERNEL=/var/tmp/vmlinuz-virt
INITRD=/var/tmp/initramfs-virt
OVMF=/usr/share/ovmf/OVMF.fd

cleanup_hook() {
  rmdir "$ESP" "$BOOT" 2>/dev/null
}

missing=0
for f in "$KERNEL" "$INITRD" "$OVMF"; do
  [ -r "$f" ] || { echo "引导依赖缺失：$f"; missing=1; }
done
for c in qemu-system-x86_64 grub-install mkfs.vfat; do
  command -v "$c" >/dev/null || { echo "引导依赖缺失：$c"; missing=1; }
done
[ -d /usr/lib/grub/x86_64-efi ] || { echo "引导依赖缺失：grub x86_64-efi 模块（grub-efi-amd64-bin）"; missing=1; }
[ "$missing" = 1 ] && { echo "SKIP: 依赖不全"; exit 0; }
# 有 /dev/kvm 用硬件加速；否则降级 TCG 纯软件模拟（慢一个数量级，按比例放宽等待）
if [ -c /dev/kvm ] && [ -w /dev/kvm ]; then
  ACCEL=kvm
else
  ACCEL=tcg
  echo "/dev/kvm 不可用，降级 TCG 软件模拟（单次等待放宽 3 倍）"
fi
if [ "$missing" != 0 ]; then
  echo "==== 汇总：无法验证（引导环境不完整） ===="
  exit 1
fi

setup() {
  rm -f "$T"   # 从零布局：旧表残留会触发 --overwrite-data 守卫，重建的引导也无法引导
  truncate -s 2G "$T"
  $B new "$T" --yes >/dev/null
  $B add --yes "$T" 209715200 --start 2048 --name ESP \
      --type-guid C12A7328-F81F-11D2-BA4B-00A0C93EC93B >/dev/null
  $B add --yes "$T" 629145600 --start 411648 --name boot >/dev/null
  $B add --yes "$T" 419430400 --start 1640448 --name root >/dev/null
  local LD=$(lo_attach "$T")
  lo_waitpart "${LD}p3" || { echo "SKIP: ${LD}p3 未就绪"; exit 0; }
  mkfs.vfat -F32 -n ESP "${LD}p1" >/dev/null
  mkfs.ext4 -q -L boot "${LD}p2"
  mkfs.ext4 -q -L root "${LD}p3"
  mount_at "${LD}p1" "$ESP"; mount_at "${LD}p2" "$BOOT"
  # GRUB 的模块与 prefix 都放 ESP（--removable 的约定），内核放 boot 分区
  # 引导安装失败当场退出：坏 ESP 的失败会在后续每个 phase 级联成 BOOT FAIL
  grub-install --target=x86_64-efi --efi-directory="$ESP" --boot-directory="$ESP/boot" --removable \
    || { echo "FATAL: grub-install 失败，无法继续引导验证"; exit 1; }
  mkdir -p "$ESP/boot/grub"
  cat > "$ESP/boot/grub/grub.cfg" <<'EOF'
set timeout=3
set default=0
menuentry "test" {
  search --no-floppy --set=root --label boot
  linux /vmlinuz-virt console=ttyS0
  initrd /initramfs-virt
}
EOF
  cp "$KERNEL" "$BOOT/vmlinuz-virt"
  cp "$INITRD" "$BOOT/initramfs-virt"
  sync; umount "$ESP" "$BOOT" 2>/dev/null; lo_detach "$LD"
}

qemu_boot() { # $1=label  $2=最多等待秒
  local lim=${2:-60}
  [ "$ACCEL" = tcg ] && lim=$((lim * 3))
  setsid timeout "$lim" qemu-system-x86_64 -m 1024 -accel "$ACCEL" -bios "$OVMF" \
      -drive file="$T",format=raw,if=virtio -nographic -no-reboot \
      < /dev/null > "$LOG.$1" 2>&1 &
  local pid=$!
  for _ in $(seq 1 $((lim / 3))); do
    sleep 3
    tr '\r' '\n' < "$LOG.$1" 2>/dev/null | grep -qE 'Linux version [0-9]+\.[0-9]+\.[0-9]+' && break
    kill -0 "$pid" 2>/dev/null || break
  done
  # GRUB 用 \r + ANSI 光标覆盖重绘菜单，未归一化时判据会被污染；
  # 且覆盖序列不含 \n，故只用 -o 取匹配片段本身（要求带版本号，排除菜单文本误匹配）
  tr '\r' '\n' < "$LOG.$1" > "$LOG.$1.clean" 2>/dev/null
  # 清理产物缺失时退回原始日志：空输出会掩盖真正的失败原因
  local src=$LOG.$1.clean
  [ -s "$src" ] || src=$LOG.$1
  local ver
  ver=$(grep -oE 'Linux version [0-9]+\.[0-9]+\.[0-9]+[^ ]*' "$src" | head -1)
  if [ -n "$ver" ]; then
    echo "  OK   BOOT OK [$1]: $ver"
  else
    echo "  BAD  BOOT FAIL [$1] (末尾输出):"
    grep -vE '^[[:space:]]*$' "$src" | tail -4 | sed 's/^/    /'
    rc=1
  fi
  kill "$pid" 2>/dev/null
  wait "$pid" 2>/dev/null
}

echo "########## 1: 基线（搬移前）##########"
setup
$B info "$T" | grep -o '"num":[0-9]*,"first_lba":[0-9]*,"last_lba":[0-9]*'
qemu_boot baseline 60

echo
echo "########## 2: 搬移 boot 分区（内核所在）后启动 ##########"
OUT=$($B move "$T":2 --start 2600000 --chunk 4 --yes 2>&1); exp "$?" 0 "搬移 boot 分区" || echo "$OUT" | head -2
$B info "$T" | grep -o '"num":2,[^}]*}'
qemu_boot after-move 60

echo
echo "########## 3: 搬移 ESP（GRUB 所在）后启动 ##########"
# boot 搬走后其原位置(411648..1640447)空闲，ESP 移入其中一段
OUT=$($B move "$T":1 --start 411648 --chunk 4 --yes 2>&1); exp "$?" 0 "搬移 ESP 分区" || echo "$OUT" | head -2
$B info "$T" | grep -o '"num":1,[^}]*}'
qemu_boot after-esp-move 60

echo
echo "########## 4: 扩容 boot 分区后启动 ##########"
OUT=$($B resize "$T":2 grow --allow-move --yes 2>&1); exp "$?" 0 "扩容 boot 分区" || echo "$OUT" | head -2
$B info "$T" | grep -o '"num":2,[^}]*}'
qemu_boot after-grow 60

echo
echo "########## 5: 搬移中断 → 续传完成 → 启动（端到端）##########"
setup   # 重建可引导盘
# resize_part 路径的确定性崩溃注入（第 20 个 chunk 落盘后 abort）
DISKEDIT_FAULT=chunk:20 $BF move "$T":2 --start 2600000 --chunk 4 --yes >/dev/null 2>&1
exp "$?" 134 "注入中断（SIGABRT）"
OUT=$($B move "$T":2 --start 2600000 --chunk 4 --yes 2>&1); E=$?
RESUMED=$(echo "$OUT" | grep -o 'resuming at entry [0-9]* chunk [0-9]*')
echo "  resume: $RESUMED"
exp "$E" 0 "续传重跑"
[ -n "$RESUMED" ] && echo "  OK   确实从断点续传" \
  || { echo "  BAD  未见 resuming at entry/chunk（未续传而是从头重跑？）"; rc=1; }
$B info "$T" | grep -o '"num":2,[^}]*}'
qemu_boot after-interrupt-resume 60

echo
echo "==== 汇总：$( [ "$rc" = 0 ] && echo 全部符合契约 || echo 有断言失败 ) ===="
exit $rc