# Allumette browser client

Versioned guest client for `/browser/v1`, independent of any engine or UI framework.
Requires a secure browser context with WebCrypto Ed25519. Identities and credentials remain
in memory. Hosts own their catalog, invite-link domain and game renderer. Invitations can
be placed in a URL fragment and removed after reading; identity JWTs and signaling tickets
must never be added to navigation URLs or logged.

Poll `listLobbies` while the lobby UI is mounted. Stop/abort requests when it closes.
Capacity, compatibility, readiness and session admission are enforced by the server.
`getSessionContext` resolves the signaling origin and fetches STUN/TURN credentials. On
401, authenticate a new guest; no hidden fallback to STUN or an expired identity.
