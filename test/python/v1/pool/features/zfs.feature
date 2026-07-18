Feature: ZFS pool support

  Background:
    Given a mayastor instance "ms0"
    And a ZFS parent dataset backed by a loop device

  Scenario: creating a zfs pool on a parent dataset
    When the user creates a pool specifying the parent dataset and pooltype zfs
    Then the zfs pool should be created
    And the container dataset should exist with a local io.mayastor:pool property

  Scenario: creating a zfs pool with option query parameters
    When the user creates a pool with compression=zstd as a disks query parameter
    Then the zfs pool should be created
    And the container dataset should have compression zstd set locally for children to inherit

  Scenario: creating a zfs pool that already exists
    Given a zfs pool
    When the user creates a pool with the same name and disks again
    Then the create request should succeed without creating a second pool

  Scenario: importing an exported zfs pool
    Given a zfs pool
    When the user exports the pool
    And the user imports the pool
    Then the imported pool should keep its original uuid

  Scenario: destroying a zfs pool
    Given a zfs pool
    When the user destroys the pool
    Then the zfs pool should be removed
    And the container dataset should be removed

  Scenario: destroying a zfs pool containing a foreign dataset
    Given a zfs pool
    And a foreign dataset inside the pool container dataset
    When the user attempts to destroy the pool
    Then the destroy request should fail
    And the zfs pool should still be present

  Scenario: creating a zfs pool on a parent dataset which does not exist
    When the user attempts to create a pool on a dataset which does not exist
    Then the create request should fail

  Scenario: listing zfs pools
    Given a zfs pool
    When a user calls the listPool() gRPC method
    Then the zfs pool should be listed with correct pooltype capacity and used fields
