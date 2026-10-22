# Sync endpoint services

Sync endpoints are explicitly started through the interactive JSON Lines entry point. Startup of the player and read-only sync queries do not open a listener.

```json
{"service":"sync_server_start","config":{"server_id":"personal-desktop","server_name":"Desktop","pairing_epoch":1,"port":0}}
```

The default address is IPv4 loopback. A non-loopback address requires `bind_lan:true`. `port:0` selects an available port. List and dislike scopes default to enabled and can each be disabled with `list:false` or `dislike:false`. An omitted pairing code generates six digits and returns them only in the successful start response. A supplied code must be six digits.

```json
{"service":"sync_client_start","config":{"url":"http://127.0.0.1:9527","pinned_address":"127.0.0.1:9527","device_name":"Desktop client","pairing_code":"123456","pairing_epoch":1}}
```

The client configuration names an explicit origin and concrete pinned socket address. For an existing pairing, replace `pairing_code` with the public `server_id` selected from saved peer metadata. Exactly one of those fields is required. Saved credentials are loaded from the active private repository record; keys are never accepted in the request or returned by queries. The contacted server must prove the selected server identity before authentication. The native service API also supports prepared TLS configuration; an HTTPS origin requires verified TLS configuration through that API.

`query:sync` returns endpoint state, address, owner epoch, connections, peer count and resource usage. `query:sync_peers` returns public peer metadata, authentication/session epochs, dataset capabilities and chosen modes. Neither query returns pairing codes, shared keys or source permissions.

`sync_choose_mode` takes a `config` object with `owner_epoch`, `peer_id`, `auth_epoch`, `session_epoch`, `dataset` and `mode`. It applies only to the actual matching live session. Dataset is `list` or `dislike`; the two full replacement modes apply only to playlists. Choices remain pending until explicitly supplied or recovered from a matching authentication epoch.

```json
{"service":"sync_revoke_peer","peer_id":"peer-id"}
```

Revocation closes the peer and invalidates its credentials. `sync_stop` closes the current endpoint. OpenAPI stop and rotation operate independently. Standard input EOF closes active sync and OpenAPI workers within the shared shutdown deadline. A shutdown timeout retains endpoint ownership for retry instead of reporting that unfinished workers joined.
