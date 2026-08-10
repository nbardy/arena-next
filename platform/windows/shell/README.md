# ArenaNext Windows shell

This is the small native Windows edge for the shared ArenaNext model. It is
deliberately not a widget toolkit or a second process. The eventual adapter
will use `Shell_NotifyIcon` for the notification-area item and a
`WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_NOACTIVATE` window for the
click-through overlay, updated from the shared RGBA render buffer.

The crate is compiled only with the `windows` dependency on Windows. On other
platforms it exposes the same capability types and returns an explicit
`UnsupportedPlatform` error. Window capture remains a separate capability;
the shell must not claim it is available until a Windows Graphics Capture
adapter has been added.
