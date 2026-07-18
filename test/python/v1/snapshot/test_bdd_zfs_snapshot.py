"""ZFS snapshot support feature tests."""

import os
import shutil
import subprocess

import pytest
from pytest_bdd import (
    given,
    scenario,
    then,
    when,
    parsers,
)

from common.command import run_cmd
from common.nvme import nvme_connect, nvme_disconnect, nvme_disconnect_all
from v1.mayastor import mayastor_mod, container_mod
import grpc
import pool_pb2 as pool_pb
import replica_pb2 as replica_pb
import snapshot_pb2 as snapshot_pb
import common_pb2 as common_pb

ZPOOL_NAME = "mstest"
PARENT_DATASET = f"{ZPOOL_NAME}/disks"
POOL_NAME = "zfspool"
POOL_DATASET = f"{PARENT_DATASET}/{POOL_NAME}"
DISK_IMAGE = "/tmp/ms0-zfs-disk0.img"

REPLICA_UUID = "af68d693-e9cd-4846-9726-4178d8823cee"
REPLICA_DATASET = f"{POOL_DATASET}/{REPLICA_UUID}"
REPLICA_SIZE = 64 * 1024 * 1024
SNAP_UUID = "d6dabcb4-ca97-49c1-85ff-4e393ad974b7"
SNAP_NAME = "zfssnap1"
SNAP_ENTITY_ID = "71f81dfb-6896-4085-b76d-2957a19266f6"
SNAP_TXN_ID = "8ec22d5e-5a1d-44b8-8f22-d9e48b7ec781"
SNAP_DATASET = f"{REPLICA_DATASET}@{SNAP_UUID}"
CLONE_UUID = "cd2cecdc-1e49-49ca-993d-92606155a4ad"
CLONE_NAME = "zfsclone1"
CLONE_DATASET = f"{POOL_DATASET}/{CLONE_UUID}"

PATTERN_MB = 4
PATTERN_FILE = "/tmp/zfs-snapshot-pattern.img"
OVERWRITE_FILE = "/tmp/zfs-snapshot-overwrite.img"
READBACK_FILE = "/tmp/zfs-snapshot-readback.img"

pytestmark = pytest.mark.skipif(
    not os.path.exists("/sys/module/zfs") or shutil.which("zfs") is None,
    reason="ZFS is not available (zfs kernel module not loaded or zfs binary missing)",
)


@scenario("features/zfs_snapshot.feature", "creating a replica snapshot")
def test_creating_a_replica_snapshot(setup):
    """creating a replica snapshot."""


@scenario("features/zfs_snapshot.feature", "listing replica snapshots")
def test_listing_replica_snapshots(setup):
    """listing replica snapshots."""


@scenario("features/zfs_snapshot.feature", "creating a clone from a snapshot")
def test_creating_a_clone_from_a_snapshot(setup):
    """creating a clone from a snapshot."""


@scenario("features/zfs_snapshot.feature", "destroying a snapshot which has clones")
def test_destroying_a_snapshot_which_has_clones(setup):
    """destroying a snapshot which has clones."""


@scenario(
    "features/zfs_snapshot.feature", "destroying the last clone of a discarded snapshot"
)
def test_destroying_the_last_clone_of_a_discarded_snapshot(setup):
    """destroying the last clone of a discarded snapshot."""


@scenario(
    "features/zfs_snapshot.feature",
    "destroying a replica with a live snapshot is refused",
)
def test_destroying_a_replica_with_a_live_snapshot_is_refused(setup):
    """destroying a replica with a live snapshot is refused."""


@scenario(
    "features/zfs_snapshot.feature",
    "destroying a replica whose snapshots all have clones",
)
def test_destroying_a_replica_whose_snapshots_all_have_clones(setup):
    """destroying a replica whose snapshots all have clones."""


def zfs_get(dataset, prop):
    """Return the parseable value of a zfs property."""
    out = subprocess.run(
        f"nix-sudo zfs get -Hp -o value {prop} '{dataset}'",
        shell=True,
        check=True,
        capture_output=True,
    )
    return out.stdout.decode("ascii").strip("\n")


def zfs_exists(dataset, dstype="volume"):
    """Check whether the given dataset exists."""
    p = subprocess.run(
        f"nix-sudo zfs list -Hp -o name -t {dstype} '{dataset}'",
        shell=True,
        check=False,
        capture_output=True,
    )
    return p.returncode == 0


def md5sum(path):
    """Return the md5 digest of the given file."""
    out = subprocess.run(
        f"nix-sudo md5sum '{path}'",
        shell=True,
        check=True,
        capture_output=True,
    )
    return out.stdout.decode("ascii").split()[0]


