# Synchronization storage

Schema 12 stores private peers, immutable dataset snapshots, per-peer baselines and the bounded synchronization journal. Schema 13 adds `sync_mode_preferences`, keyed by peer and dataset, with an authentication epoch and a JSON mode value limited to 128 bytes.

Upgrading a schema 12 profile preserves playlists, dislikes, peer credentials, pairing preferences, synchronization sequence, baseline coverage and repository revision. Migration creates an empty mode table and preserves a database backup before changing the schema. Opening a profile does not start a synchronization listener.

Mode preferences require the actual active session, its authentication epoch and the permitted dataset scope. A preference with an earlier authentication epoch is inactive. Forgetting a peer removes its mode rows through the foreign key. Dislike synchronization rejects the two full-list replacement modes.

Mode preference changes do not advance the playlist/dislike synchronization sequence or the user-data repository revision. A stored choice supplies a negotiation preference; the live session remains responsible for validating scope and acknowledging an actual committed transaction.

Three-way user-list metadata selection follows the fixed upstream JavaScript
comparison: if the local field equals the baseline, the remote field is selected;
otherwise the local field is retained. Missing and null optional fields compare
equal for this selection. Numeric `sourceListId` values compare to strings using
ECMAScript StringToNumber, including whitespace, radix prefixes and exponent
notation. Two strings compare exactly. List and music identifiers continue to
use exact identity equality; snapshot serialization and digests preserve missing
and null as distinct representations.

Logical backups omit pairing secrets. Physical snapshot publication requires revoking copied peers, clearing private keys, baseline coverage and mode preferences, and disabling pairing in the copied profile. Sanitization applies to the private copy; the live profile retains its credentials and negotiation state.
