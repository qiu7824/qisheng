# Backup file export

`export_backup` captures a revision-bound logical snapshot and publishes a new file in an explicitly selected existing directory. The basename is one leaf; an existing target is never replaced. File work runs on a tracked worker and does not open a window.

```json
{"request_id":"export-1","command":"export_backup","directory":"D:\\Music Backups","basename":"playlists.lxmc","format":{"kind":"lx_play_list"},"encoding":"gzip"}
```

Formats are `lx_all_data`, `lx_settings`, `lx_play_list`, `lx_play_list_part` with `list_id`, `native`, and `csv` with `list_id`. JSON and gzip are explicit encodings; CSV requires `json` and produces plain UTF-8 CSV. LX playlist archives exclude the temporary list. Native archives preserve temporary-list metadata.

LX export preserves imported unknown settings and list extension fields. Current implemented player preferences and current list identities take precedence over archived values. Source approvals, URL approvals, file grants, pairing secrets and bearer tokens are removed from settings exports.

Native exports accept `include_sources`, `include_downloads`, and `include_edited_lyrics`, each defaulting to false. Source bytes are archived without approval records. Download records contain inactive plans and omit destination grants. Local paths are metadata and require fresh file selection after restore. Pairing keys and running service authority are absent from logical exports.

The worker captures playlists, dislikes, selected downloads and edited lyrics in one repository read transaction. Player preferences are captured at the accepted command revision; if a later repository change precedes the read transaction, export fails and a new request is required. Successful export does not change the profile revision.

```json
{"query":"backup_export","operation_id":"export-1"}
```

Results distinguish `running`, `published`, `published_unconfirmed`, `failed`, and `cleaned`. A published receipt contains the actual file identity, byte count, whole SHA-256, selected directory identity and selection epoch. Imported JSON cannot recreate live directory or file ownership.

`cancel_operation` cancels the active export or recovery request. Each operation has a 120-second deadline, checked during capture and file I/O. At most four operations may retain resources. The shared logical output reservation is 16 MiB; a capture reserves its full upper bound before encoding. Failed parts retain their reservation and actual file owner.

An unpublished failed part supports explicit cleanup using a fresh directory selection. The selected directory must identify the original physical root.

```json
{"request_id":"cleanup-1","command":"cleanup_backup_export","export_operation_id":"export-1","directory":"D:\\Music Backups"}
```

`retry_backup_export` verifies an already published but unconfirmed output through its retained owner. It neither republishes nor deletes that final file. Published outputs cannot use temporary cleanup. A failed private cleanup retains ownership for a later retry. Backend shutdown cancels workers and waits for their actual exit; remaining unconfirmed parts produce a shutdown error and are preserved.
