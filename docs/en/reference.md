# diskedit command reference

Generated from the command schema in `src/cli.rs` by `src/docs.rs`.
Regenerate with `DISKEDIT_WRITE_DOCS=1 cargo test --bin DiskEdit docs_are_generated_in_sync`;
CI regenerates it and fails when the working tree differs. Do not edit by hand.

Write commands on a block device that exposes no WWN, EUI or repeatable serial
(nbd, rbd, dm, md) require `--identity NAME`; the same name is needed to resume the
record and to release it. Images and devices with an identity of their own reject
the flag.

## Commands

### `info`

show partition table and per-partition filesystem identification; --grow appends a relocation preview for :N

```text
diskedit info <TARGET> [--sector-size N]
diskedit info <TARGET>:N --grow [--sector-size N]
```

| property | value |
|---|---|
| positionals | `TARGET` / `TARGET` |
| options | `--sector-size`, `--grow` |
| confirmation | none |
| journal | read-only |
| capabilities | readonly |

```text
  Show partition table and per-partition filesystem identification
  (an LVM PV shows up in the fs field as lvm2_pv; PV space takes effect
  through `resize --grow-lv`). Read-only.
  With --grow the same document gains a `preview` key for growing partition :N
  to the last usable LBA; it only reads — nothing is written:
    grow_part     the partition being grown
    new_end_lba   its end LBA after the growth
    sector_size   logical block size the LBAs are counted in
    moves         array of {num, first_lba, last_lba, delta_lba, bytes, swap};
                  swap marks an area recreated in place instead of copied
  stdout is JSON in both outcomes; a failure adds `error` (a string) and keeps
  the layout fields it could not determine at null. Keys:
    label         "gpt" | "mbr" | "none"
    damaged       bool — the table needs repair or cannot be read as-is
    sector_size   logical block size in bytes
    size_bytes    target size in bytes
    disk_guid     GPT disk GUID, null when the table has none
    last_part     the partition number `:last` resolves to; absent when none
    partitions    array of {num, first_lba, last_lba, size_bytes, type, fs, name};
                  type is a GPT GUID or an MBR "0xXX", fs is the detected name
                  (or "container" / "error"), name is null on MBR
```

### `inspect`

show the recovery record beside a target (read-only)

```text
diskedit inspect <TARGET|PATH>
```

| property | value |
|---|---|
| positionals | `TARGET` |
| options | none |
| confirmation | none |
| journal | read-only |
| capabilities | readonly, companion file |

```text
target:
  TARGET is a whole image or block device; a `:N` partition suffix is refused.
  `--sector-size` is not accepted: inspect reports the record, not the disk geometry.

path:
  PATH is dispatched by structure: a direct entry inside a recovery-record
  directory (`<image_state_root>/images/<key-hash>/...`,
  `<image>.diskedit/...` or `<state_root>/recovery/<identity-hash>/...`,
  excluding `record.lock`) is itself decoded as evidence. Entries in the
  quarantine area (`.abandoned[.N]`) are marked as such and do not count as
  active records. Any other path is treated as a TARGET.

output:
  the evidence path, the record fields (transaction, plan, operation, progress,
  commit and retention states), or the structural failure that prevents a verdict.
  `identity check: ForeignCheckUnavailable` means the platform gave no stable
  target identity; the record is not compared.

exit status:
  0  a readable result: no record, a complete record, an unfinished record (the
     output names the rollback / set-aside commands), or an identity check that
     cannot be made
  3  truncated bytes, CRC or magic mismatch, identity mismatch, a non-regular
     entry, or the anchor directory, the record, or the target cannot be read
  6  this platform does not provide the command set
  `Unverified` records are annotated and keep their verdict's exit status
```

### `records list`

list recovery records as JSON (omit TARGET: everything under <state_root>)

```text
diskedit records list [TARGET]
```

| property | value |
|---|---|
| positionals | `TARGET` (optional) |
| options | none |
| confirmation | none |
| journal | read-only |
| capabilities | readonly, companion file |

