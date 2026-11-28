# Behaviour boundaries and limits

README keeps only "what it is and how to use it". This file records the behavioural constraints: capability gate, occupancy and concurrency decisions, target identity, recovery records, resume semantics, external tools and verification scope, plus the option table reconciled against the flag registry. Update it when these behaviours change.

## Platform capability gate

The per-command outcome, the target shapes the gate evaluates and the text printed at the process-level exit are generated in the [capability matrix](capability-matrix.md#runtime-gate); this section states only the order of the checks. The gate runs before geometry computation, lock acquisition and any companion-file creation, and before the impact confirmation gate. The missing-`--yes` usage check is earlier: it runs during argument parsing before dispatch and exits with the usage status. The gate answers one question per command: whether the command's declared effect is `write` and whether this host is listed as able to write. A write is refused with the unsupported status before the target is opened, before the lock file exists and before any record directory is created; the refusal names the host family that can write. A `--dry-run` form keeps the declared effect but writes nothing, so the gate lets it through and the preview still runs its checks.

## Execution placement

Writes have one path: a job is a list of steps (table writes, partition add/delete/resize, copy, move, filesystem creation/grow/shrink/attributes, and kernel-view refresh), and the host process is the only writer of the recovery record, the lock and the output. Where the steps run is a deployment detail decided once per job, before the first write, and never changed between steps.

- **Native** when the host satisfies every capability the job needs: the platform/root requirements of the filesystem layer and each external tool the step list resolves to. These needs come from the same tool matrix the native executor uses.
- **VM** when the host falls short but `DISKEDIT_VM_BUNDLE` points at a carrier bundle that passes verification. The whole plan moves into the guest; the host keeps the record, the lock and the exit code.
- Neither: exit 6 with the missing capability and both remedies (install the tools, or provide a bundle).

What the gate decides is not just a yes/no: the resolved need — platform, root and the exact tool list — drives the placement decision before the first write. A job that moves to the VM because the host lacks a capability is therefore a decided placement rather than a silent fallback, and the refusal path names the same capability when no bundle is available.

The barrier against an unaccounted write has one point: the VM branch of the step executor. Before a step is handed to the guest, the host stores the step's preimage, arms the state machine and only then sends the step; the plan loop reaches the guest exclusively through that single step function, so a step cannot reach the carrier without the record having admitted "the target may already have been modified". The guest never writes the record, the lock or the exit code. The guest-side parameters are likewise not part of this promise: the target is a file descriptor, and the token and session boundary are described below.

A bundle directory holds `manifest.json`, `kernel` and `initramfs`. The manifest declares `schema`, `arch`, `binary_version`, and for each artifact a relative `path` and `sha256`; an optional `worker.sha256` pins the worker image to this executable. The manifest is accepted only when the schema is current, the architecture equals this process's, `binary_version` equals this executable's version, every artifact path stays inside the bundle directory, and every SHA-256 matches. Anything else is refused before QEMU starts — the host does not hand its target to an unidentified guest.

The launcher passes the already-open target through a file descriptor (`-add-fd` plus `/dev/fdset/`), hands it to the block layer with `-blockdev` and `cache.direct=on`, presents it as virtio-blk with the target's logical sector size (512 or 4096), routes the protocol over the serial console, and supplies entropy through virtio-rng. QEMU's own exit status describes how the VM ended; whether a step was applied is answered by the protocol `result` after the guest's own postcondition check, not by that status.

The session boundary, not the token, is what separates the host from a stranger on the same pipe. The one-time `DISKEDIT_WORKER_TOKEN` (read from `DISKEDIT_WORKER_TOKEN`, delivered to the guest through the kernel command line because the guest-side boot script is outside this repository) is checked once, in the `hello` frame. The worker then answers `ready` with a session id drawn from 128 bits of process entropy, and **every later frame must echo that id** (protocol version 3); the host adds it in a single send path, so a new frame kind cannot forget it. The id only travels guest→host on stdout, so a party holding write access to the worker's stdin — the only party the token could not stop — cannot produce a frame the worker accepts. A frame without it ends the session as a protocol error, and the target is not touched.

## Occupancy boundary

Writes to a block device rely on opening the whole disk with `O_EXCL`: when any partition is mounted or used as swap, the kernel refuses the open with EBUSY; a second occupancy gate re-checks before writing (probing each partition on block devices — defence in depth). Image files have no such kernel semantics, so the gate enumerates loop devices in reverse: the `(st_dev, st_ino)` of the already-opened image handle is compared with `(lo_device, lo_inode)` returned by `LOOP_GET_STATUS64`; on a match it checks whether that loop and its partitions are mounted (walking every readable mount namespace), are active swap, or are held by a block-device stack. The decision does not depend on the `loop/backing_file` path, so hard-link aliases, deleted backing paths and mounts in other mount namespaces are all caught. The loop exclusivity this process registers is a process-level name table; every entry is re-checked against the current live mapping when consumed (the name only locates the registration). The occupancy decision matches every loop whose backing has the file's `(st_dev, st_ino)`, so it answers "this file is mapped or held", not which of several stacked mappings on the same filesystem is the one. **Commands that write the table directly** (`new`/`add`/`delete`/`set name|type|flag`/`undo`/`replay`/`release`) require the target to be completely idle: any partition in use would fork the new table from the kernel's view, so the command does not care which partition it edits. `copy` is the exception — it only adds an entry in a free area after an overlap check, changes no existing extent and moves no partition's extent. Move-class commands (`resize` in every form — `--end`, `--grow-to-end` or a size — and `move`) and `mkfs` distinguish by the shape of the write: a write that keeps the start and only extends the end is allowed on a target held only by dm (the grow semantics of LVM PVs and dm-crypt); moving, shrinking and whole-range rewrites are refused. If any enumerated loop node cannot be opened, an ioctl returns an error other than ENXIO, sysfs cannot be read, or `/proc/<pid>/mountinfo` cannot be read for permission reasons (e.g. `hidepid`), the tool refuses to write (exit code 4) as "cannot confirm" — even if that loop has nothing to do with the target; this is a safety-first environment limit, not evidence that the target is in use.

## LVM finish boundary

`pvresize`/`lvextend` for a PV inside an image run through a temporary loop mapping. If that PV is already mapped by another loop and the VG has an active LV, LVM prefers the device view "in use by an LV" over the temporary mapping, so `pvresize` against the temporary mapping has no effect (exit status still 0). The tool reports exit code 5 (stale postcondition view) from its postcondition check and prints the remedy; it does not claim success. The partition-table extension has been committed; the PV, LV and filesystem are unchanged. To finish the LVM chain in that configuration, deactivate the VG, release the user-side loop and re-run. The tool will not raise the `sizelimit` of a loop owned by another stack: when an external loop has the same offset as the request but a different `sizelimit` (higher or lower), it refuses and points at detaching and re-attaching from the layer that owns the mapping (`losetup -d <dev>`, letting the stack map again); the same applies to `--grow-lv` — without that re-attach the owner will not finish the LVM chain for you.

After a successful `--grow-lv` on a PV inside an image, `lvextend -r` leaves a previously inactive LV active, so the temporary loop cannot be detached immediately: `losetup -d` returns success, the kernel marks the loop autoclear, and the mapping is released when the VG is deactivated (`vgchange -an`). This is the lifetime of a temporary mapping, not a leak; after the VG is deactivated, `losetup -a` no longer lists the image.

## Concurrency boundary

The exclusive lock on a target is *advisory*: it constrains only processes that follow the DISKEDIT protocol, and only across their invocations. **It is not a mandatory lock against every writer on the system** and does not stop other programs, kernel-side writes, or paths that bypass this tool from writing the same target; it must not be read as a system-wide exclusivity guarantee. The whole-disk `O_EXCL` and the mount occupancy gate only stop writers within kernel semantics. The loop reverse lookup for images is a point-in-time decision taken before writing: between the check and the actual write, an external process can still create a loop, start swap or mount the image, and this tool's lock does not constrain those processes. A table write reads the table once at parse time and writes that snapshot back at commit; before committing it re-reads the entry array and compares it entry by entry with the snapshot (slots this layer edited may already equal the target value), and refuses to write the table if any entry differs, so changes made by an outside process in the window are not silently overwritten (the repair step uses the same rule, covering add, set and delete plus the whole relocation and resize path). To share a target with other writers, serialise externally. The inode-level second credential is taken only on Unix; non-Unix platforms promise the lock file alone. Hard-link, symbolic-link and junction aliases each take their own lock and have their own scene; concurrent use through aliases is outside the supported boundary (`std::fs::File::try_lock` is a mandatory whole-file lock on Windows and cannot stand in for this credential).

## Device identity boundary

Recovery records live at `<state_root>/images/<key>/record` for every target — images and block devices alike; `<key>` is the first 16 bytes of the target's 32-byte identity fingerprint in lower-case hex, and the lock is `record.lock` in the same directory, so one target has one record and one lock. The fingerprint is sha256 over a class tag and that class's fields: images use `file\0` plus `(st_dev, st_ino, len)` on Unix and `(volume serial, file index, len)` on Windows, so two hard links to one inode share a record directory while replacing or recreating the file at a path yields a new identity — a job interrupted through one hard link is resumable through the other. Block devices use the first available of `wwid`, `device/serial` (the serial only when two reads agree), `dm/uuid` and `md/uuid`, followed by the device length and, for a partition node, its start and size; a partition node and its whole disk therefore each have their own record directory. A loop device, which has none of those UUIDs, uses the backing file's identity plus the mapping offset and size limit and the partition position. When the sysfs topology or the class material cannot be read, the identity is `Unusable`: no record directory, and write commands refuse with 6 — the tool does not fall back to a name built from the caller's path. `--alias NAME` is the identity only for a block device that proves none of its own; it expands the name into the same 32-byte key space with four seeded FNV-1a lanes and is rejected on images and on devices that identify themselves. The state root is `$XDG_STATE_HOME/diskedit` when that variable is set, otherwise `~/.local/state/diskedit` on Unix and `%LOCALAPPDATA%\diskedit` on Windows; a host without a state root has no record directory, and write commands refuse.

## Identity check of a recovery scene

A record states which scene it belongs to in its payload prefix, and every path that resumes, rolls back or replays compares that statement with the current target before writing.

- **Target fingerprint.** The prefix stores the target's 32-byte fingerprint. Recovery requires `Fingerprint::matches`: both sides are `Strong` and the 32 bytes are equal. A record that names another target is refused as needing manual handling, and a session opening an existing record refuses it as belonging to a different device.
- **Length generation.** The prefix stores the generation, an FNV-1a expansion of the target's total byte count with the lowest bit set. Recovery recomputes it from the target's current length; zero or a different value is refused, so an offset recorded against another length is never replayed. Sector size does not enter the comparison: recorded offsets are byte offsets, and only the capacity bounds them.
- **Plan digest.** The prefix stores the 32-byte plan digest. An unfinished record is reused only when that digest equals the current plan's request digest — the digest that stays the same across re-runs — and a record from another plan is refused rather than mixed into this job.
- **Lock-time identity.** The executors probe the target identity again after taking the record lock: a `Strong` observation must match the planned fingerprint, and a plan identified by a declared name stays valid only while the target still cannot prove an identity of its own.

Capacity, mount state and loop mapping have no separate copy in the record: where they participate in identity they are already fields of the fingerprint, so a target that no longer composes the same fingerprint is refused by the fingerprint check itself.

## Record format

A record is a 16-byte header plus one payload: the header holds the magic `DREC`, the payload length, the payload CRC and a header CRC over the first twelve bytes, and there is no separate version field. The payload opens with a fixed prefix — the target's 32-byte fingerprint, the length generation, the 32-byte plan digest and a CRC over that prefix — followed by the frames. The length generation is an FNV-1a expansion of the target's total byte count with the lowest bit set; zero marks a record that carries no length evidence. Recovery compares the stored fingerprint with the current target and recomputes the generation from the target's current length: a zero value or a different value is refused as needing manual handling, so an offset recorded against another length is never replayed. A record whose magic is not `DREC` is reported as a foreign record and is never decoded; commands that would write refuse it with the refused status, and `release` archives it as `record.<N>.abandoned` in the same directory.

## Placement of the recovery record

A recovery record is a regular file at its location (`<state_root>/images/<key>/record`; the lock is `record.lock` in the same directory and holds no record). What sits at that location is answered by one probe: absent, decodable, damaged (partially written or CRC mismatch), an unrecognised format (magic other than `DREC`), not a recovery file (symbolic link / directory / non-regular file), or unreadable. On Linux the path is resolved from the parent-directory anchor with successive `openat(O_NOFOLLOW|O_DIRECTORY)`, and the final component with `O_NOFOLLOW`: a symbolic link at any level, a non-directory, or a non-regular file at the last component all count as "not a recovery file". On other platforms the probe answers with `symlink_metadata` and reads only a regular file: a symbolic link, a directory or any other object at the last component is reported as "not a recovery file". Removal and renaming act on that path entry itself (parent directory fd plus relative name), following no link and changing no link target; atomic replacement is a rename within the same directory. When the entry is not a recovery file, or removal fails, the scene is treated as still present and the tool does not report it clean.

## Behaviour changes in this release

These are deliberate, user-visible changes; a build from before this release behaves differently:

- **Writes are bounded by the target's current length.** A write that would extend a regular target past its length at the moment of the write is refused by the platform, by the carrier protocol against the declared disk size, and by the worker before it reaches the disk; the write channel also re-checks the declared region against the target length and reads every written segment back.
- **A symbolic link is not a record.** A record location that is a symbolic link at any level of the path is reported as "not a recovery file"; the tool never reads, replaces, removes or renames through it.
- **A contradictory backup header is damage.** When a GPT backup header's reverse pointer disagrees with the copy position, the table state is reported as damaged rather than as having no table.
- **Records from another build are foreign.** The record format magic is `DREC`; a record with any other magic is reported as a foreign record and is never read.
- **A write needs a host that can write.** Commands whose declared effect is `write` are refused with the unsupported status on hosts outside the writing family, before the target is opened; read-only entries keep running, and `--dry-run` previews still work.
- **An irreversible step cannot be rolled back.** When a record contains an irreversible step, `undo` refuses to roll the target back; `replay` may still restore only the recorded partition-table regions.

## Resume boundary

Re-running the original command resumes from its recovery record for `new`, `move`, `resize` (every form: `--end`, `--grow-to-end` or a size), `mkfs`, `resize --fs-only` and `set`; GPT and MBR share the same record family, so an interrupted MBR grow is resumed the same way and `undo` rolls the recorded table write back. An online resize (block device with the partition mounted) writes through the tools instead: there is nothing to resume and `undo` cannot roll it back. `add`, `delete` and `copy` have no resume path (`help copy` states `No resume`), so roll back with `undo` or release the scene with `DiskEdit release` before re-running. The shrink step records three states (not shrunk / verified / unverified): when the filesystem size cannot be read back, the job stops at the shrink step and reports an unfinished transaction with exit code 5, and a re-run first reads the actual size and only repeats the shrink after confirming it never happened. A grow is read back for ext2/3/4, ntfs and btrfs; f2fs, xfs and vfat have no read-back, so a successful grow stays unverified: the run prints a pending item naming a size check and exits 5. At finish time the recovery record is deleted only when six conditions all hold: the record is recognisable, its commit state is committed, the identity matches, persistence is confirmed, the postcondition read-back passes, and the table-write step has completed. Otherwise the record is kept and the tool prints `the recovery record was not cleaned up (<reason>) — release it with DiskEdit release`. When the deletion itself fails, the transaction reports exit code 5 rather than success. Relocated data is synced in batches that advance the durable cursor (the sync precedes the record advance, and the cursor never runs ahead of bytes actually on disk); the batch size adapts to commit latency, so after a crash at most 256 MiB of copied data is redone, and block sizes larger than that window degrade to per-block commits. A self-overlapping relocation (displacement smaller than the source range) commits block by block on every path — blocks buffered by batching would reach into the redo range, and the redo would read source data overwritten by that same batch.

## External tools

Looked up only when a step needs them; they need not all be present:

| Purpose | Tools |
|---|---|
| kernel sync / online table writes | `sfdisk`, `partx` |
| loops and mounts | `losetup`, `udevadm`, `mount`, `umount` |
| ext2/3/4 | `mke2fs`, `e2fsck`, `resize2fs`, `dumpe2fs`, `tune2fs` |
| xfs | `mkfs.xfs`, `xfs_growfs`, `xfs_repair`, `xfs_admin` |
| btrfs | `mkfs.btrfs`, `btrfs`, `btrfstune` |
| ntfs | `mkfs.ntfs`, `ntfsresize`, `ntfsfix`, `ntfslabel` |
| f2fs | `mkfs.f2fs`, `fsck.f2fs`, `resize.f2fs` |
| vfat | `mkfs.vfat`, `fsck.vfat`, `fatresize`, `fatlabel` |
| exfat | `mkfs.exfat`, `fsck.exfat`, `exfatlabel` |
| swap | `mkswap`, `swaplabel` |
| LVM | `pvs`, `lvs`, `vgs`, `pvresize`, `lvextend` |

Every external invocation gets `LC_ALL=C`, so localised output cannot break parsing.

## Tool behaviour and criteria

The kernel does not re-read a partition table after a write. Rereading goes through `partx -u` first; `BLKRRPART` is only usable when the whole disk can be opened, and fails with `EBUSY` whenever any partition is mounted or used as swap. Added and deleted partitions are then applied per partition with `BLKPG`. A failed reread does not mean the table did not change: it degrades to "kernel view pending", and whether the write holds is answered by reading target bytes back.

An ext grow lands on block-group boundaries. When the last segment cannot hold a complete block group, `resize2fs` leaves it unused — 1.47.x takes a 400 MiB volume grown to 900 MiB to 896 MiB — so "grown to the end of the region" means the end falls inside the last unit, not that byte counts are equal. `e2fsck -fp` runs before grow or shrink; exit 1 means repairs were made and execution continues, and only larger exit bits abort.

An NTFS volume stores its starting sector as `HiddenSectors` at boot-sector offset `0x1C`. After a move, both the main boot sector and the backup in the last sector of the volume must carry the new start; fixing only the main one makes the NTFS checker report a damaged backup. Both signatures are verified first, and nothing is written when either does not match. A resize first tries `ntfsresize`; when it refuses a dirty volume, one `ntfsfix` is run and the resize retried, and the first error is kept if the retry fails.

Loop nodes may be stale: occupancy and mount checks never trust the `/dev/loopN` name or a `backing_file` path. They compare `(lo_device, lo_inode)` from `LOOP_GET_STATUS64` with `(st_dev, st_ino)` of the opened target; names only locate registry entries.

## OpenWrt combined layout

A read-only squashfs/erofs root plus a trailing RW overlay in the same partition: a grow identifies the inner filesystem and extends only the RW layer through an offset loop, and only ever grows; an inner filesystem other than ext or f2fs is refused. A trailing RW layer that is not yet formatted (first-boot install scene) is not a failure: the partition still grows, and the layer is initialised by fstools on first mount, leaving nothing to do here. Multi-device btrfs is refused for grow.

## Verification scope

The tool works against the command-line contracts of Linux `util-linux` and the official per-filesystem tools and is not tied to a distribution; the regression suite (`tests/wsl/`) is verified on Ubuntu with `util-linux` 2.41.3, and other distributions and versions are unverified. Occupancy probing is recorded in the grades below; paths that have not been run are not described as passing:

| Evidence grade | Content |
|---|---|
| run and verified | loop identity encoding (`st_dev`/`st_ino` against `lo_device`/`lo_inode`), `ENXIO` for an unbound loop, hard-link aliases and deleted backing paths, mounts in other mount namespaces, loop and partition mounts, active swap, sysfs holders, mountinfo of an exited process (`EINVAL`), fail-closed probing (exit code 4), the full `--grow-lv` chain on an LVM PV with no holder, `AllowDm` not rejected by the general rule when an active LV holds the partition (the chain reaches the LVM step) while a whole-range `mkfs` rewrite is still refused, all of `new`/`add`/`delete`/`set name\|type\|flag`/`undo`/`replay`/`release` refused on a mounted image with the disk bytes unchanged, a refusal that names the actual holder when several loops match, image operations usable by a non-root user (who cannot open `root:disk` loop nodes) |
| unverified | completeness of loop enumeration under containers with an independent PID namespace; deployments where mountinfo is unreadable because of permission limits such as `hidepid` (the code refuses as a failed probe, but it has not been run in a real `hidepid` environment); distributions and kernel versions outside the table above; **the full `--grow-lv` finish chain when the PV inside the image is mapped by another loop and the VG has an active LV** (see the section above: LVM's duplicate-PV rule makes `pvresize` on the temporary mapping a no-op, and the tool reports exit code 5 without claiming success) |
| design limit | the occupancy gate is a point-in-time decision before writing and does not stop an external loop / swap / mount created after the check; image-side enumeration covers only loop nodes visible to this process, and a node that cannot be opened for lack of permission (`root:disk`) is skipped as unbound — creating a binding and accessing the node require the same permission class, so such a user cannot create a binding to this image; a node that does not exist, `ENODEV`, and other cases where identity cannot be confirmed are refused as a failed probe, and this boundary moves with the kernel/udev permission model; fail-closed applies to a failed probe of an unrelated loop as well; a missing record item (`None`) does not participate in the comparison |

## Options

Which options a command accepts is defined by `DiskEdit help <CMD>`; the table below lists every option and its value form. An option whose value column reads `—` takes no value.

| Option | Value | Meaning |
|---|---|---|
| `--align` | A | placement alignment: `mib` or `none` (default `none`, the LBA as given; accepted by `add`, `move`, `copy`, `resize --end` and `resize --grow-to-end`; for `resize` it applies to the computed end) |
| `--allow-move` | — | `resize` may relocate blocking partitions on GPT |
| `--chunk` | MiB | recovery granularity for `move`, `copy` and `resize` (MiB, 1..=64). A resumed job that omits it keeps the record's value, a new job uses 4; an explicit value that differs from the record is refused. Refused where the path cannot relocate: MBR `resize`, online `resize`, and superfloppy `resize` |
| `--discard-replay` | — | `release` must be given it while the record still has a replayable range (or a damaged payload, or an unknown entry count); accepted by `release` only |
| `--dry-run` | — | `resize` prints the computed ranges, the filesystem step and any relocation plan; `add` prints the landing spot of the new partition. Both stop before the first write, create no record and do not require `--yes` |
| `--end` | LBA | ending LBA for `resize --end` |
| `--fs-only` | — | `resize` only: skip the partition-table change and act on the filesystem alone — offline `<TARGET>:N` grows the filesystem into its partition; `<MOUNTPOINT> [BYTES]` grows it online through a mounted volume |
| `--grow-lv` | — | `resize` continues into the LV and its filesystem on an LVM PV |
| `--grow-to-end` | — | `resize --grow-to-end` pins the end at the end of the usable space |
| `--json` | — | `layout`: print the structured per-partition detail instead of the text layout |
| `--alias` | NAME | names a target that exposes no WWN, EUI or repeatable serial (`nbd`, `rbd`, `dm`, `md`); accepted by every write command, by `release`, and by `records show` / `records list` / `records verify` for locating and verifying the record. The same name is needed to undo, replay or release the record, or verify it; images and devices with an identity of their own reject it (exit 2) |
| `--keep-partuuid` | — | `copy` reuses the source PARTUUID |
| `--label` | S | filesystem label written by `mkfs` (omitted: an empty label; no default value is invented) |
| `--lv` | NAME | LV to grow with `--grow-lv` (omitted: the only LV on that PV) |
| `--name` | S | partition name |
| `--no-fs` | — | `resize` only: skip the filesystem step of the grown partition |
| `--overwrite-data` | — | `new` authorises overwriting bytes of old partitions that fall inside the new table's metadata area |
| `--part-number` | N | entry slot for `add` (1-based, must be free; omitted: the first unused slot) |
| `--random` | — | `set uuid` asks the target to generate a new value: ext uses `tune2fs -U random`, xfs `xfs_admin -U generate`, btrfs `btrfstune -u`; swap has no generate form, and the NTFS serial is not the Windows volume UUID |
| `--root` | ROOT | `records list`: scan a record container directory instead of listing a target's record |
| `--sector-size` | N | image logical sector size (power of two, at least 512 bytes; the 65536-byte ceiling is an implementation limit, not a format requirement); for `copy` it applies to one target |
| `--start` | LBA\|end\|align | starting LBA; accepted by `add`, `move` and `copy` only. For `add` it may be omitted (the first hole that fits SIZE); `move` and `copy` require it and also accept `end`; `move` alone accepts `align` (the next 1 MiB boundary at or after the current start). `resize` changes only the end — relocating is `move` |
| `--table` | gpt\|mbr | partition table type created by `new` |
| `--type-guid` | GUID\|0xNN | partition type for `add`: GPT takes standard GUID text (omitted: Linux filesystem data); MBR takes a two-digit hex type byte (omitted: 0x83) |
| `--yes` | — | confirmation gate before writing: required by `new`, `add`, `delete`, `move`, `copy`, `resize`, `mkfs`, `set`, `undo`, `replay` and `release`, and a missing one exits 2 at parse time; the other commands (`layout`, `records list/verify/show`, `check`) do not accept it |
