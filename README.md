# pichi

The docker/podman-style front-end for the [pichi-vm](https://github.com/pichi-vm)
ecosystem: it manages VM images as OCI artifacts and boots them.

`pichi` owns image management — importing, pulling, pushing, and inspecting
artifacts in a local content-addressed cache — and delegates the actual VM
launch to the [`dillo`](https://github.com/pichi-vm/dillo) VMM. `pichi run`
derives the guest's device set from the cached artifact, then launches dillo
through the separate [`madrillo`](https://github.com/pichi-vm/madrillo) monitor.

## The artifact model

A pichi artifact is an OCI image composed of:

- **scute layers** — the read-only block-device layers of a
  [carapace](https://github.com/pichi-vm/carapace) (the guest root filesystem),
- an optional **PMI** layer — the measured boot payload (kernel + initramfs)
  produced by [`arma`](https://github.com/pichi-vm/arma), described by the
  [`pmi`](https://github.com/pichi-vm/pmi) wire-format spec,
- optional native **bootconfig**, and
- a **config** blob — the launch contract (CPU / memory / storage / interface
  requirements).

An artifact is *bootable* exactly when it carries a PMI. A carapace with no PMI
is still a valid image — a base others derive from.

## Commands

| Command | Purpose |
|---|---|
| `pichi import` | Import a raw image (+ optional PMI / DTB / config) into the cache |
| `pichi images` | List cached artifacts |
| `pichi inspect` | Inspect a cached image and its platform manifest |
| `pichi tag` / `rmi` | Manage tags (refcount-aware blob GC) |
| `pichi pull` / `push` | Move artifacts to and from an OCI registry |
| `pichi run` | Create and boot a persistent instance; `-d` prints its ID, `-i` forwards stdin, `-t` enables terminal mode |
| `pichi start` | Boot a saved instance; `-a` attaches to its console |
| `pichi rm` | Remove a stopped instance and its private disk |
| `pichi attach` | Attach to console output, forwarding stdin if saved with `-i` or explicitly requested |
| `pichi logs` | Print a snapshot of retained stdout/stderr for a workload ID |
| `pichi wait` | Wait for a workload ID and print its native exit status |
| `pichi stop` | Request console shutdown; force cleanup after `--time` seconds (default 10) |
| `pichi ps` | List retained IDs and created/running/exited/unknown/removing state |
| `pichi update` | Update a working-tree plan’s image lock |
| `pichi build` | Build via a VM appliance (thin launcher; see [BUILD.md](BUILD.md)) |
| `pichi system` | System-level inspection and maintenance |

Run `pichi <command> --help` for details.

Pichi's own usage, configuration and command errors exit with status 125.
Help and version requests exit with status 0. Foreground `run`, `start -a` and `attach`
preserve the VM's native exit status.

## Launch requirements

The image config's recommended CPU, memory and storage sizes supply the run
defaults. Operator configuration (`run.cpus`, `run.memory_mib`, `run.storage_mib`)
overrides those defaults, and CLI values take precedence. Allocations below a
required minimum fail; manual allocations below a numeric recommendation warn.
`recommended: max` uses detected host availability when no operator value is
set: available CPU parallelism, allocatable guest memory, or free space on the
storage backing filesystem. Linux guest memory uses the available, unreserved
2-MiB hugepage pool; other hosts use available physical memory, rounded to 2 MiB.

When no storage size is specified, Pichi recommends 1024 MiB, raised to any
image-required minimum. Explicit sizes take
precedence; the default recommendation does not impose a minimum.

Pichi attaches one unformatted writable block device alongside the read-only
image. The distribution decides how to format and mount it. Each instance owns a
private qcow2 disk that survives exit, detach and restart:

```sh
id=$(pichi run -d example:1 --cpus 4 --memory 4g --storage 8g -it)
pichi stop "$id"
pichi start -a "$id"  # same disk and saved image/resources
pichi rm "$id"        # only after the instance has stopped
```

`run` combines creation and start. Image selection, CPU, memory, disk capacity,
stdin and terminal settings are frozen at creation; changing a tag or config does
not change an existing instance. Without image or operator defaults, CPU and memory
are 1 vCPU and 1024 MiB. One boot at a time owns the disk; starting a running
instance fails as running or busy. A disk consistency check before
restart reports damage without repairing it. `rm` deletes the disk and boot history;
an interrupted removal can be retried and cannot be started again.

`-m/--memory` and `--storage` accept `<number>[b|k|m|g]` (bytes, KiB, MiB,
GiB; units are case-insensitive). Bare numbers are bytes, so replace an old
`--memory 1024` invocation with `--memory 1024m` or `--memory 1g`. Values must
be whole numbers of MiB. `run` and `build` use the same syntax; configuration
keys `run.memory_mib` and `run.storage_mib` remain in MiB.

`--storage-file` and `run.storage_file` are retired and rejected with migration
guidance. Remove those settings and use `run`/`start` for an instance-owned
disk. Existing external files are left untouched and are not imported. Build
guests retain their separate disposable raw storage and existing cleanup rules.

The image's `requirements.slot` defaults to `mmio`; interface and storage
slot declarations override it. Pichi supplies an explicit bus for every device.
Dillo allocates numeric slots within that bus and fails if the declared
platform cannot satisfy the request.

`pichi inspect` includes the decoded image config beside its manifest. Templates
can access it directly, for example
`pichi inspect example:1 --format '{{ config.requirements.storage.recommended }}'`.

Builds use the build appliance's requirements. `pichi build --storage <size>`
overrides its recommended storage allocation; output-image requirements remain
metadata for later launches.

## Build

`pichi build <dir> --ref <ref> -t <tag>` is a specialization of `pichi run`:
same launch path, plus virtiofs wires and a bidirectional build-port CBOR
exchange on named console `dev.pichi.build` (guest requests build-input; host
replies). The host is a thin launcher; the [Pichi build appliance](appliance/)
runs the packaged Conglobate builder and writes an OCI layout to the output mount. The host
reads the committed plan using gix and Conglobate's existing decoder. It omits
the network adapter when no declared step requests `network: true`, skipping
appliance network readiness and clock synchronization. Unused online stages
and plan paths crossing symlinks, submodules or parent components retain
networking. The same resolved commit is sent to the guest, so dirty
working-tree edits do not affect this decision.

Plan and lock paths are relative to `--context` (default `.`). The defaults
are `pichi.yaml` and `pichi.lock`. `-p`/`--plan` and `-l`/`--lock` override
them. The build-port response carries those resolved relative paths as
`plan` and `lock`, alongside the selected `context`.

Update the lock from the plan currently on disk, then commit both files:

```sh
pichi update .
git add pichi.yaml pichi.lock
git commit -m "Update image inputs"
```

`pichi update [DIR]` defaults to the current directory. `-p`/`--plan` and
`-l`/`--lock` select paths within DIR, defaulting to `pichi.yaml` and
`pichi.lock`. It reads working-tree edits directly; Git, the Conglobate CLI,
and a VM are unnecessary. Stage commands are never executed during update. Plans require an explicit
`version: 1` field.

Update resolves only the selected image's remote dependencies, including
stage-copy and boot sources, and replaces the complete version-1 lock with
exact OCI index pins. Unused entries are removed. An error preserves the
existing lock. The summary reports added, changed, and removed pins; an
unreadable or malformed previous lock can be replaced, with comparison
reported as unavailable. Plan reads and lock writes stay within DIR.

The default `--pull=always` refreshes references from the registry. The
existing pull policies also apply: `missing` uses cached references and
fetches absent ones, `never` uses only the local cache, and `newer` checks
upstream and refreshes changed images. For locally imported inputs or
cached-only operation, use `pichi update --pull=never`. Inputs must resolve
to OCI indexes; bare manifest references are rejected.

The `conglobate` library supplies plan, artifact, and lock types;
`pichi-launch` supplies the build-port types. `pichi update` maintains locks
on the host; `pichi build` remains a thin launcher and consumes a committed
plan and lock in the appliance.

`-b`/`--build-image` selects the build appliance image. When it is omitted,
pichi uses `ghcr.io/pichi-vm/pichi-build:latest`. If that image is not already cached,
the first build pulls it and continues. Full contract: [BUILD.md](BUILD.md).

## Workload supervision

Install `madrillo` and `dillo` on PATH. Dillo must support qcow2 block devices and
console closure requesting shutdown. Detached `run` and unattached
`start` print the instance ID on stdout. Use the full ID with instance commands.
`run` forwards no stdin unless `-i` is given, and enables no terminal
mode unless `-t` is given. These choices are saved. Host terminals use raw input
only when both apply; Ctrl-P Ctrl-Q detaches. Piped input remains binary. Multiple interactive
attachments may write input. Input EOF releases that attachment's input while
madrillo keeps dillo's liveness pipe open. Attachment is live
only; completed workloads retain their result and configured CRI console log.
On Unix, control and resize sockets use the native temporary directory (`TMPDIR`
when supplied). Their addresses are retained with the workload, so later clients
can use a different environment. The directory must allow Unix sockets and leave
room for their names within the operating system's socket-path limit.
A slow guest backpressures input without a write deadline. The attachment keeps
receiving output and completion and handling resize and caller signals while a
write waits. Input read-ahead is bounded. Once input is closed, attachments keep
receiving output; terminal detach discards unsent input and leaves the VM running.
On Unix, SIGQUIT ends the attachment with status 131 after restoring the host
terminal; the supervised VM keeps running.

For an instance created with `-t`, a host terminal sends its initial dimensions and subsequent changes
directly to Dillo's private resize endpoint. Reattachment supplies the new
terminal's dimensions, including after a detached launch. This works even without
stdin forwarding. Piped input does not change guest size.
Madrillo does not interpret these updates. Guest resize requires Linux's
[virtio-console cols/rows fix](https://github.com/torvalds/linux/commit/5326ab737a47278dbd16ed3ee7380b26c7056ddd)
or a backport; older kernels interpret the standard dimensions in reverse.
Confirmed workload cleanup also removes a Unix resize socket left by an abrupt
Dillo exit. Unknown or incomplete cleanup preserves runtime files.
`pichi logs ID` prints a snapshot of the selected boot's log, to the original stdout/stderr
streams, preserving binary bytes and partial-line fragments. It includes only
complete CRI records present when the file is opened; a record still being
written appears on a later invocation. Logs remain available after monitor exit
or loss, independently of the live attachment endpoint.
`pichi stop ID` closes Dillo's console input to request clean shutdown, even
while other clients forward input. After 10 seconds it asks madrillo to force
workload cleanup; `--time SECONDS` changes that grace period and `--time 0`
forces immediately. It prints the ID after confirmed exit. Stopping an exited
workload succeeds using its retained result. For a never-started instance, `stop`
succeeds without launching anything and `wait` prints 0; no exit result is invented.
The latest boot's original exit status remains available through `pichi wait ID`.
Missing results remain unknown. Instances retain their image pins until `rm`,
including after a successful exit. `rmi` may remove a tag without deleting a pinned
image. Build and older workload records remain readable but have no persistent
instance disk and cannot be restarted.

Build uses the same monitor launch and retains its output outside the cache.
Its named guest API pipes pass directly through to dillo: the guest requests
input before the host responds. Explicit build success and successful guest exit
are both required before OCI import.
Output is removed only after import and confirmed complete workload cleanup.
An active build retains the image cache because the guest selects its inputs.
Native caller qualification uses ordinary processes on public Linux, macOS and
Windows MSVC runners. It covers exact handle inheritance, independent caller
lifetime, terminal input and resize, and the opaque build channel. The pinned
source revisions and completed rounds are recorded in
[Madrillo's conformance status](https://github.com/pichi-vm/madrillo/blob/main/docs/STATUS.md).
Real VM build tests remain separate.

Windows launch requires permission to break away from the caller's Job Object.
If that permission is denied, launch fails rather than tying the monitor's
lifetime to the caller's Job. External
termination signals end the foreground attachment; they are not proxied to
dillo. Workload state lives beside the canonical image cache, in a directory
whose name appends `.workloads` (for example `storage.workloads`). This keeps
console data outside the build guest's cache mount.

## Source checkout

Cargo fetches the pinned Conglobate library as a Git dependency. No submodule
checkout is needed to build Pichi. The native Windows/macOS CI jobs initialize
`appliance/dillo` to build their launcher.

## License

Apache-2.0. See [LICENSE](LICENSE).

## COPR packages

The `npmccallum/pichi` COPR project builds `main` for Fedora on x86_64 and
aarch64. `.copr/Makefile` creates one source RPM; COPR builds the native
packages using the spec in `.copr/`. Binary builds need network access
for Cargo's locked dependencies. The unreleased RPM version is `0.1.0~dev`;
COPR's build ID in `Release` makes successive builds of that version upgrades.
