Feature: ZFS snapshot support

  Background:
    Given a mayastor instance "ms0"
    And a ZFS backed pool called "zfspool"
    And a zfs backed replica

  Scenario: creating a replica snapshot
    When a user creates a snapshot of the replica
    Then a zfs snapshot should exist for the replica zvol
    And the zfs snapshot should carry the io.mayastor snapshot properties

  Scenario: listing replica snapshots
    Given a replica snapshot
    When a user lists the snapshots of the replica
    Then the snapshot parameters should round-trip

  Scenario: creating a clone from a snapshot
    Given the replica is shared over nvmf and written with a known pattern
    And a replica snapshot
    And the replica is overwritten with a different pattern
    When a user creates a clone from the snapshot
    Then reading the clone over nvmf should return the pre-snapshot data

  Scenario: destroying a snapshot which has clones
    Given a replica snapshot
    And a clone created from the snapshot
    When a user destroys the snapshot
    Then the snapshot should still be listed as discarded
    And the zfs snapshot should be held with deferred destroy

  Scenario: destroying the last clone of a discarded snapshot
    Given a replica snapshot
    And a clone created from the snapshot
    And the snapshot has been destroyed and marked discarded
    When a user destroys the clone
    Then the snapshot should be gone

  Scenario: destroying a replica with a live snapshot is refused
    Given a replica snapshot
    When a user attempts to destroy the replica
    Then the replica destroy request should fail
    And the replica should still be present

  Scenario: destroying a replica whose snapshots all have clones
    Given a replica snapshot
    And a clone created from the snapshot
    When a user destroys the replica
    Then the replica should be destroyed
    And the clone should still be present and readable
