# diskedit command reference

Generated from the command schema in `src/cli.rs` by `src/docs.rs`.
Regenerate with `DISKEDIT_WRITE_DOCS=1 cargo test --bin DiskEdit docs_are_generated_in_sync`;
CI regenerates it and fails when the working tree differs. Do not edit by hand.

## Commands

### `info`

show partition table and per-partition filesystem identification

```text
diskedit info <TARGET> [--sector-size N]
```

| property | value |
|---|---|
| positionals | `TARGET` |
| options | `--sector-size` |
| confirmation | none |
| journal | read-only |
| capabilities | readonly |

```text
  Show partition table and per-partition filesystem identification
  (an LVM PV shows up in the fs field as lvm2_pv; PV space takes effect
  through `resize --grow-lv`). Read-only.
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
diskedit inspect <TARGET>
diskedit inspect <PATH>
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
  directory (`<image>.diskedit/...` or `<state_root>/recovery/<identity-hash>/...`,
  excluding `record.lock`) is itself decoded as evidence. Entries in the
  quarantine area (`.abandoned[.N]`, `.torn[.N]`) are marked as such and do not
  count as active records. Any other path is treated as a TARGET.

output:
  the evidence path, the record fields (transaction, plan, operation, progress,
  commit and retention states), or the structural failure that prevents a verdict.
  `identity check: ForeignCheckUnavailable` means the platform gave no stable
  target identity; the record is not compared.

exit status:
  0  no record, a complete record that may be cleaned up, or an identity check
     that cannot be made
  3  torn/truncated bytes, CRC or magic mismatch, identity mismatch, or a non-regular entry
  4  the anchor directory or the record cannot be read
  5  the record is unfinished; the output names the rollback / set-aside commands
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
| positionals | `TARGET` |
| options | none |
| confirmation | none |
| journal | read-only |
| capabilities | readonly, companion file |

```text
  List recovery records as JSON. Only decodable records are listed; an entry in
  an unknown format is left out (use `records scan` to see it). Quarantine
  entries (`.abandoned[.N]`, `.torn[.N]`) are listed and marked, but they are not
  part of the active record set.

  There is no index yet: every row carries "registered": false, and records are
  enumerated in memory. Sidecars that are not addressed by TARGET are not
  searched — pass TARGET to see one, or use `records scan` with the directory
  that holds the images.
```

### `records verify`

re-check one record by its 32-hex id

```text
diskedit records verify <ID> <TARGET>
```

| property | value |
|---|---|
| positionals | `ID`, `TARGET` |
| options | none |
| confirmation | none |
| journal | read-only |
| capabilities | readonly, companion file |

```text
  Locate the record whose id is ID (32 lowercase hex digits, as printed by
  `records list`), re-probe the identity of TARGET, and print the verdict as
  JSON.

  Exit status:
    0  exactly one record, and the identity check did not RunOutcome
    3  the id was not found (A4), or it exists in two or more places, or the
       single record fails its identity / backend / structure check
    4  the record, its directory, or the target could not be read
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
    <state_root>/recovery   every direct child is an identity-hash record directory
    <image directory>       every direct child named <name>.diskedit is a record directory

  A subdirectory of <state_root> other than `recovery` is refused: a record
  directory is not a container. Scanning is bounded to depth 3, 64 MiB per file,
  512 MiB in total and 30 seconds. Exceeding a bound reports an incomplete
  result and exits 4; no partial list is printed.

  There is no index yet: every row carries "registered": false.