def write_pattern(device, pattern_file):
    """Fill a pattern file with random data and write it to the device."""
    run_cmd(f"rm -f '{pattern_file}'", True)
    run_cmd(f"dd if=/dev/urandom of='{pattern_file}' bs=1M count={PATTERN_MB}", True)
    run_cmd(
        f"nix-sudo dd if='{pattern_file}' of='{device}' bs=1M count={PATTERN_MB}"
        " oflag=direct conv=fsync",
        True,
    )
    return md5sum(pattern_file)


def read_pattern(device):
    """Read the pattern area back from the device and return its md5."""
    run_cmd(f"rm -f '{READBACK_FILE}'", True)
    run_cmd(
        f"nix-sudo dd if='{device}' of='{READBACK_FILE}' bs=1M count={PATTERN_MB}"
        " iflag=direct",
        True,
    )
    return md5sum(READBACK_FILE)


@pytest.fixture(scope="module")
def setup(container_mod):
    nvme_disconnect_all()
    yield
    nvme_disconnect_all()
    for file in [PATTERN_FILE, OVERWRITE_FILE, READBACK_FILE]:
        run_cmd(f"rm -f '{file}'", True)


@pytest.fixture(scope="module")
def zpool_parent_dataset():
    p = subprocess.run(
        f"nix-sudo zpool list {ZPOOL_NAME}",
        shell=True,
        check=False,
        capture_output=True,
    )
    # if a zpool is left over from a previous run then remove it first
    if p.returncode == 0:
        run_cmd(f"nix-sudo zpool destroy {ZPOOL_NAME}", True)
    run_cmd(f"rm -f '{DISK_IMAGE}'", True)
    run_cmd(f"truncate -s 4G '{DISK_IMAGE}'", True)
    out = subprocess.run(
        f"sudo -E losetup -f '{DISK_IMAGE}' --show",
        shell=True,
        check=True,
        capture_output=True,
    )
    disk = out.stdout.decode("ascii").strip("\n")
    run_cmd(f"nix-sudo zpool create {ZPOOL_NAME} '{disk}'", True)
    run_cmd(f"nix-sudo zfs create {PARENT_DATASET}", True)
    yield PARENT_DATASET
    run_cmd(f"nix-sudo zpool destroy {ZPOOL_NAME}", False)
    run_cmd(f"sudo -E losetup -d '{disk}'", False)
    run_cmd(f"rm -f '{DISK_IMAGE}'", True)


@pytest.fixture
def find_replica(get_mayastor_instance):
    def find(uuid):
        for replica in get_mayastor_instance.replica_rpc.ListReplicas(
            replica_pb.ListReplicaOptions(pooltypes=[pool_pb.Zfs])
        ).replicas:
            if replica.uuid == uuid:
                return replica
        return None

    yield find


@pytest.fixture
def find_snapshot(get_mayastor_instance):
    def find(uuid):
        for snapshot in get_mayastor_instance.snapshot_rpc.ListSnapshot(
            snapshot_pb.ListSnapshotsRequest()
        ).snapshots:
            if snapshot.snapshot_uuid == uuid:
                return snapshot
        return None

    yield find


@pytest.fixture
def destroy_replica(get_mayastor_instance):
    def destroy(uuid):
        try:
            get_mayastor_instance.replica_rpc.DestroyReplica(
                replica_pb.DestroyReplicaRequest(uuid=uuid)
            )
        except grpc.RpcError:
            pass

    yield destroy


@pytest.fixture
def destroy_snapshot(get_mayastor_instance):
    def destroy(uuid):
        try:
            get_mayastor_instance.snapshot_rpc.DestroySnapshot(
                snapshot_pb.DestroySnapshotRequest(
                    snapshot_uuid=uuid, pool_name=POOL_NAME
                )
            )
        except grpc.RpcError:
            pass

    yield destroy


@given(
    parsers.parse('a mayastor instance "{name}"'),
    target_fixture="get_mayastor_instance",
)
def get_mayastor_instance(mayastor_mod, name):
    return mayastor_mod[f"{name}"]


@given(
    parsers.parse('a ZFS backed pool called "{pool_name}"'),
    target_fixture="zfs_pool",
)
def zfs_pool(get_mayastor_instance, zpool_parent_dataset, pool_name):
    pool = get_mayastor_instance.pool_rpc.CreatePool(
        pool_pb.CreatePoolRequest(
            name=pool_name, disks=[PARENT_DATASET], pooltype=pool_pb.Zfs
        )
    )
    yield pool
    try:
        get_mayastor_instance.pool_rpc.DestroyPool(
            pool_pb.DestroyPoolRequest(name=pool_name)
        )
    except grpc.RpcError:
        pass


