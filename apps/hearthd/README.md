# Retired development daemon

This directory is retained only as a historical development snapshot. It is
explicitly excluded from the supported Cargo workspace and is not built,
tested, packaged, or distributed by ArenaNext.

The supported product is the single `arena-next` executable. It owns fixture
replay, diagnostics, log observation, and validated restart checkpoints in one
native process. Do not reintroduce this socket daemon, Tokio runtime, or HTTP
updater into the release dependency graph.
