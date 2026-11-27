# Fallbacks and refusals

A fallback is not a way to make a command succeed. It is a decision about whether to continue when a fact is missing. This tool is responsible for the records it creates, the changes it starts, and the recovery of its own interrupted work. It is not responsible for repairing disks, reconstructing foreign metadata, or rescuing data. When the facts are not available, the safe default is to refuse.

## Scope

This document answers one narrow question: when a fact is missing or an observation failed, does the tool continue, refuse, or recover? It does not define the product's command set. A capability counts as promised when it appears in the current command surface, in the README, in the generated reference or capability matrix, or in the observable behaviour of the previous release. This policy cannot be used on its own to remove a promised capability. Removing one is a separate scope decision with its own review, tests, and recorded rationale.

## What this tool does not do

It does not repair a partition table it did not write. A corrupt primary or backup header, a wrong CRC, overlapping existing entries, a missing protective MBR, or a table of unknown origin is reported and left alone. Reading such a table to decide whether it is safe to proceed is allowed; writing, adopting, or "fixing" it is not.

It does not reconstruct or adopt records from another tool, another product, or an unknown schema. A record whose ownership cannot be established is preserved and refused. Recovery is limited to state this tool created, including records written by earlier released versions when their schema is known.

It does not rescue data. Bad sectors, deleted partitions, overwritten filesystems, failing drives, and forensic recovery are outside the command set. A read-only report is allowed; recovery of user data is not promised.

It does not implement filesystem internals. mkfs, check, resize, label, and UUID changes go through the official filesystem tools. If a tool is missing, fails, or cannot report a fact, the step fails; it is not emulated by writing filesystem metadata directly.

It does not guess facts. Sector size, partition geometry, device identity, probe results, mount state, and whether an external step completed are either known or the operation stops. A guessed value is not a degraded success.

It does not emulate an unsupported platform or capability, and it does not add a shell fallback that performs the same operation by other means. An unsupported platform is refused with an error.

It does not silently change semantics. A path that produces a different result from the promised one is a behavior change and must be visible to the user, not hidden inside a fallback.

It does not grow a repair mode under the name of recovery. Recovery resumes or rolls back this tool's own operation; it does not become a general disk repair or data-rescue feature.

## Exceptions

An operation can look like one of the exclusions above and still be allowed in these cases.

Recovering our own state is in scope. A record left by this tool, a table this tool wrote halfway, or an external step this tool started can be resumed, rolled back, quarantined, or released. The same applies to records from earlier released versions when their schema is known and tested; an unknown schema is refused.

Read-only inspection is allowed. Reading a foreign record, an unknown table, or a damaged header to decide whether proceeding is safe does not violate the boundary, as long as nothing is written, adopted, deleted, or repaired.

Documented compatibility is allowed. Old record formats, CLI aliases, and behaviours that appear in the README, reference, capability matrix, or previous release are product promises. They may be kept, but each needs tests and an owner. This policy does not authorize dropping them.

Documented features that touch these areas remain product scope. Online resize, LVM handling, the VM carrier, and image formats are features when they appear in the command surface or capability matrix. They are not fallbacks, and this policy is not a reason to remove them.

A partial failure of our own operation may be finished, rolled back, or cleaned up. Only artifacts created by this tool are touched; foreign artifacts are preserved.

A retry after the kernel view lags is allowed once the write fact is recorded, the retry is bounded, and repeating it cannot duplicate a side effect.

A branch that only refuses is not an empty branch. It produces an observable error and needs a test that fails when the refusal is removed.

## Removing a fallback

Removing a fallback requires a test that proves the current behavior, a recorded decision that the case is outside the promised scope, and a check that normal paths are unchanged. The change is made one item at a time, with the behavior change recorded. A branch that looks unused is not enough reason to delete it; a refusal that looks like it does nothing is not an empty branch.

## Deciding

The first question is whether the fact can be known. Sector size, partition geometry, device identity, probe results, and whether an external step completed are not values to guess. A wrong guess in a write, shrink, delete, or rebuild is not a failed command; it is damaged data.

The second question is whether a failure is visible. A record that cannot be read, a parse error, a permission problem, or an I/O error must travel upward as an error. It must not become "no record", an empty set, or a default value. The only exception is a failure that is explicitly modelled as unknown, with every later branch treating unknown as a reason not to continue.

The third question is who owns the state. A record left by an earlier run of this tool, a table this tool wrote halfway, or an external step this tool started is inside our responsibility. Another tool's record, a damaged backup header, or a partition table of unknown origin is not. Such state is preserved and refused: it is not repaired, adopted, or deleted.

Before continuing, check whether continuing is safe under every interpretation. If the same observation can mean two things and one of them would destroy data, do not continue. A degraded path is allowed only when none of the possible meanings can cause an irreversible result, and it must leave something observable behind. Silent degradation is not allowed.

If a degraded step fails halfway, the state it leaves must be recognizable by the next run. A half-written, half-changed, or half-completed operation must not look successful. Retries are allowed only after the write fact is recorded and only when repeating the step cannot duplicate a side effect. Retry, compatibility, and repair are different things and cannot substitute for one another.

Every fallback that remains must justify itself. It needs a test that fails when the fallback is removed; an empty branch with no such test is either given one or deleted. Compatibility with older records, retrying after the kernel view lags, and reporting an unsupported platform are legitimate categories, but each has a boundary: old formats only for records this tool created with a known schema, retries only after the write fact is set, and unsupported platforms only refused, never emulated.

Finally, a fallback must not expand the tool's responsibility. Supporting a format, platform, or repair scenario that was never promised, or bypassing permission, ownership, or identity checks to make progress, is not allowed. A fallback must not quietly change what the user sees. If it changes the result, the user has to know.

In one sentence: continue only when the fact is known; refuse when it is not; recover our own state, leave other state alone; and any continuation must be observable, recoverable, and testable.