@given("a zfs backed replica", target_fixture="zfs_replica")
def zfs_replica(get_mayastor_instance, zfs_pool, destroy_replica):
    replica = get_mayastor_instance.replica_rpc.CreateReplica(
        replica_pb.CreateReplicaRequest(
            name=REPLICA_UUID,
            uuid=REPLICA_UUID,
            pooluuid=zfs_pool.uuid,
            size=REPLICA_SIZE,
            thin=True,
        )
    )
    yield replica
    destroy_replica(REPLICA_UUID)


@given(
    "the replica is shared over nvmf and written with a known pattern",
    target_fixture="written_pattern",
)
def the_replica_is_shared_over_nvmf_and_written_with_a_known_pattern(
    get_mayastor_instance, zfs_replica
):
    replica = get_mayastor_instance.replica_rpc.ShareReplica(
        replica_pb.ShareReplicaRequest(uuid=REPLICA_UUID, share=common_pb.NVMF)
    )
    device = nvme_connect(replica.uri)
    digest = write_pattern(device, PATTERN_FILE)
    yield {"uri": replica.uri, "device": device, "md5": digest}
    nvme_disconnect(replica.uri)


@given("a replica snapshot", target_fixture="replica_snapshot")
def replica_snapshot(get_mayastor_instance, zfs_replica, destroy_snapshot):
    response = get_mayastor_instance.snapshot_rpc.CreateReplicaSnapshot(
        snapshot_pb.CreateReplicaSnapshotRequest(
            replica_uuid=REPLICA_UUID,
            snapshot_uuid=SNAP_UUID,
            snapshot_name=SNAP_NAME,
            entity_id=SNAP_ENTITY_ID,
            txn_id=SNAP_TXN_ID,
        )
    )
    yield response
    destroy_snapshot(SNAP_UUID)


@given("the replica is overwritten with a different pattern")
def the_replica_is_overwritten_with_a_different_pattern(written_pattern):
    write_pattern(written_pattern["device"], OVERWRITE_FILE)


@given("a clone created from the snapshot", target_fixture="snapshot_clone")
def snapshot_clone(get_mayastor_instance, replica_snapshot, destroy_replica):
    clone = get_mayastor_instance.snapshot_rpc.CreateSnapshotClone(
        snapshot_pb.CreateSnapshotCloneRequest(
            snapshot_uuid=SNAP_UUID, clone_name=CLONE_NAME, clone_uuid=CLONE_UUID
        )
    )
    yield clone
    destroy_replica(CLONE_UUID)


@given("the snapshot has been destroyed and marked discarded")
def the_snapshot_has_been_destroyed_and_marked_discarded(get_mayastor_instance):
    get_mayastor_instance.snapshot_rpc.DestroySnapshot(
        snapshot_pb.DestroySnapshotRequest(snapshot_uuid=SNAP_UUID, pool_name=POOL_NAME)
    )


@when("a user creates a snapshot of the replica", target_fixture="create_snapshot")
def a_user_creates_a_snapshot_of_the_replica(
    get_mayastor_instance, zfs_replica, destroy_snapshot
):
    response = get_mayastor_instance.snapshot_rpc.CreateReplicaSnapshot(
        snapshot_pb.CreateReplicaSnapshotRequest(
            replica_uuid=REPLICA_UUID,
            snapshot_uuid=SNAP_UUID,
            snapshot_name=SNAP_NAME,
            entity_id=SNAP_ENTITY_ID,
            txn_id=SNAP_TXN_ID,
        )
    )
    yield response
    destroy_snapshot(SNAP_UUID)


@when("a user lists the snapshots of the replica", target_fixture="list_snapshots")
def a_user_lists_the_snapshots_of_the_replica(get_mayastor_instance):
    return get_mayastor_instance.snapshot_rpc.ListSnapshot(
        snapshot_pb.ListSnapshotsRequest(source_uuid=REPLICA_UUID)
    ).snapshots


@when("a user creates a clone from the snapshot", target_fixture="clone_replica")
def a_user_creates_a_clone_from_the_snapshot(get_mayastor_instance, destroy_replica):
    clone = get_mayastor_instance.snapshot_rpc.CreateSnapshotClone(
        snapshot_pb.CreateSnapshotCloneRequest(
            snapshot_uuid=SNAP_UUID, clone_name=CLONE_NAME, clone_uuid=CLONE_UUID
        )
    )
    yield clone
    destroy_replica(CLONE_UUID)


@when("a user destroys the snapshot")
def a_user_destroys_the_snapshot(get_mayastor_instance):
    get_mayastor_instance.snapshot_rpc.DestroySnapshot(
        snapshot_pb.DestroySnapshotRequest(snapshot_uuid=SNAP_UUID, pool_name=POOL_NAME)
    )