```

### `resize`

resize partition + FS (auto online/offline)

```text
diskedit resize <TARGET>[:N] <SIZE> [OPTIONS]
```

| property | value |
|---|---|
| positionals | `TARGET`, `SIZE` |
| options | `--allow-move`, `--chunk`, `--grow-lv`, `--lv`, `--no-fs`, `--sector-size`, `--yes` |
| confirmation | required (`--yes`) |
| journal | writes, undo journal |
| capabilities | target data, companion file, external tool, device state |

```text
  Resize partition and its filesystem, partition -> PV -> LV -> filesystem as
  required. The online/offline method is chosen automatically. GPT and MBR
  primary partitions; superfloppy (whole-disk FS) omits :N — grow only, never
  shrinks.

  Examples:
    diskedit resize /dev/sdb1 +2G --yes
    diskedit resize disk.img 10G --allow-move --chunk 4 --yes

  SIZE:
    10G       set size to 10 GiB (units k/m/g/t, 1024 base; plain bytes
              when no unit)
    +2G       grow by 2 GiB
    +10%      grow by 10% of current size (rounded down to 1MiB)
    grow      grow into the contiguous free space to the right
    Sizes may carry a fraction: 1.5G = 1610612736 bytes, +1.5% likewise. A
    size that does not land on a sector boundary is truncated, and the
    truncation is reported

  Constraints:
    shrink    refused: this command only grows. Shrink with `resize-part`
              --end: it shrinks the filesystem first (offline), then the
              partition, and an interrupted shrink resumes from its record
    --no-fs   refused on an LVM PV (the PV chain has no filesystem step of
              its own). A swap partition relocated as a blocker is rebuilt
              regardless: that step belongs to the relocation, not to the
              filesystem
    MBR       uses the same recovery record as GPT: an interrupted grow is
              resumed by re-running the same command; `undo` rolls the
              recorded table write back instead
    record    a relocation record written before requests were bound to
              their digest carries none: it cannot be tied to this request,
              so re-running `resize --allow-move` is refused. Inspect it with
              `inspect`, then release it with `undo`
    online    writes go through the tools (sfdisk / the FS's own resize),
              not this tool's record: nothing to resume, and `undo`
              cannot roll it back

  Related commands: this is the single entry point — it plans and applies in
  one run. To inspect the plan first, use `plan`; `resize-part` and `move` are
  the low-level single-partition commands, and they resume an interrupted job
  of their own. A resize interrupted after its partition step (the LVM finish
  chain still pending) is resumed by re-running `resize`.
```

### `move`

move a partition, data follows (chunked copy, resumable)

```text
diskedit move <TARGET>:N --start <LBA|end> [--chunk N] --yes [--align A]
```

| property | value |
|---|---|
| positionals | `TARGET` |
| options | `--align`, `--chunk`, `--no-fs`, `--sector-size`, `--start`, `--yes` |
| confirmation | required (`--yes`) |
| journal | writes, undo journal |
| capabilities | target data, companion file, external tool, device state |

```text
  Move a partition, data follows (chunked copy, resumable from its recovery record).

  LOCATION:
    <LBA>     new start LBA (aligned per --align; default none = the LBA as given)
    end       tail-pack to the last possible position

  --no-fs has no effect: a move keeps the partition's size, so there is no
  filesystem step to skip.

  A swap partition is relocated by committing its entry and rebuilding the area
  at the new position (mkswap); its contents are not copied.

  CONFIRMATION: required. The destination range is overwritten as-is and an
  interrupted relocation cannot be rolled back with `undo`; pass --yes.
```

### `copy`

copy a partition to a new location; the source is untouched

```text
diskedit copy <TARGET>:N --start <LBA|end> [--chunk N] --yes [--align A] [--name S] [--keep-partuuid]
```

| property | value |
|---|---|
| positionals | `TARGET` |
| options | `--align`, `--chunk`, `--keep-partuuid`, `--name`, `--sector-size`, `--start`, `--yes` |
| confirmation | required (`--yes`) |
| journal | writes, undo journal |
| capabilities | target data, companion file, device state |

```text
  Byte-wise copy a partition to a new location; the source is untouched.
  --start end packs the copy against the end of the usable range.
  No resume: an interrupted copy cannot pick up where it left off. It leaves
  a non-rollbackable journal on the target, so write commands refuse until
  that is released with `diskedit abandon` — re-run then to rewrite the
  destination from the beginning.

  PARTUUID: the copy gets a freshly generated v4 GUID, so the two
  partitions stay distinguishable. --keep-partuuid reuses the source
  GUID instead — mount-by-PARTUUID then resolves to either partition.

  CONFIRMATION: required. The destination range is overwritten with the
  source bytes and the journal records the write as non-rollbackable —
  `undo` cannot restore what was there before; pass --yes.
```

### `delete`

delete a partition entry (the data area is not wiped)

```text
diskedit delete <TARGET>:N --yes
```

| property | value |
|---|---|
| positionals | `TARGET` |
| options | `--sector-size`, `--yes` |
| confirmation | required (`--yes`) |
| journal | writes, undo journal |
| capabilities | target data, companion file, device state |

```text
  Delete a partition entry (the data area is not wiped).
```

### `set`

set a partition property (name / label / uuid / flag / type)

```text
diskedit set <TARGET>:N name S | label S | uuid U | flag F on|off --yes
diskedit set <TARGET>:N type GUID | type 0xXX --yes
```

| property | value |
|---|---|
| positionals | `TARGET`, `PROPERTY`, `VALUE`, `STATE` |
| options | `--random`, `--sector-size`, `--yes` |
| confirmation | required (`--yes`) |
| journal | writes, undo journal |
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

check filesystem consistency without writing

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
  Check filesystem consistency without writing. The tool depends on the type
  and always runs in its read-only form: ext e2fsck -n, ntfs ntfsfix -n,
  xfs xfs_repair -n, btrfs check, vfat/exfat fsck -n. A non-zero tool exit
  is a failed check (exit 3).
```

### `mkfs`

create a filesystem (destroys all data on the partition)

```text
diskedit mkfs <TARGET>:N <FS> --yes [--label S]
```

| property | value |
|---|---|
| positionals | `TARGET`, `FS` |
| options | `--label`, `--sector-size`, `--yes` |
| confirmation | required (`--yes`) |
| journal | writes, undo journal |
| capabilities | companion file, external tool, device state |

```text
  Create a filesystem. Destroys all data on the partition; stale signatures
  are erased first. Supported: ext2/3/4, xfs, btrfs, f2fs, vfat, exfat,
  ntfs, swap.
```

### `resizefs`

resize a filesystem only — the partition entry stays put

```text
diskedit resizefs <TARGET>:N --yes
diskedit resizefs <MOUNTPOINT> --yes [BYTES]
```

| property | value |
|---|---|
| positionals | `TARGET`, `BYTES` |
| options | `--sector-size`, `--yes` |
| confirmation | required (`--yes`) |
| journal | writes, undo journal |
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
```

### `undo`

roll back the writes this tool completed (replays its record)

```text
diskedit undo <TARGET> --yes
```

| property | value |
|---|---|
| positionals | `TARGET` |
| options | `--sector-size`, `--yes` |
| confirmation | required (`--yes`) |
| journal | writes, no undo journal |
| capabilities | target data, companion file, external tool, device state |

```text
  Roll back this target's recovery record: put back the bytes the recorded
  operation overwrote — partition-table entries and the in-place filesystem
  metadata it changed (NTFS backup sector pointer). Relocated data cannot be
  rolled back, and writes made by external FS tools are not undone.

  A legacy undo journal is not a recovery record; this build does not replay,
  migrate, or delete it.

  Example: diskedit undo /dev/sdb --yes
```

### `abandon`

drop an unfinished operation's recovery files; no rollback

```text
diskedit abandon <TARGET>
```

| property | value |
|---|---|
| positionals | `TARGET` |
| options | `--discard-replay` |
| confirmation | none |
| journal | writes, no undo journal |
| capabilities | companion file |

```text
  Give up on recovering an unfinished operation: move its recovery record into
  the record directory's quarantine area (.abandoned[.N]) so the target can be
  written again. No byte of the target itself is touched.

  Unlike `undo` this does not need the record to be readable — a damaged or
  unreadable file can still be released. A record that is readable but was
  recorded for another target is refused: renaming it would destroy that
  target's rollback evidence. The ability to roll back is lost.

  A legacy undo journal or an old relocation checkpoint is not a recovery
  record; this command leaves such files in place.

  When the scene still holds at least one range `undo` could replay — or the
  payload is damaged, so the count is unknown — the command refuses until
  --discard-replay is given. A scene with nothing left to replay needs no
  flag.
```

### `new`

create a fresh partition table

```text
diskedit new <TARGET> [--table gpt|mbr] --yes [--overwrite-data]
```

| property | value |
|---|---|
| positionals | `TARGET` |
| options | `--overwrite-data`, `--sector-size`, `--table`, `--yes` |
| confirmation | required (`--yes`) |
| journal | writes, undo journal |
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
| options | `--align`, `--name`, `--part-number`, `--sector-size`, `--start`, `--type-guid`, `--yes` |
| confirmation | required (`--yes`) |
| journal | writes, undo journal |
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

### `resize-part`

repartition one partition in place

```text
diskedit resize-part <TARGET>:N (--end LBA | --grow-to-end) [--align A] --yes
```

| property | value |
|---|---|
| positionals | `TARGET` |
| options | `--align`, `--end`, `--grow-to-end`, `--sector-size`, `--yes` |
| confirmation | required (`--yes`) |
| journal | writes, undo journal |
| capabilities | target data, companion file, external tool, device state |

```text
  Low-level grow/shrink in place: repartition the end without moving the
  partition. The start stays at the partition's current starting LBA;
  relocating it is `diskedit move`. --grow-to-end pins end at
  last_usable_lba (fill semantics).
  A grow also resizes the filesystem inside.

  CONFIRMATION: --yes is required. An end below the current end shrinks the
  partition together with its filesystem; a shrink that gets interrupted
  cannot be rolled back with `undo`.
```

### `plan`

read-only report: geometry, table, FS and usage — the same fields as `info`; --grow appends a relocation preview

```text
diskedit plan <TARGET>[:N] [--grow] [--sector-size N]
```

| property | value |
|---|---|
| positionals | `TARGET` |
| options | `--grow`, `--sector-size` |
| confirmation | none |
| journal | read-only |
| capabilities | readonly |

```text
  Read-only report on a target: geometry, partition table, filesystems and
  space-usage conclusions. The fields are the ones `info` prints; `plan` opens
  no recovery path and probes no write directory.
  With --grow the same document gains a `preview` key for growing partition :N
  to the last usable LBA:
    grow_part     the partition being grown
    new_end_lba   its end LBA after the growth
    sector_size   logical block size the LBAs are counted in
    moves         array of {num, first_lba, last_lba, delta_lba, bytes, swap};
                  swap marks an area recreated in place instead of copied
  The preview only reads: nothing is written and no reusable plan is produced.
```

## Global options

| flag | value | description |
|---|---|---|
| `--align` | `A` | placement alignment: mib\|none. add's automatic placement defaults to 1 MiB; an explicit --start and move/copy/resize-part align only when --align is given |
| `--allow-move` | — | permit relocating the partitions in the way: the plan is printed, then the write proceeds |
| `--chunk` | `MiB` | recovery granularity in MiB, 1..=64 (default 4); an interrupted run must repeat the same value |
| `--discard-replay` | — | confirm giving up the ranges `undo` could still replay |
| `--end` | `LBA` | new end LBA for resize-part (grow or shrink in place) |
| `--grow-lv` | — | also grow the LV: the one named by --lv, else the only LV |
| `--grow-to-end` | — | pin the end at the last usable LBA (fill semantics) |
| `--grow` | — | append the relocation preview for partition :N |
| `--keep-partuuid` | — | reuse the source PARTUUID instead of generating a new one |
| `--label` | `S` | filesystem label (omitted = an empty label, not a default) |
| `--lv` | `NAME` | which LV to grow (implies --grow-lv) |
| `--name` | `S` | name for the entry this command creates |
| `--no-fs` | — | skip the filesystem step of the grown partition (a move has no such step) |
| `--overwrite-data` | — | accept overwriting bytes an existing partition holds |
| `--part-number` | `N` | 1-based entry slot; it must be empty |
| `--random` | — | ask the target to generate a new value |
| `--sector-size` | `N` | logical sector size override for raw images (power of two, max 65536 bytes — an implementation limit, not a format requirement) |
| `--start` | `LBA\|end` | new start: an LBA, or `end` for pack-to-end |
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
| 4 | operation failed with no persistent modification |
| 5 | partial, or applied with follow-up steps pending or a stale kernel view (the remedy is printed) |
| 6 | the platform or capability is not supported |