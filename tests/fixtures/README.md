The RSA keypair is a public, test-only fixture copied from tower-mcp's JWKS
regression suite (same project owner and MIT/Apache-2.0 license). It is used only
to sign local test tokens; it must never be used for production authentication.

`reload_backend.py` implements the minimal watched-revision ping peer.
`session_backend.py` requires `notifications/initialized` before discovery/calls,
echoes tool arguments, and appends only executed tool calls to its temporary
journal. Both need Python 3's standard library only and exit on stdin EOF.
`shutdown_backend.py` deliberately ignores EOF/signals to exercise bounded
child termination and reap; its owner must clean up the fixture.

Shared Rust server/session/application/backend and OAuth helpers are documented
in CONTRIBUTING.md. All fixtures bind loopback or use stdio, use temporary files,
and have no external service or production-credential dependency. Docker/Redis
is still required for the complete integration suite.
