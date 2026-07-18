Feature: ZFS replica support

  Background:
    Given a mayastor instance "ms0"
    And a ZFS backed pool called "zfspool"

  Scenario: creating a thick replica on a zfs pool
    When a user creates a thick replica on the zfs pool
    Then a zvol should be created with a refreservation

  Scenario: creating a thin replica on a zfs pool
    When a user creates a thin replica on the zfs pool
    Then a sparse zvol should be created without a refreservation

  Scenario: pool default volblocksize is applied to new replicas
    Given a zfs pool created with a default volblocksize of 32k
    When a user creates a replica on the pool with the volblocksize default
    Then the zvol volblocksize should be 32768

  Scenario: per-replica properties override the pool defaults
    When a user creates a replica with a volblocksize property of 64k
    Then the zvol volblocksize should be 65536

  Scenario: share properties survive pool export and import
    Given a replica shared over nvmf with an allowed host
    When the user exports and imports the pool
    Then the replica should still be shared over nvmf with the same allowed host

  Scenario: growing a replica
    Given a zfs backed replica
    When a user resizes the replica to a larger size
    Then the replica size should be updated

  Scenario: shrinking a replica is rejected
    Given a zfs backed replica
    When a user attempts to resize the replica to a smaller size
    Then the resize request should fail
    And the replica size should be unchanged

  Scenario: destroying a replica backed by a zfs pool
    Given a zfs backed replica
    When a user calls destroy replica
    Then the replica gets destroyed and the zvol removed
