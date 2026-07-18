# ZFS pool backend

A new pool backend, `Zfs`, sits alongside the existing `Lvs` (SPDK blobstore) and `Lvm`
(volume group) backends. It is modeled on how openebs/zfs-localpv drives ZFS:

- The DiskPool `disks` entry is a ZFS pool/dataset path (e.g. `tank/data`, nesting
  allowed), like zfs-localpv's `poolname` StorageClass parameter.
- Creating a mayastor pool named `P` on disks entry `tank/data` creates the container ZFS
  filesystem dataset `tank/data/P`.
- Replicas are ZFS volumes (zvols) at `tank/data/P/<replica-uuid>`, exposed to SPDK through
  their `/dev/zvol/...` block device via an aio bdev.
- Mayastor snapshots and clones use native `zfs snapshot` / `zfs clone` (which the LVM
  backend cannot support at all).
- zfs-localpv-style options (volblocksize, compression, dedup, logbias, ...) are
  configurable at both pool level (defaults, inherited by child zvols) and per replica.

The backend drives the `zfs(8)` CLI via arg-vector exec only (no shell, no libzfs), exactly
like zfs-localpv. Ownership and metadata are persisted as ZFS user properties in the
`io.mayastor:` namespace.

Sparse zvols reclaim space on host trim through the aio bdev's UNMAP passthrough (BLKDISCARD
on block devices), which is a separate, backend-general feature; the ZFS backend needs no
trim-specific handling of its own.

## Architecture

### Pool / replica / snapshot model

| Concept | ZFS object | Notes |
| --- | --- | --- |
| Pool `P` on `tank/data` | container filesystem dataset `tank/data/P` | tagged with local `io.mayastor:pool=<uuid>` |
| Replica `<uuid>` | zvol `tank/data/P/<uuid>` | exposed via `aio:///dev/zvol/tank/data/P/<uuid>` |
| Snapshot | `tank/data/P/<uuid>@<snapshot-uuid>` | native `zfs snapshot`, props set atomically |
| Clone | zvol `tank/data/P/<clone-uuid>` | native `zfs clone` from a snapshot |

### Ownership via ZFS user properties (`io.mayastor:` namespace)

Metadata is persisted as ZFS user properties (analogous to the LVM backend's LVM tags and
the LVS backend's blob xattrs):

- Container dataset: `io.mayastor:pool` (= pool uuid; local presence is the ownership
  marker), `io.mayastor:disks`, `io.mayastor:volblocksize`.
- Zvol: `io.mayastor:uuid`, `:name`, `:share` (off/nvmf), `:allowed_hosts` (csv),
  `:entity_id`, `:snapshot_uuid` (clones only).
- Snapshot: `io.mayastor:snapshot_uuid`, `:name`, `:entity_id`, `:parent_id`, `:txn_id`,
  `:create_time`, `:discarded`.

CRITICAL subtlety: ZFS user properties are inherited by child datasets and snapshots, so
every "is this ours" check requires the property *source* to be `local`, not merely
present. This is enforced via `DsProps::local()` in `cli.rs`.

### Command reference (mirrors zfs-localpv)

- Create zvol: `zfs create [-s] -V <size> -b <volblocksize> -o <props...> tank/data/P/<uuid>`
- Resize (expand only): `zfs set volsize=<size> <dataset>`
- Property update: `zfs set <k=v> <dataset>` / `zfs inherit <k> <dataset>`
- Snapshot: `zfs snapshot -o <7 io.mayastor props> <ds>@<snapshot-uuid>` (atomic)
- Clone: `zfs clone -o <props> <ds>@<snap> tank/data/P/<clone-uuid>` (never re-sets
  encryption props; no `-V`/`-b`, inherited from origin)
- Destroy: `zfs destroy [-r|-d] <dataset>[@<snap>]`
- Discovery/detail: `zfs get/list -Hp ...` (tab-separated, exact byte values)

### Snapshot / clone / destroy semantics

- Deferred destroy: destroying a snapshot that still has clones sets
  `io.mayastor:discarded=true` then `zfs destroy -d` (the snapshot lingers until its last
  clone is gone). `discarded()` reports true when `io.mayastor:discarded=true` OR native
  `defer_destroy=on`.
- Replica destroy ordering:
  1. No snapshots -> `zfs destroy <ds>`.
  2. Only discarded, clone-less snapshots -> `zfs destroy -r <ds>`.
  3. A snapshot with clones -> `zfs promote <first-clone>`, re-evaluate, then 1/2.
  4. A remaining live (non-discarded) clone-less snapshot -> REFUSE with
     `HasLiveSnapshots`. This is a deliberate divergence from LVS (where snapshots outlive
     the replica): a ZFS snapshot is physically bound to its dataset and cannot be
     detached. The control-plane's delete-volume-but-keep-snapshots flow should delete
     snapshots first.

Identity is property-driven, never path-driven, so `zfs promote`/reparenting cannot orphan
metadata.

### Options plumbing

- Pool-level defaults: query params on the disks string, e.g.
  `tank/data?compression=zstd&volblocksize=16k&quota=200GiB`, parsed by `zfs/options.rs`.
  ZFS inheritance applies compression/dedup/logbias/sync to child zvols automatically;
  the volblocksize default is persisted as `io.mayastor:volblocksize`.
- Per-replica overrides: the `map<string,string> properties` on `CreateReplicaRequest`. The
  ZFS backend consumes an allowlist (volblocksize, compression, dedup, logbias, sync);
  replica value overrides the pool default; unknown keys error. LVS and LVM reject a
  non-empty properties map with `invalid_argument`.

## Deployment

The io-engine container image must include the `zfs` userland binaries and have `/dev/zvol`
mounted, and the DaemonSet must set `ENABLE_ZFS=true`; the backend is gated off otherwise.

## Runtime notes / limitations

- The zvol device node (`/dev/zvol/...`) appears asynchronously via udev; the backend polls
  for it after create/clone (`wait_for_zvol_device`, 10s timeout).
- Encryption: ZFS-native encryption create can be expressed via options, but key reload on
  import after reboot (`zfs load-key`) is not yet handled; the SPDK-crypto pool path is
  explicitly rejected for the ZFS backend.
- Capacity on a shared zpool floats with free space unless `?quota=` is set on the pool;
  quota is recommended when multiple pools share a zpool.