```text
  List recovery records as JSON. Only decodable records are listed; an entry in
  an unknown format is left out (use `records scan` to see it). Quarantine
  entries (`.abandoned[.N]`) are listed and marked, but they are not part of
  the active record set.

  There is no index yet: every row carries "registered": false, and records are
  enumerated in memory. Record directories that are not addressed by TARGET are
  not searched — pass TARGET to see one, or use `records scan` with the
  directory that holds the images or with the image state root.
```

### `records verify`

re-check one record by its 32-hex id

```text
diskedit records verify <TARGET> <ID>
```

| property | value |
|---|---|
| positionals | `TARGET`, `ID` |
| options | none |
| confirmation | none |
| journal | read-only |
| capabilities | readonly, companion file |

```text
  Locate the record whose id is ID (32 lowercase hex digits, as printed by
  `records list`) on TARGET, re-probe the identity of TARGET, and print the
  verdict as JSON.

  Exit status:
    0  exactly one record, and the identity check did not fail
    3  the id was not found (A4), or it exists in two or more places, or the
       single record fails its identity / backend / structure check, or the
       record, its directory, or the target could not be read

  A failed lookup prints a summary line with "hits" (exact matches) and
  "unverified" (entries whose id could not be decoded) as counts. "records"
  appears only in row documents, where it is always an array.
```

### `records scan`

enumerate records under a container root (<state_root>/recovery, or an image directory)

```text
diskedit records scan <ROOT>
```

| property | value |
|---|---|
| positionals | `ROOT` |
| options | none |
| confirmation | none |
| journal | read-only |
| capabilities | readonly, companion file |

```text
  Enumerate records under a container root and print them as JSON.

  ROOT is one of:
    <state_root>/recovery      every direct child is an identity-hash record directory
    <image_state_root>         its `images` child holds one record directory per image key
    <image_state_root>/images  every direct child is an image record directory
    <image directory>          every direct child named <name>.diskedit is a record directory

  A subdirectory of <state_root> other than `recovery` is refused: a record
  directory is not a container. Scanning is bounded to depth 3, 64 MiB per file,
  512 MiB in total and 30 seconds. Exceeding a bound reports an incomplete
  result and exits 3; no partial list is printed.

  There is no index yet: every row carries "registered": false.
```

### `resize`

resize a partition and its filesystem; --end / --grow-to-end repartition without moving data

```text
diskedit resize <TARGET>[:N] <SIZE> [--allow-move] [--chunk N] [--grow-lv] [--lv NAME] [--no-fs] --yes
diskedit resize <TARGET>:N --end LBA [--align A] [--no-fs] --yes
diskedit resize <TARGET>:N --grow-to-end [--align A] [--no-fs] --yes
```

| property | value |
|---|---|
| positionals | `TARGET`, `SIZE` / `TARGET` / `TARGET` |
| options | `--no-fs`, `--identity`, `--sector-size`, `--yes`, `--allow-move`, `--chunk`, `--dry-run`, `--grow-lv`, `--lv`, `--end`, `--align`, `--grow-to-end` |
| confirmation | required (`--yes`) |
| journal | writes, recovery record |
| capabilities | target data, companion file, external tool, device state |

