# Expected state fixtures

`sample-arena-session.json` is the raw `ArenaSnapshot` expected after replaying
the available sanitized session logs. It is not the resolved observer envelope
and therefore does not include card metadata.

The sample starts after the run-start boundary. A complete replay of the files
we have is still not proof of earlier draft history, so its
`draft.historyStatus` and `draft.phaseProgressStatus` are intentionally
`unknown`, with `pickNumber: 0`. This is the truthful result, not a missing
default of pick one.

Keep `schemaVersion` in this fixture synchronized with
`hs_state::SNAPSHOT_SCHEMA_VERSION` whenever the raw snapshot shape changes.
