"""ZFS pool support feature tests."""

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
import pool_pb2 as pb

ZPOOL_NAME = "mstest"
PARENT_DATASET = f"{ZPOOL_NAME}/disks"
POOL_NAME = "zfspool"
POOL_DATASET = f"{PARENT_DATASET}/{POOL_NAME}"
DISK_IMAGE = "/tmp/ms0-zfs-disk0.img"

pytestmark = pytest.mark.skipif(
    not os.path.exists("/sys/module/zfs") or shutil.which("zfs") is None,
    reason="ZFS is not available (zfs kernel module not loaded or zfs binary missing)",
)


@scenario("features/zfs.feature", "creating a zfs pool on a parent dataset")
def test_creating_a_zfs_pool_on_a_parent_dataset():
    """creating a zfs pool on a parent dataset."""


@scenario("features/zfs.feature", "creating a zfs pool with option query parameters")
def test_creating_a_zfs_pool_with_option_query_parameters():
    """creating a zfs pool with option query parameters."""


@scenario("features/zfs.feature", "creating a zfs pool that already exists")
def test_creating_a_zfs_pool_that_already_exists():
    """creating a zfs pool that already exists."""


@scenario("features/zfs.feature", "importing an exported zfs pool")
def test_importing_an_exported_zfs_pool():
    """importing an exported zfs pool."""


@scenario("features/zfs.feature", "destroying a zfs pool")
def test_destroying_a_zfs_pool():
    """destroying a zfs pool."""


@scenario("features/zfs.feature", "destroying a zfs pool containing a foreign dataset")
def test_destroying_a_zfs_pool_containing_a_foreign_dataset():
    """destroying a zfs pool containing a foreign dataset."""


@scenario(
    "features/zfs.feature",
    "creating a zfs pool on a parent dataset which does not exist",
)
def test_creating_a_zfs_pool_on_a_parent_dataset_which_does_not_exist():
    """creating a zfs pool on a parent dataset which does not exist."""


@scenario("features/zfs.feature", "listing zfs pools")
def test_listing_zfs_pools():
    """listing zfs pools."""


def zfs_get(dataset, prop):
    """Return the parseable value of a zfs property."""
    out = subprocess.run(
        f"nix-sudo zfs get -Hp -o value {prop} '{dataset}'",
        shell=True,
        check=True,
        capture_output=True,
    )
    return out.stdout.decode("ascii").strip("\n")


def zfs_get_source(dataset, prop):
    """Return the source of a zfs property (local/inherited/default/-)."""
    out = subprocess.run(
        f"nix-sudo zfs get -Hp -o source {prop} '{dataset}'",
        shell=True,
        check=True,
        capture_output=True,
    )
    return out.stdout.decode("ascii").strip("\n")


def zfs_exists(dataset, dstype="filesystem"):
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
            pb.CreatePoolRequest(name=name, disks=disks, pooltype=pooltype)
        )

    yield create


@pytest.fixture
def find_pool(get_mayastor_instance):
    def find(name):
        for pool in get_mayastor_instance.pool_rpc.ListPools(
            pb.ListPoolOptions()
        ).pools:
            if pool.name == name:
                return pool
        return None

    yield find


@given(
    parsers.parse('a mayastor instance "{name}"'),
    target_fixture="get_mayastor_instance",
)
def get_mayastor_instance(mayastor_mod, name):
    return mayastor_mod[f"{name}"]


@given("a ZFS parent dataset backed by a loop device")
def a_zfs_parent_dataset_backed_by_a_loop_device(zpool_parent_dataset):
    return zpool_parent_dataset


@given("a zfs pool", target_fixture="zfs_pool")
def zfs_pool(get_mayastor_instance, zpool_parent_dataset, create_pool):
    pool = create_pool(POOL_NAME, [PARENT_DATASET], pb.Zfs)
    yield pool
    try:
        get_mayastor_instance.pool_rpc.DestroyPool(
            pb.DestroyPoolRequest(name=POOL_NAME)
        )
    except grpc.RpcError:
        pass


@given("a foreign dataset inside the pool container dataset")
def a_foreign_dataset_inside_the_pool_container_dataset(zfs_pool):
    dataset = f"{POOL_DATASET}/foreign"
    run_cmd(f"nix-sudo zfs create '{dataset}'", True)
    yield dataset
    run_cmd(f"nix-sudo zfs destroy '{dataset}'", False)


@when(
    "the user creates a pool specifying the parent dataset and pooltype zfs",
    target_fixture="created_zfs_pool",
)
def the_user_creates_a_pool_specifying_the_parent_dataset_and_pooltype_zfs(
    get_mayastor_instance, zpool_parent_dataset, create_pool
):
    pool = create_pool(POOL_NAME, [PARENT_DATASET], pb.Zfs)
    yield pool
    try:
        get_mayastor_instance.pool_rpc.DestroyPool(
            pb.DestroyPoolRequest(name=POOL_NAME)
        )
    except grpc.RpcError:
        pass


@when(
    "the user creates a pool with compression=zstd as a disks query parameter",
    target_fixture="created_zfs_pool",
)
def the_user_creates_a_pool_with_compression_zstd_as_a_disks_query_parameter(
    get_mayastor_instance, zpool_parent_dataset, create_pool
):
    pool = create_pool(POOL_NAME, [f"{PARENT_DATASET}?compression=zstd"], pb.Zfs)
    yield pool
    try:
        get_mayastor_instance.pool_rpc.DestroyPool(
            pb.DestroyPoolRequest(name=POOL_NAME)
        )
    except grpc.RpcError:
        pass


