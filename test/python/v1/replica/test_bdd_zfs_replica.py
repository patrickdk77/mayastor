"""ZFS replica support feature tests."""

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
from v1.mayastor import mayastor_mod, container_mod
import grpc
import pool_pb2 as pool_pb
import replica_pb2 as pb
import common_pb2 as common_pb

ZPOOL_NAME = "mstest"
PARENT_DATASET = f"{ZPOOL_NAME}/disks"
POOL_NAME = "zfspool"
POOL_DATASET = f"{PARENT_DATASET}/{POOL_NAME}"
VBS_POOL_NAME = "zfspool2"
VBS_POOL_DATASET = f"{PARENT_DATASET}/{VBS_POOL_NAME}"
DISK_IMAGE = "/tmp/ms0-zfs-disk0.img"

ZFS_REPLICA_UUID = "45c23e54-dc86-45f6-b55b-e44d05f154dd"
THIN_REPLICA_UUID = "0b921e06-0962-4116-a065-01e2c8f76068"
PROP_REPLICA_UUID = "3f9b1c2d-8a44-4a3f-9b5e-2d1c0a7e6f31"
VBS_REPLICA_UUID = "a7c6b2d4-1e2f-4c5a-8d9b-0f1e2d3c4b5a"
SHARED_REPLICA_UUID = "6f3d1a2b-4c5d-4e6f-8a9b-1c2d3e4f5a6b"
REPLICA_SIZE = 64 * 1024 * 1024
HOST_NQN = "nqn.2014-08.org.nvmexpress:uuid:f81bc329-fadb-4507-a8b1-cd2fb5cf9b0c"

pytestmark = pytest.mark.skipif(
    not os.path.exists("/sys/module/zfs") or shutil.which("zfs") is None,
    reason="ZFS is not available (zfs kernel module not loaded or zfs binary missing)",
)


@scenario("features/zfs_replica.feature", "creating a thick replica on a zfs pool")
def test_creating_a_thick_replica_on_a_zfs_pool():
    """creating a thick replica on a zfs pool."""


@scenario("features/zfs_replica.feature", "creating a thin replica on a zfs pool")
def test_creating_a_thin_replica_on_a_zfs_pool():
    """creating a thin replica on a zfs pool."""


@scenario(
    "features/zfs_replica.feature",
    "pool default volblocksize is applied to new replicas",
)
def test_pool_default_volblocksize_is_applied_to_new_replicas():
    """pool default volblocksize is applied to new replicas."""


@scenario(
    "features/zfs_replica.feature", "per-replica properties override the pool defaults"
)
def test_per_replica_properties_override_the_pool_defaults():
    """per-replica properties override the pool defaults."""


@scenario(
    "features/zfs_replica.feature", "share properties survive pool export and import"
)
def test_share_properties_survive_pool_export_and_import():
    """share properties survive pool export and import."""


@scenario("features/zfs_replica.feature", "growing a replica")
def test_growing_a_replica():
    """growing a replica."""


@scenario("features/zfs_replica.feature", "shrinking a replica is rejected")
def test_shrinking_a_replica_is_rejected():
    """shrinking a replica is rejected."""


@scenario("features/zfs_replica.feature", "destroying a replica backed by a zfs pool")
def test_destroying_a_replica_backed_by_a_zfs_pool():
    """destroying a replica backed by a zfs pool."""


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
def create_pool(get_mayastor_instance):
    def create(name, disks, pooltype):
        return get_mayastor_instance.pool_rpc.CreatePool(
            pool_pb.CreatePoolRequest(name=name, disks=disks, pooltype=pooltype)
        )

    yield create


@pytest.fixture
def create_replica(get_mayastor_instance):
    def create(
        uuid,
        pool_uuid,
        size,
        share=common_pb.NONE,
        thin=False,
        properties=None,
        allowed_hosts=None,
    ):
        return get_mayastor_instance.replica_rpc.CreateReplica(
            pb.CreateReplicaRequest(
                name=uuid,
                uuid=uuid,
                pooluuid=pool_uuid,
                size=size,
                thin=thin,
                share=share,
                allowed_hosts=allowed_hosts or [],
                properties=properties or {},
            )
        )

    yield create