```text
  Resize partition and its filesystem, partition -> PV -> LV -> filesystem as
  required. The online/offline method is chosen automatically. GPT and MBR
  primary partitions; superfloppy (whole-disk FS) omits :N — grow only, never
  shrinks.

  Examples:
    diskedit resize /dev/sdb:1 +2G --yes
    diskedit resize disk.img 10G --allow-move --chunk 4 --yes

  SIZE:
    10G       set size to 10 GiB (units k/m/g/t, 1024 base; plain bytes
              when no unit)
    +2G       grow by 2 GiB
    -500M     shrink by 500 MiB (GPT and MBR primary partitions; the
              filesystem shrinks first, offline)
    +10%      grow by 10% of current size (rounded down to 1MiB)
    -10%      shrink by 10% of current size
    grow      grow into the contiguous free space to the right
    Sizes may carry a fraction: 1.5G = 1610612736 bytes, +1.5% likewise. A
    size that does not land on a sector boundary is truncated, and the
    truncation is reported

  DRY RUN:
    --dry-run prints the computed ranges, the filesystem step and any
    relocation plan, then stops before the first write; no record is
    created and --yes is not required. The numbers come from the same
    resolution and planning code the write path uses.

  IN PLACE (--end LBA | --grow-to-end):
    repartition the end without moving the partition; the start stays at the
    current starting LBA (relocating it is `diskedit move`). --grow-to-end
    pins end at last_usable_lba (fill semantics). An end below the current
    end shrinks the partition together with its filesystem (offline); a
    shrink that gets interrupted cannot be rolled back with `recover`.

  Constraints:
    shrink    a SIZE below the current size shrinks on GPT and on MBR
              primary partitions (1..4; extended containers and logical
              partitions are out of scope); an interrupted shrink resumes when
              re-run with the same absolute size. A shrink on a mounted
              filesystem is refused — unmount first
    --no-fs   refused on an LVM PV (the PV chain has no filesystem step of
              its own) and on a shrink. A swap partition relocated as a
              blocker is rebuilt regardless: that step belongs to the
              relocation, not to the filesystem
    MBR       uses the same recovery record as GPT: an interrupted grow or
              shrink resumes when re-run with the same absolute size; `recover`
              rolls the recorded table write back instead
    record    a relocation record written before requests were bound to
              their digest carries none: it cannot be tied to this request,
              so re-running `resize --allow-move` is refused. Inspect it with
              `inspect`, then release it with `recover --release`
    online    writes go through the tools (sfdisk / the FS's own resize),
              not this tool's record: nothing to resume, and `recover`
              cannot roll it back

  Read-back:
    after a successful grow the filesystem size is read back and must reach
    the end of the new range within one placement unit for ext2/3/4
    (dumpe2fs: one block group — resize2fs leaves a tail it cannot fit the
    group's metadata into), ntfs (ntfsresize: one cluster) and btrfs (btrfs
    inspect-internal dump-super: one sector size). An ext grow may stop up to
    one block group short: resize2fs drops the tail it cannot fit the group's
    metadata into, and the read-back counts that as reaching the end. f2fs,
    xfs and vfat have no read-back in this build: the
    tool's success is recorded as an unverified result, the run prints a
    pending item naming a size check, and the exit status is 5. The
    partition step is committed either way; compare `df -B1` on the
    mountpoint with the range end to settle it.

  Related commands: this is the single entry point for a partition — it plans
  and applies in one run. `info --grow` previews a growth without writing;
  `move` relocates a partition and resumes an interrupted job of its own. A
  resize interrupted after its partition step (the LVM finish chain still
  pending) is resumed by re-running `resize`.
```

### `move`

move a partition, data follows (chunked copy, resumable)

```text
diskedit move <TARGET>:N --start <LBA|end|align> [--chunk N] --yes [--align A]
```

| property | value |
|---|---|
| positionals | `TARGET` |
| options | `--identity`, `--sector-size`, `--yes`, `--align`, `--chunk`, `--start` |
| confirmation | required (`--yes`) |
| journal | writes, recovery record |
| capabilities | target data, companion file, external tool, device state |

```text
  Move a partition, data follows (chunked copy, resumable from its recovery record).

  LOCATION:
    <LBA>     new start LBA (aligned per --align; default none = the LBA as given)
    end       tail-pack to the last possible position
    align     the next 1 MiB boundary at or after the current start (GPT only;
              --align does not apply). Adjacent partitions are not moved, so a
              start that would land on one is refused as a range overlap.

  A swap partition is relocated by committing its entry and rebuilding the area
  at the new position (mkswap); its contents are not copied.

  CONFIRMATION: required. The destination range is overwritten as-is and an
  interrupted relocation cannot be rolled back with `recover`; pass --yes.
```

### `copy`

copy a partition to a new location; the source is untouched

```text
diskedit copy <TARGET>:N --start <LBA|end> [--chunk N] --yes [--align A] [--name S] [--keep-partuuid]
```

| property | value |
|---|---|
| positionals | `TARGET` |
| options | `--identity`, `--sector-size`, `--yes`, `--align`, `--chunk`, `--keep-partuuid`, `--name`, `--start` |
| confirmation | required (`--yes`) |
| journal | writes, recovery record |
| capabilities | target data, companion file, device state |

