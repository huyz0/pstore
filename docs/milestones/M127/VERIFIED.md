# M127 verification

Current source uses quinn-proto 0.11.18, Tokio 1.53.3 and maintained PEM APIs.
Current tests pass; the locked dependencies passed audit. The transport release
campaign was refreshed after the remote-close classifier correction; authentication
and split-search samples retain their earlier source hashes, as MEASURE.md records.
Earlier default-feature workspace checks, quality gates and delta review passed;
the current delta review and final precommit gates remain pending. Coverage first observed
red at 94.91% lines / 93.90% regions. Thirteen additional behavioral controls
and their bad-mutation checks pass. Coverage then measured 96.41% lines /
94.87% regions, still below the unchanged region floor. The next library run
passed 81 tests and failed the new remote TLS-alert classifier regression as
expected (one benchmark ignored). The classifier correction passes 83 library
tests, eleven controlled bad mutations and strict all-feature server Clippy.
An incremental coverage invocation produced mixed old/new source mappings and
is invalid evidence. The unchanged clean coverage command then passed every
workspace test but measured 96.55% lines / 94.99% regions (five regions short).
The user requested resumed coverage work after the three-attempt stop.
The resumed suite passes 85 library tests, four new fail-open mutations and
strict all-feature server Clippy (.harness/m127-resumed-coverage.log). An
interrupted clean run produced no measurement and is not evidence. The rerun of
the unchanged clean command passed every workspace test and both floors:
96.56% lines / 95.01% regions (.harness/m127-resumed-clean-coverage.log).
Both 95% floors are unchanged.

1. PASS: `a_bare_api_refuses_http_before_engine_access`, `standalone_requires_auth_before_binding`; E. Observed behavioral red on M126: accepted write (200 vs 401), and startup reached bind without authentication.
2. PASS: `bearer_identity_is_bound_before_dispatch`, `invalid_bearers_are_refused_before_blob_access`, `duplicate_credentials_and_conflicting_tenant_headers_are_refused`; E.
3. PASS: `route_scopes_fence_every_public_operation` covers 23 public methods after legacy removal; E.
4. PASS: `source_grants_fence_branch_replication_and_external_copy`, `signed_sources_permit_real_branch_replication_and_external_copy`; E. Exact source pairs are checked before lookup/I/O; refused writes preserve destination/session/registry state.
5. PASS: `authenticated_vector_and_phased_text_parts_match_local_execution`; E. Both transports produced matching results/epochs, successful whole/open/scan counters and consumed held state. Wrong-CA work fell back locally. Legacy public refusal and HTTP/3 wire controls also passed. Cloud interoperability is not claimed.
6. PASS: `auth_configuration_is_explicit_bounded_and_redacted`, `overlapping_keys_and_audience_arrays_are_supported`, `expiry_is_strict_and_not_before_is_inclusive`, `both_directions_require_trusted_current_certificates_and_server_san`, `tls_material_is_bounded_validated_and_redacted`; E. `verification_enforces_the_injected_clock_at_expiry_and_not_before` passes in E; bypassing its predicate was killed historically (.harness/m127-clock-mutation.log), then restored and rebuilt.
7. PARTIAL: E passed all 99 regressions; `cargo deny check` passed (.harness/m127-security-patch.log). Current 83 library tests, eleven fault mutations and all-feature server Clippy pass (.harness/m127-final-controls-green.log); clean all-feature workspace tests pass (.harness/m127-clean-coverage.log). Clean all-feature coverage then passed at 96.56% lines / 95.01% regions (.harness/m127-resumed-clean-coverage.log). Default-feature workspace checks and recall/NDCG/depth gates passed before the final classifier correction. Final precommit gates and delta review remain pending. No advisory ignore or threshold was added or weakened.
8. PASS: `admission_cpu_allocations_and_response_digests`, `peer_transport_microbench` and `split_bench` passed on the patched tree. Provisional paired JWT overhead is 2.541 us, +18 allocations/+1,576 allocated bytes. H2/H3 cold/warm, multiplex, large frame, QUIC loss, unavailable-UDP fallback, wrong-CA refusal and dense/text/hybrid split-search samples are recorded in MEASURE.md. Every split result array matched; no-fallback assertions passed.

E: cargo test -p pstore-server --lib --test peer_startup --test auth_baseline --test auth_contract --test auth_peers --test auth_sources --test peers

Latest E: 59 library + 4 baseline + 7 contract + 1 peer auth + 2 source +
1 standalone + 25 legacy peer tests passed; transport benchmark ignored in E.
Log: .harness/m127-security-patch.log. Strict server Clippy and advisory/licence/
ban/source audit passed in that log. Both release campaigns then passed.

`standalone_serves_http2_and_http3_then_releases_both_listeners_on_sigterm`
proves real process listeners and Unix SIGTERM cleanup; this control is Unix-only.
`standalone_peer_configuration_is_validated_before_store_access_or_binding`
observed backend construction before peer validation, then passed after validation
moved ahead of backend/cache access. Both pass in E.

Component evidence and decisive fault controls:

- `typed_payloads_preserve_float_bits_without_a_json_value_bridge` passes directly and through both pooled transports (E). Signed zero, subnormal, infinity and NaN payload bits survive. Initial missing API was compile red; engine query validation remains active.
- Native protocol, concurrent out-of-order calls, larger-than-window transfer, certificate/SAN refusal, malformed frames before dispatch, wrong route/content/bearer, reply identity/version and receive-credit controls pass in E.
- `http2_advertises_bounded_receive_credit_in_both_directions` observed red: 1048576/1048576 versus 65536/262144. Explicit receive windows and disabled adaptive expansion fixed it; larger-than-window transfer passes.
- Pool preference/reuse, UDP-blackhole fallback/cache, no trust-failure fallback, idle/departed cleanup, LRU cap, invalid address refusal and local-admission isolation pass in E.
- `waiting_for_local_capacity_is_not_an_http3_availability_probe` observed red (3 probes versus 2). Admission now precedes the network probe timer; the overall deadline still covers queueing. Original logs: .harness/m127-pool-first.log and .harness/m127-pool-green.log.
- Sweeping starts from a fully constructed Arc via OnceLock. OBSERVED-NOT: the earlier new_cyclic Weak-startup race was not forced; construction avoids it. Idle-release controls pass.
- Anonymous-client TLS verification mutation was killed historically; source restored and rebuilt. Current certificate controls pass after migration to the maintained parser. Initial TLS/codec API controls were compile red before those APIs existed.
- `frames_preserve_named_data_and_return_the_byte_budget`, `invalid_lengths_truncation_encoding_and_trailing_data_are_refused`, `wire_buffer_admission_waits_until_previous_frames_drop`, `http_chunks_admit_exactly_one_complete_frame_and_return_cancelled_buffers` pass in E.
- `a_timed_out_rpc_releases_admission_and_does_not_poison_later_calls` observed red: a pending handler survived cancellation (.harness/m127-assurance-first.log). Two-second server deadlines fixed it on both transports; current E passes.
- `inbound_and_outbound_connections_share_the_same_admission_ceiling` passed after missing-API compile red. Pool/listener share per-Api connection and incoming wire-buffer limits across cluster replacements.
- `auto_fallback_uses_the_current_call_deadline_after_configuration_changes` observed red: a 200 ms call was consumed by an old 250 ms probe. Current-deadline probe budgeting after admission fixed it (.harness/m127-qualification-first.log, E).
- `peer_configuration_refusals_do_not_echo_embedded_credentials` observed red, then passed after static/gossip errors stopped retaining raw values (E).
- `http3_wire_settings_disable_dynamic_qpack_and_limit_header_sections` and `http3_refuses_dynamic_references_and_incomplete_headers_before_dispatch` pass in E.
- `http3_rejects_encoded_headers_before_waiting_for_an_oversized_payload` observed red, then passed with the early frame guard. Split-prefix controls were compile red. Outgoing-adapter bypass mutation was killed, source restored and rebuilt (.harness/m127-nodelay-cycle.log); current E passes.
- `accepted_peer_tcp_sockets_disable_nagle_before_tls` observed red with the bare accept helper, then passed after TCP_NODELAY was set before TLS (.harness/m127-nodelay-cycle.log); current E passes. Matched historical and fresh security-tree measurements are separated in MEASURE.md.

Advisory gate observed red (.harness/m127-post-audit-validation.log):
RUSTSEC-2026-0338/0340/0343 in quinn-proto 0.11.17,
RUSTSEC-2026-0386 in Tokio 1.53.1, and RUSTSEC-2025-0134 for archived
rustls-pemfile 2.2.0. Precise patch upgrades and replacement with maintained
rustls-pki-types PEM APIs made cargo-deny 0.20.2 pass; current E and Clippy
then passed. No advisory was bypassed. Older cargo-deny 0.18.4 could not parse
the database's CVSS 4 syntax; its failure was tooling evidence, not an audit pass.

Independent implementation review round 1 passed the pre-advisory tree without
blocking or major findings. Round 2 delta review also passed without blocking or major findings; the
maintained parser and dependency fixes preserve the required security behavior.
Full all-feature fast gates and rustdoc passed before the dependency patch
(.harness/m127-remaining-validation.log); they do not certify the current patch.
The current source still requires full-feature final gates and coverage.
Current default-feature workspace checks and quality gates passed in the round 2
qualification. Both protocol search campaigns passed; region coverage was then still red (since fixed above).
The follow-up library suite passes 72 tests (one benchmark ignored), then passes
again after all 13 controlled mutations fail their intended behavioral tests.
Exact-bound TLS files, keyring/claim limits, poisoned-pool recovery, stale-connection
errors, frame/header bounds and peer route/envelope refusals are covered.
Strict server all-targets Clippy passes (.harness/m127-coverage-controls-retry.log).

[MEASURE.md](MEASURE.md) records fresh samples/digests, source/compiler/resource
metadata and historical comparisons. No real-cloud timing is claimed. BYOC
fixture syntax passed; image smoke remains NOT-RUN. No M127 commit/push yet.