@pytest.fixture
def find_replica(get_mayastor_instance):
    def find(uuid):
        for replica in get_mayastor_instance.replica_rpc.ListReplicas(
            pb.ListReplicaOptions(pooltypes=[pool_pb.Zfs])
        ).replicas:
            if replica.uuid == uuid:
                return replica
        return None

    yield find


@pytest.fixture
def destroy_replica(get_mayastor_instance):
    def destroy(uuid):
        try:
            get_mayastor_instance.replica_rpc.DestroyReplica(
                pb.DestroyReplicaRequest(uuid=uuid)
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
def zfs_pool(get_mayastor_instance, zpool_parent_dataset, create_pool, pool_name):
    pool = create_pool(pool_name, [PARENT_DATASET], pool_pb.Zfs)
    yield pool
    try:
        get_mayastor_instance.pool_rpc.DestroyPool(
            pool_pb.DestroyPoolRequest(name=pool_name)
        )
    except grpc.RpcError:
        pass


@given(
    "a zfs pool created with a default volblocksize of 32k",
    target_fixture="vbs_pool",
)
def vbs_pool(get_mayastor_instance, zpool_parent_dataset, create_pool):
    pool = create_pool(
        VBS_POOL_NAME, [f"{PARENT_DATASET}?volblocksize=32768"], pool_pb.Zfs
    )
    yield pool
    try:
        get_mayastor_instance.pool_rpc.DestroyPool(
            pool_pb.DestroyPoolRequest(name=VBS_POOL_NAME)
        )
    except grpc.RpcError:
        pass


@given("a zfs backed replica", target_fixture="zfs_replica")
def zfs_replica(zfs_pool, create_replica, destroy_replica):
    replica = create_replica(ZFS_REPLICA_UUID, zfs_pool.uuid, REPLICA_SIZE)
    yield replica
    destroy_replica(ZFS_REPLICA_UUID)


@given(
    "a replica shared over nvmf with an allowed host",
    target_fixture="shared_replica",
)
def shared_replica(zfs_pool, create_replica, destroy_replica):
    replica = create_replica(
        SHARED_REPLICA_UUID,
        zfs_pool.uuid,
        REPLICA_SIZE,
        share=common_pb.NVMF,
        allowed_hosts=[HOST_NQN],
    )
    yield replica
    destroy_replica(SHARED_REPLICA_UUID)


@when("a user creates a thick replica on the zfs pool", target_fixture="thick_replica")
def a_user_creates_a_thick_replica_on_the_zfs_pool(
    zfs_pool, create_replica, destroy_replica
):
    replica = create_replica(ZFS_REPLICA_UUID, zfs_pool.uuid, REPLICA_SIZE, thin=False)
    yield replica
    destroy_replica(ZFS_REPLICA_UUID)


@when("a user creates a thin replica on the zfs pool", target_fixture="thin_replica")
def a_user_creates_a_thin_replica_on_the_zfs_pool(
    zfs_pool, create_replica, destroy_replica
):
    replica = create_replica(THIN_REPLICA_UUID, zfs_pool.uuid, REPLICA_SIZE, thin=True)
    yield replica
    destroy_replica(THIN_REPLICA_UUID)


@when(
    "a user creates a replica on the pool with the volblocksize default",
    target_fixture="vbs_replica",
)
def a_user_creates_a_replica_on_the_pool_with_the_volblocksize_default(
    vbs_pool, create_replica, destroy_replica
):
    replica = create_replica(VBS_REPLICA_UUID, vbs_pool.uuid, REPLICA_SIZE)
    yield replica
    destroy_replica(VBS_REPLICA_UUID)


@when(
    "a user creates a replica with a volblocksize property of 64k",
    target_fixture="prop_replica",
)
def a_user_creates_a_replica_with_a_volblocksize_property_of_64k(
    zfs_pool, create_replica, destroy_replica
):
    replica = create_replica(
        PROP_REPLICA_UUID,
        zfs_pool.uuid,
        REPLICA_SIZE,
        properties={"volblocksize": "65536"},
    )
    yield replica
    destroy_replica(PROP_REPLICA_UUID)


@when("the user exports and imports the pool")
def the_user_exports_and_imports_the_pool(get_mayastor_instance):
    get_mayastor_instance.pool_rpc.ExportPool(pool_pb.ExportPoolRequest(name=POOL_NAME))
    get_mayastor_instance.pool_rpc.ImportPool(
        pool_pb.ImportPoolRequest(
            name=POOL_NAME, disks=[PARENT_DATASET], pooltype=pool_pb.Zfs
        )
    )


@when("a user resizes the replica to a larger size", target_fixture="resized_replica")
def a_user_resizes_the_replica_to_a_larger_size(get_mayastor_instance, zfs_replica):
    return get_mayastor_instance.replica_rpc.ResizeReplica(
        pb.ResizeReplicaRequest(uuid=ZFS_REPLICA_UUID, requested_size=2 * REPLICA_SIZE)
    )


@when(
    "a user attempts to resize the replica to a smaller size",
    target_fixture="attempt_resize_replica",
)
def a_user_attempts_to_resize_the_replica_to_a_smaller_size(
    get_mayastor_instance, zfs_replica
):
    try:
        get_mayastor_instance.replica_rpc.ResizeReplica(
            pb.ResizeReplicaRequest(
                uuid=ZFS_REPLICA_UUID, requested_size=REPLICA_SIZE // 2
            )
        )
        return None
    except grpc.RpcError as error:
        return error


@when("a user calls destroy replica")
def a_user_calls_destroy_replica(get_mayastor_instance):
    get_mayastor_instance.replica_rpc.DestroyReplica(
        pb.DestroyReplicaRequest(uuid=ZFS_REPLICA_UUID)
    )


@then("a zvol should be created with a refreservation")
def a_zvol_should_be_created_with_a_refreservation(find_replica):
    replica = find_replica(ZFS_REPLICA_UUID)
    assert replica is not None
    assert replica.thin is False
    dataset = f"{POOL_DATASET}/{ZFS_REPLICA_UUID}"
    assert zfs_exists(dataset)
    assert zfs_get(dataset, "refreservation") != "none"


@then("a sparse zvol should be created without a refreservation")
def a_sparse_zvol_should_be_created_without_a_refreservation(find_replica):
    replica = find_replica(THIN_REPLICA_UUID)
    assert replica is not None
    assert replica.thin is True
    dataset = f"{POOL_DATASET}/{THIN_REPLICA_UUID}"
    assert zfs_exists(dataset)
    assert zfs_get(dataset, "refreservation") == "none"


@then("the zvol volblocksize should be 32768")
def the_zvol_volblocksize_should_be_32768():
    dataset = f"{VBS_POOL_DATASET}/{VBS_REPLICA_UUID}"
    assert zfs_get(dataset, "volblocksize") == "32768"


@then("the zvol volblocksize should be 65536")
def the_zvol_volblocksize_should_be_65536():
    dataset = f"{POOL_DATASET}/{PROP_REPLICA_UUID}"
    assert zfs_get(dataset, "volblocksize") == "65536"


@then("the replica should still be shared over nvmf with the same allowed host")
def the_replica_should_still_be_shared_over_nvmf_with_the_same_allowed_host(
    find_replica,
):
    replica = find_replica(SHARED_REPLICA_UUID)
    assert replica is not None
    assert replica.share == common_pb.NVMF
    assert replica.uri.startswith("nvmf://")
    assert HOST_NQN in replica.allowed_hosts


@then("the replica size should be updated")
def the_replica_size_should_be_updated(resized_replica, find_replica):
    assert resized_replica.size >= 2 * REPLICA_SIZE
    assert find_replica(ZFS_REPLICA_UUID).size >= 2 * REPLICA_SIZE


@then("the resize request should fail")
def the_resize_request_should_fail(attempt_resize_replica):
    assert attempt_resize_replica is not None


@then("the replica size should be unchanged")
def the_replica_size_should_be_unchanged(find_replica):
    assert find_replica(ZFS_REPLICA_UUID).size == REPLICA_SIZE


@then("the replica gets destroyed and the zvol removed")
def the_replica_gets_destroyed_and_the_zvol_removed(find_replica):
    assert find_replica(ZFS_REPLICA_UUID) is None
    assert not zfs_exists(f"{POOL_DATASET}/{ZFS_REPLICA_UUID}")
