# Spatial audio controls

```json
{"request_id":"spatial-1","command":"set_spatial_settings","settings":{"enabled":true,"radius":0.7,"speed":25}}
```

Radius is 0.1 to 3.0 and speed is an integer from 1 to 50. The LX slider radius maps by division by ten; speed 25 advances one degree every 50 ms. Settings are persisted separately from actual output acknowledgement.

```json
{"query":"spatial_settings"}
```

```json
{"query":"spatial_state"}
```

The state returns desired preferences, requested and applied sequences, and a bounded preparation error. Applied sequence is published only after an actual effect frame has passed stream checks, committed its media coordinate where applicable and reached the final device slot. Idle configuration does not claim that audio has been presented.

The output owner prepares the spatial coefficient table and effect bank outside the callback. A bounded SPSC queue sends copyable targets to the unique renderer. The owner phase uses monotonic wall time, continues during pause and across tracks, and handles speed/radius changes with the LX restart and next-tick rules. The callback interpolates prepared coefficients without clock or trigonometric calls.

The enhanced graph retains independent left/right lanes until final device folding and uses the linked compressor before spatialization. Disabling spatialization returns the position to the origin; an already activated enhanced graph retains its admitted audio delay for that stream lifetime. Rejected preparations keep the previous actual bank and report their error instead of acknowledging a missing effect.