```text
  Byte-wise copy a partition to a new location; the source is untouched.
  --start end packs the copy against the end of the usable range.
  No resume: an interrupted copy cannot pick up where it left off. It leaves
  a non-rollbackable journal on the target, so write commands refuse until
  that is released with `diskedit recover --release` — re-run then to rewrite the
  destination from the beginning.

  PARTUUID: the copy gets a freshly generated v4 GUID, so the two
  partitions stay distinguishable. --keep-partuuid reuses the source
  GUID instead — mount-by-PARTUUID then resolves to either partition.

  CONFIRMATION: required. The destination range is overwritten with the
  source bytes and the journal records the write as non-rollbackable —
  `recover` cannot restore what was there before; pass --yes.
```

### `delete`

delete a partition entry (the data area is not wiped)

```text
diskedit delete <TARGET>:N --yes
```

| property | value |
|---|---|
| positionals | `TARGET` |
| options | `--identity`, `--sector-size`, `--yes` |
| confirmation | required (`--yes`) |
| journal | writes, recovery record |
| capabilities | target data, companion file, device state |

```text
  Delete a partition entry (the data area is not wiped).
```

### `set`

set a partition property (name / label / uuid / flag / type)

```text
diskedit set <TARGET>:N name S --yes
diskedit set <TARGET>:N label S --yes
diskedit set <TARGET>:N uuid [U] --yes [--random]
diskedit set <TARGET>:N flag F on|off --yes
diskedit set <TARGET>:N type GUID|0xXX --yes
```

| property | value |
|---|---|
| positionals | `TARGET`, `PROPERTY`, `VALUE` / `TARGET`, `PROPERTY`, `VALUE` / `TARGET`, `PROPERTY`, `VALUE` (optional) / `TARGET`, `PROPERTY`, `VALUE`, `STATE` / `TARGET`, `PROPERTY`, `VALUE` |
| options | `--identity`, `--random`, `--sector-size`, `--yes` |
| confirmation | required (`--yes`) |
| journal | writes, recovery record |
| capabilities | target data, companion file, external tool, device state |

```text
  Set a partition property.
    name          GPT partition name
    type          GPT: partition type GUID (hyphens optional, e.g.
                  C12A7328-F81F-11D2-BA4B-00A0C93EC93B); MBR: type byte (0xXX)
    label / uuid  filesystem label / UUID (FS-aware). --random asks the target
                  to generate a new value (the literal value `random` counts
                  as --random). The tool form differs by filesystem: ext
                  tune2fs -U random, xfs xfs_admin -U generate, btrfs
                  btrfstune -u; swap has no generate form; ntfs's serial is
                  not the Windows volume UUID
    flag          GPT: esp|boot|legacy|legacy_boot|hidden|required — esp/boot on
                  switches the type GUID to EFI System Partition; off switches
                  it back only when the type is currently ESP (falling back to
                  Microsoft basic data for FAT/NTFS content, Linux filesystem
                  data otherwise; the previous type GUID is not preserved).
                  Other type GUIDs are left untouched. MBR: boot|hidden
```

### `check`

check filesystem consistency (no repair; f2fs writes metadata on a SIT mismatch)

```text
diskedit check <TARGET>:N
```

| property | value |
|---|---|
| positionals | `TARGET` |
| options | `--sector-size` |
| confirmation | none |
| journal | read-only |
| capabilities | readonly, external tool |

```text
  Check filesystem consistency without repairing. The tool depends on the
  type: ext e2fsck -n, ntfs ntfsfix -n, xfs xfs_repair -n, btrfs check,
  vfat/exfat fsck -n, f2fs fsck.f2fs. A non-zero tool exit is a failed check:
  exit 3 for the read-only checkers; f2fs can have written, so its failure is
  exit 5. It is not strictly read-only: when a SIT segment type disagrees with
  its recorded type, fsck.f2fs rewrites global metadata even without -f, and
  --dry-run does not stop it.
```

### `mkfs`

create a filesystem (destroys all data on the partition)

```text
diskedit mkfs <TARGET>:N <FS> --yes [--label S]
```

| property | value |
|---|---|
| positionals | `TARGET`, `FS` |
| options | `--identity`, `--sector-size`, `--yes`, `--label` |
| confirmation | required (`--yes`) |
| journal | writes, recovery record |
| capabilities | companion file, external tool, device state |

