# Upstream source

Vendored into OmaBeam from OmaSend's LocalSend core snapshot so live-share
links can be sent with the LocalSend protocol without the LocalSend app.

Source: https://github.com/localsend/localsend/tree/6279d3e30d1d1290caee3b81549f8128a8b01d9f/packages/core

Imported on 2026-09-08 with the upstream repository's MIT LICENSE.
Vendoring only this package avoids Cargo recursively downloading the upstream
Flutter SDK submodule. Update this directory from a reviewed upstream revision;
record any future local patches here.

Local patch: discovery emits `ProbeFailed` for failed announcement verification,
so the app can display a scoped diagnostic. Certificate verification and device
acceptance rules are unchanged.

Local patch: WebRTC-only imports, private fields and helpers are feature-gated
to avoid unused-code warnings in LocalSend-only builds.

Local patch: probe-only discovery for devices that run no server.
`DiscoveryConfig::receivable` (`true` keeps upstream behavior) set to `false`
makes targeted discovery, subnet scans and announcement answers confirm peers
with `GET /info` instead of `POST /register`, so no peer stores the device at
a port nobody serves. Announcement answers stay pinned to the announced
fingerprint. `LsHttpClientV2::info` now returns `ResultWithPublicKey`, carrying
the peer certificate's fingerprint and public key in HTTPS mode like
`register`, so the HTTPS identity still comes from the certificate.
`InfoResponseDtoV2::fingerprint` defaults when absent, like the register
response's.

Local patch: generated identities are ECDSA P-256 (rcgen with ring) instead of
RSA-2048: generated in well under a millisecond, and peers trust certificates
by fingerprint, not key type. `crypto::cert::identity_fingerprint` checks a
stored identity (valid certificate, matching private key) and returns its
fingerprint. The RSA-PSS token verifier, used only by WebRTC, and the `rsa`
dependency moved from the `crypto` feature to `webrtc`, so builds without
WebRTC no longer carry `rsa` (RUSTSEC-2023-0071, no fixed release).
`tests/v2_tls_pinning.rs` keeps an RSA fixture identity to cover RSA and ECDSA
peers talking to each other.
