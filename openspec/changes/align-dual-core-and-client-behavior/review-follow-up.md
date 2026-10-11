# Review follow-up: issues #70–#73

Baseline: local `df0b7a73` on `fix/review-round5`, 2026-10-11. Remote issue comments are acceptance criteria, not proof of completed implementation.

## Rule edit persistence contract (A4/C4)

The CLI stores versioned, per-profile rule overlays as regular files in `profiles/`, preserving GUI-compatible source YAML and existing Rules/Merge/Script fragments. Overlays run after the complete enhancement chain, before core-specific conversion. Profile identity determines the filename; profile switching must never reuse another profile's overlay.

An overlay records the original composed Clash rule strings and the edited sequence as references to individual original occurrences or literal added rules. Deletion and reordering operate on exact strings and occurrence identity, preserving duplicates. On source refresh, vanished source occurrences disappear; unchanged occurrences retain the edited order, added rules persist, and newly supplied source occurrences retain their own relative order near the next surviving source anchor, before a catch-all when no anchor survives. A changed source string counts as a new source rule; semantic rewrites of provider rules are not guessed. Upstream rules must not be frozen into a whole-list replacement.

Malformed overlays fail before runtime mutation. Persisted overlays and sing-box logical ordering use atomic writes and roll back the overlay if logical-order persistence fails. Native sing-box JSON remains outside the Clash rule editor. Existing contiguous local YAML edits remain compatible; non-contiguous chain contributions use an overlay rather than duplicating fragments. Backups include the regular overlay files alongside profile files.

## Acceptance boundaries

- A5 and C7 have local implementation commits; rerun their targeted regressions on the integrated result.
- A1/A2/A3, B1/B3/B4 and C2/C5/C6 require fresh code/test evidence.
- B2/B5 remain conservative capability limits, not missing implementation claims.
- C3 user decision (2026-10-11): provide an explicit configurable external-origin allowlist shared by both cores; unset/empty settings retain loopback-only defaults. Subscription CORS data cannot authorize additional origins. Reject wildcard, credential-bearing or non-origin URLs.
- C1 and #73 privileged TUN/user-environment acceptance remain open unless actually exercised. Isolated real-core DNS checks do not certify all of #73.
- Main-spec sync and archival must not claim that unverified 9.2–9.6 have passed.