```text
  Create a filesystem. Destroys all data on the partition; stale signatures
  are erased first. Supported: ext2/3/4, xfs, btrfs, f2fs, vfat, exfat,
  ntfs, swap.
```

### `resizefs`

resize a filesystem only — the partition entry stays put

```text
diskedit resizefs <TARGET>:N [BYTES] --yes
diskedit resizefs <MOUNTPOINT> [BYTES] --yes
```

| property | value |
|---|---|
| positionals | `TARGET`, `BYTES` (optional) / `TARGET`, `BYTES` (optional) |
| options | `--identity`, `--sector-size`, `--yes` |
| confirmation | required (`--yes`) |
| journal | writes, recovery record |
| capabilities | companion file, external tool, device state |

```text
  Resize a filesystem. The form picks the mode: <TARGET>:N is offline and grows
  the FS into its partition; a mountpoint is online and operates on the mounted
  partition. No growable filesystem in the region is refused: an LVM PV takes
  its space through `resize --grow-lv`, and an unidentified type has no action
  to take. The OpenWrt overlay layer before its first mount needs nothing here
  (exit 0): fstools creates it at first mount, filling the trailing region.

  Online form: the partition table is never changed; the filesystem is grown
  inside the current partition (btrfs also shrinks). A size beyond the current
  partition is refused — run `diskedit resize` to enlarge the partition first.
  The target size is absolute (units k/m/g/t, 1024 base; plain bytes when no
  unit; fractions allowed); omitted means grow to fill the partition.

  A grow is read back for ext2/3/4, ntfs and btrfs, and must reach the end of
  the partition within one placement unit (one block group for ext2/3/4, whose
  tail resize2fs may leave unused). f2fs, xfs and vfat have
  no read-back — the tool's success stays unverified, the run prints a pending
  item naming a size check, and the exit status is 5.
```

### `recover`

finish with a recovery record: replay it, or release it without rollback

```text
diskedit recover <TARGET> --yes
diskedit recover <TARGET> --replay-table-only --yes
diskedit recover <TARGET> --release [--discard-replay] --yes
```

| property | value |
|---|---|
| positionals | `TARGET` / `TARGET` / `TARGET` |
| options | `--identity`, `--sector-size`, `--yes`, `--replay-table-only`, `--release`, `--discard-replay` |
| confirmation | required (`--yes`) |
| journal | writes, no recovery record |
| capabilities | target data, companion file, external tool, device state |

```text
  Finish with this target's recovery record. Two outcomes share one command:

  replay    (default) put back the bytes the recorded operation overwrote —
            partition-table entries and the in-place filesystem metadata it
            changed (NTFS backup sector pointer). Relocated data cannot be
            rolled back, and writes made by external FS tools are not undone.
            --replay-table-only replays only the recorded table regions of a
            move whose data has already moved.
  --release drop the recovery files, keeping the writes they describe. This
            does not need the record to be readable, and it does not replay
            anything. When the scene still holds at least one range a replay
            could restore — or the payload is damaged, so the count is
            unknown — the release is refused until --discard-replay is given;
            that flag confirms giving up a replayable range, it is not
            another spelling of --yes.

  Examples:
    diskedit recover /dev/sdb --yes
    diskedit recover /dev/sdb --release --yes
```

### `new`

create a fresh partition table

```text
diskedit new <TARGET> [--table gpt|mbr] --yes [--overwrite-data]
```

| property | value |
|---|---|
| positionals | `TARGET` |
| options | `--identity`, `--sector-size`, `--yes`, `--table`, `--overwrite-data` |
| confirmation | required (`--yes`) |
| journal | writes, recovery record |
| capabilities | target data, companion file, device state |

```text
  Create a fresh partition table; overwrites any existing one. Default gpt.
  Drawing a table writes the entry arrays at the front of the disk and, for
  gpt, a backup copy in the last sectors; for mbr, residual GPT header
  signatures are cleared — bytes an existing partition may hold. Such a
  collision is refused and named; --overwrite-data accepts overwriting those
  bytes (--yes alone only confirms the rebuild).
```