@when("a user destroys the clone")
def a_user_destroys_the_clone(get_mayastor_instance):
    get_mayastor_instance.replica_rpc.DestroyReplica(
        replica_pb.DestroyReplicaRequest(uuid=CLONE_UUID)
    )


@when("a user destroys the replica")
def a_user_destroys_the_replica(get_mayastor_instance):
    get_mayastor_instance.replica_rpc.DestroyReplica(
        replica_pb.DestroyReplicaRequest(uuid=REPLICA_UUID)
    )


@when(
    "a user attempts to destroy the replica",
    target_fixture="attempt_destroy_replica",
)
def a_user_attempts_to_destroy_the_replica(get_mayastor_instance):
    try:
        get_mayastor_instance.replica_rpc.DestroyReplica(
            replica_pb.DestroyReplicaRequest(uuid=REPLICA_UUID)
        )
        return None
    except grpc.RpcError as error:
        return error


@then("a zfs snapshot should exist for the replica zvol")
def a_zfs_snapshot_should_exist_for_the_replica_zvol():
    assert zfs_exists(SNAP_DATASET, "snapshot")


@then("the zfs snapshot should carry the io.mayastor snapshot properties")
def the_zfs_snapshot_should_carry_the_io_mayastor_snapshot_properties():
    assert zfs_get(SNAP_DATASET, "io.mayastor:snapshot_uuid") == SNAP_UUID
    assert zfs_get(SNAP_DATASET, "io.mayastor:entity_id") == SNAP_ENTITY_ID
    assert zfs_get(SNAP_DATASET, "io.mayastor:txn_id") == SNAP_TXN_ID


@then("the snapshot parameters should round-trip")
def the_snapshot_parameters_should_round_trip(list_snapshots):
    snapshots = [
        snapshot for snapshot in list_snapshots if snapshot.snapshot_uuid == SNAP_UUID
    ]
    assert len(snapshots) == 1
    snapshot = snapshots[0]
    assert snapshot.snapshot_name == SNAP_NAME
    assert snapshot.entity_id == SNAP_ENTITY_ID
    assert snapshot.txn_id == SNAP_TXN_ID
    assert snapshot.source_uuid == REPLICA_UUID
    assert snapshot.pool_name == POOL_NAME
    assert snapshot.discarded_snapshot is False


@then("reading the clone over nvmf should return the pre-snapshot data")
def reading_the_clone_over_nvmf_should_return_the_pre_snapshot_data(
    get_mayastor_instance, written_pattern, clone_replica
):
    clone = get_mayastor_instance.replica_rpc.ShareReplica(
        replica_pb.ShareReplicaRequest(uuid=CLONE_UUID, share=common_pb.NVMF)
    )
    device = nvme_connect(clone.uri)
    try:
        assert read_pattern(device) == written_pattern["md5"]
    finally:
        nvme_disconnect(clone.uri)


@then("the snapshot should still be listed as discarded")
def the_snapshot_should_still_be_listed_as_discarded(find_snapshot):
    snapshot = find_snapshot(SNAP_UUID)
    assert snapshot is not None
    assert snapshot.discarded_snapshot is True


@then("the zfs snapshot should be held with deferred destroy")
def the_zfs_snapshot_should_be_held_with_deferred_destroy():
    assert zfs_get(SNAP_DATASET, "defer_destroy") == "on"


@then("the snapshot should be gone")
def the_snapshot_should_be_gone(find_snapshot):
    assert find_snapshot(SNAP_UUID) is None
    assert not zfs_exists(SNAP_DATASET, "snapshot")


@then("the replica destroy request should fail")
def the_replica_destroy_request_should_fail(attempt_destroy_replica):
    assert attempt_destroy_replica is not None


@then("the replica should still be present")
def the_replica_should_still_be_present(find_replica):
    assert find_replica(REPLICA_UUID) is not None
    assert zfs_exists(REPLICA_DATASET)


@then("the replica should be destroyed")
def the_replica_should_be_destroyed(find_replica):
    assert find_replica(REPLICA_UUID) is None
    assert not zfs_exists(REPLICA_DATASET)


@then("the clone should still be present and readable")
def the_clone_should_still_be_present_and_readable(get_mayastor_instance, find_replica):
    assert find_replica(CLONE_UUID) is not None
    assert zfs_exists(CLONE_DATASET)
    clone = get_mayastor_instance.replica_rpc.ShareReplica(
        replica_pb.ShareReplicaRequest(uuid=CLONE_UUID, share=common_pb.NVMF)
    )
    device = nvme_connect(clone.uri)
    try:
        # any successful read is enough to prove the promoted clone is intact
        read_pattern(device)
    finally:
        nvme_disconnect(clone.uri)
