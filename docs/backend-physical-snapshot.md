# Physical SQLite snapshot

`export_physical_snapshot` exports a sanitized SQLite database through an explicitly selected existing directory. It has a separate receipt and recovery ledger from the logical `export_backup` JSON, gzip and CSV formats.

```json
{"request_id":"snapshot-1","command":"export_physical_snapshot","directory":"D:\\Music Backups","basename":"profile.sqlite3","maximum_image_bytes":67108864}
```

The basename is one leaf. Publication uses the actual candidate file object and never replaces an existing destination. The receipt identifies the file and selected directory, whole SHA-256, byte count, snapshot revision, schema version and original selection epoch. Success requires durable receipt commit, verified private-object cleanup and the final ledger acknowledgement.

```json
{"query":"physical_snapshot","operation_id":"snapshot-1"}
{"query":"physical_snapshots"}
```

Views distinguish running work, published output, failed work, unconfirmed publication, recovery-needed work and completed private cleanup. `pending_recovery` and `has_live_owner` identify retained recovery accounting and actual in-process ownership. Queries return metadata without reopening destination writers.

The source copy uses one genuine SQLite read transaction with native main/WAL/SHM locking and actual file identities. Copying runs outside the repository writer mutex. Concurrent commits after the read cut remain outside the exported revision. Source WAL growth is bounded to 64 MiB; file I/O checks cancellation in chunks of at most 64 KiB.

The private image verifies the complete production schema, removes URL caches, pairing secrets and library/download execution grants, and removes the physical snapshot recovery ledger. It obtains an independent snapshot profile identity. Compaction removes stale freelist content; integrity, foreign-key and whole-file SHA checks precede publication.

Each operation admits at most a 1 GiB image and reserves `3 × maximum_image_bytes + 65 MiB` of workspace. The shared limit is 4 GiB with at most four unfinished operations. Completed history is bounded to 64 entries. The durable ledger has independent operation CAS and profile/operation/selection epochs, at most 16 latest cleanup facts per operation, a 1 MiB operation document and a 16 MiB aggregate document limit. Progress transactions do not advance the user-data revision.

`cancel_operation` cancels the running export or recovery request. Each worker has a 120-second context. Failure retains the actual owner and charged reservation; cancellation after native publication preserves the final file and reports unfinished acknowledgement.

Recovery requires a fresh explicit selection of the same actual destination directory:

```json
{"request_id":"snapshot-retry","command":"recover_physical_snapshot","snapshot_operation_id":"snapshot-1","directory":"D:\\Music Backups","action":"resume_publication"}
{"request_id":"snapshot-cleanup","command":"recover_physical_snapshot","snapshot_operation_id":"snapshot-1","directory":"D:\\Music Backups","action":"cleanup_private"}
```

`resume_publication` verifies the original compact image or already published original file object. It does not rename a confirmed publication a second time. Before a verified compact image exists, recovery permits private cleanup only. `cleanup_private` cannot delete a published final file.

Startup restores pending quota from the profile's durable ledger before admitting new work. Stored locators, paths and cleanup facts confer no filesystem capability. Fresh selection, current operation CAS and exact manifest/file identities remain mandatory. After receipt commit, acknowledgement uses that committed receipt rather than requiring the old user-data revision to remain current.

Shutdown cancels running contexts and retains tracked workers until actual exit. Native owners may close for cold recovery only after a durable identity/disposition checkpoint. A rejected handoff retains its actual file handles for supervised cleanup or acknowledgement retry. Pending recovery produces a shutdown error; an unresolved live owner prevents successful actor termination.

The native filesystem boundary is Windows NTFS. The actor, ledger, CLI and native kernel fixtures have separate acceptance scopes; type checks do not establish physical filesystem, crash-recovery or hardware results.