### `add`

add a partition; --start fixes where it begins

```text
diskedit add <TARGET> SIZE [--start LBA] [--align A] [--part-number N] [--name S] [--type-guid G] --yes
```

| property | value |
|---|---|
| positionals | `TARGET`, `SIZE` |
| options | `--identity`, `--sector-size`, `--yes`, `--align`, `--name`, `--part-number`, `--start`, `--type-guid` |
| confirmation | required (`--yes`) |
| journal | writes, recovery record |
| capabilities | target data, companion file, device state |

```text
  Add a partition that holds SIZE. Without --start the entry goes in the first
  1 MiB-aligned free gap that fits; --align changes the alignment (none = the
  LBAs as given). With --start it begins at that LBA as given and is refused
  if it does not fit. SIZE is an absolute number of bytes (K/M/G/T
  suffixes allowed); relative forms (+32M, -32M, 32%) are refused — a new
  partition has no previous size to be relative to. Without --part-number the
  entry takes the first unused slot. On GPT the type defaults to Linux
  filesystem data; on MBR --type-guid takes 0xNN and defaults to 0x83 (Linux).
```

## Global options

| flag | value | description |
|---|---|---|
| `--align` | `A` | placement alignment: mib\|none. add's automatic placement defaults to 1 MiB; an explicit --start (add/move/copy) and the end computed by resize align only when --align is given; move's `--start align` is always 1 MiB |
| `--allow-move` | — | permit relocating the partitions in the way: the plan is printed, then the write proceeds |
| `--chunk` | `MiB` | recovery granularity in MiB, 1..=64; a resumed job keeps the record's value, a new job uses 4 |
| `--discard-replay` | — | confirm giving up the ranges `recover` could still replay |
| `--dry-run` | — | print the plan and stop before the first write; --yes is not required |
| `--end` | `LBA` | new end LBA for `resize` (grow or shrink in place) |
| `--grow-lv` | — | also grow the LV: the one named by --lv, else the only LV |
| `--grow-to-end` | — | pin the end at the last usable LBA (fill semantics) |
| `--grow` | — | append the relocation preview for partition :N |
| `--identity` | `NAME` | name a target that exposes no WWN, EUI or repeatable serial (nbd, rbd, dm, md); resume and release need the same name |
| `--keep-partuuid` | — | reuse the source PARTUUID instead of generating a new one |
| `--label` | `S` | filesystem label (omitted = an empty label, not a default) |
| `--lv` | `NAME` | which LV to grow (implies --grow-lv) |
| `--name` | `S` | name for the entry this command creates |
| `--no-fs` | — | skip the filesystem step of the grown partition |
| `--overwrite-data` | — | accept overwriting bytes an existing partition holds |
| `--part-number` | `N` | 1-based entry slot; it must be empty |
| `--random` | — | ask the target to generate a new value |
| `--release` | — | drop the recovery files, keeping the writes they describe |
| `--replay-table-only` | — | allow `recover` to replay only the recorded table regions of a move whose data has already moved; the data those ranges name is not restored |
| `--sector-size` | `N` | logical sector size override for raw images (power of two, max 65536 bytes — an implementation limit, not a format requirement) |
| `--start` | `LBA\|end\|align` | new start: an LBA, `end` for pack-to-end, or `align` for the next 1 MiB boundary (move only) |
| `--table` | `gpt\|mbr` | table kind: gpt (default) or mbr |
| `--type-guid` | `GUID\|0xNN` | GPT: type GUID text, e.g. 0fc63daf-8483-4772-8e79-3d69d8477de4; MBR: type byte 0xNN |
| `--yes` | — | acknowledge a write before it starts |

## Exit status

| code | meaning |
|---|---|
| 0 | success |
| 1 | unexpected internal error |
| 2 | usage or command syntax error |
| 3 | policy refusal, or a precondition of the request is not met |
| 4 | this run wrote no target bytes and no record entry was created, renamed, moved away or deleted (the records directory and record.lock do not count) |
| 5 | partial, records changed while the target was not fully written, or applied with follow-up steps pending or a stale kernel view (the remedy is printed) |
| 6 | the platform or capability is not supported |