@when("the user creates a pool with the same name and disks again")
def the_user_creates_a_pool_with_the_same_name_and_disks_again(create_pool):
    create_pool(POOL_NAME, [PARENT_DATASET], pb.Zfs)


@when("the user exports the pool")
def the_user_exports_the_pool(get_mayastor_instance):
    get_mayastor_instance.pool_rpc.ExportPool(pb.ExportPoolRequest(name=POOL_NAME))


@when("the user imports the pool", target_fixture="imported_pool")
def the_user_imports_the_pool(get_mayastor_instance):
    return get_mayastor_instance.pool_rpc.ImportPool(
        pb.ImportPoolRequest(name=POOL_NAME, disks=[PARENT_DATASET], pooltype=pb.Zfs)
    )


@when("the user destroys the pool")
def the_user_destroys_the_pool(get_mayastor_instance):
    get_mayastor_instance.pool_rpc.DestroyPool(pb.DestroyPoolRequest(name=POOL_NAME))


@when("the user attempts to destroy the pool", target_fixture="attempt_destroy_pool")
def the_user_attempts_to_destroy_the_pool(get_mayastor_instance):
    try:
        get_mayastor_instance.pool_rpc.DestroyPool(
            pb.DestroyPoolRequest(name=POOL_NAME)
        )
        return None
    except grpc.RpcError as error:
        return error


@when(
    "the user attempts to create a pool on a dataset which does not exist",
    target_fixture="attempt_create_pool",
)
def the_user_attempts_to_create_a_pool_on_a_dataset_which_does_not_exist(
    zpool_parent_dataset, create_pool
):
    try:
        create_pool(POOL_NAME, [f"{ZPOOL_NAME}/doesnotexist"], pb.Zfs)
        return None
    except grpc.RpcError as error:
        return error


@when("a user calls the listPool() gRPC method", target_fixture="list_pools")
def list_pools(get_mayastor_instance):
    return get_mayastor_instance.pool_rpc.ListPools(pb.ListPoolOptions()).pools


@then("the zfs pool should be created")
def the_zfs_pool_should_be_created(find_pool):
    pool = find_pool(POOL_NAME)
    assert pool is not None
    assert pool.pooltype == pb.Zfs


@then("the container dataset should exist with a local io.mayastor:pool property")
def the_container_dataset_should_exist_with_a_local_io_mayastor_pool_property(
    find_pool,
):
    assert zfs_exists(POOL_DATASET)
    assert zfs_get_source(POOL_DATASET, "io.mayastor:pool") == "local"
    assert zfs_get(POOL_DATASET, "io.mayastor:pool") == find_pool(POOL_NAME).uuid


@then(
    "the container dataset should have compression zstd set locally for children to inherit"
)
def the_container_dataset_should_have_compression_zstd_set_locally(find_pool):
    assert zfs_get(POOL_DATASET, "compression") == "zstd"
    assert zfs_get_source(POOL_DATASET, "compression") == "local"


@then("the create request should succeed without creating a second pool")
def the_create_request_should_succeed_without_creating_a_second_pool(
    get_mayastor_instance,
):
    pools = [
        pool
        for pool in get_mayastor_instance.pool_rpc.ListPools(pb.ListPoolOptions()).pools
        if pool.name == POOL_NAME
    ]
    assert len(pools) == 1


@then("the imported pool should keep its original uuid")
def the_imported_pool_should_keep_its_original_uuid(zfs_pool, imported_pool, find_pool):
    assert imported_pool.uuid == zfs_pool.uuid
    assert find_pool(POOL_NAME).uuid == zfs_pool.uuid


@then("the zfs pool should be removed")
def the_zfs_pool_should_be_removed(find_pool):
    assert find_pool(POOL_NAME) is None


@then("the container dataset should be removed")
def the_container_dataset_should_be_removed():
    assert not zfs_exists(POOL_DATASET)


@then("the destroy request should fail")
def the_destroy_request_should_fail(attempt_destroy_pool):
    assert attempt_destroy_pool is not None


@then("the zfs pool should still be present")
def the_zfs_pool_should_still_be_present(find_pool):
    assert find_pool(POOL_NAME) is not None
    assert zfs_exists(POOL_DATASET)


@then("the create request should fail")
def the_create_request_should_fail(attempt_create_pool, find_pool):
    assert attempt_create_pool is not None
    assert find_pool(POOL_NAME) is None


@then("the zfs pool should be listed with correct pooltype capacity and used fields")
def the_zfs_pool_should_be_listed_with_correct_fields(list_pools):
    pools = [pool for pool in list_pools if pool.name == POOL_NAME]
    assert len(pools) == 1
    pool = pools[0]
    assert pool.pooltype == pb.Zfs
    assert pool.state == pb.POOL_ONLINE
    # capacity floats with the free space of the backing zpool (no quota given)
    # so only check for sane values rather than exact byte counts
    assert pool.capacity > 0
    assert pool.capacity <= 4 * 1024 * 1024 * 1024
    assert pool.used < pool.capacity
    assert pool.committed == 0
