# xstate-inspect

Stately Inspector support for `xstate-core` machines: an observer that emits
`@statelyai/inspect` protocol messages (actor registration, events, snapshots), sinks for
in-memory collection and `.jsonl` recording, and replay of recordings.

Context and event payloads are deny-by-default (allow-lists plus key-based redaction), and the
message bytes are identical to the Python library's `InspectorPlugin`, so recordings made by
either runtime replay in the other and in the Stately Inspector UI.
