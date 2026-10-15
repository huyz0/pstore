# M89 — The newest version a Lance dataset's hint names

**Serves:** backlog row 73's last piece, after [M82](../M82/SPEC.md) and [M87](../M87/SPEC.md):
the caller no longer has to name a manifest.

## Decision

Picking the newest manifest properly is a LIST of `_versions/`, which a read path may not do. A
dataset also keeps `_versions/latest_version_hint.json`, `{"version": N}`, and V2 manifest names
count down from the largest `u64` (`u64::MAX - N`). So `lance::latest_manifest(store, dataset)`
reads the hint (at most 1 KiB), derives the manifest's key, and checks it exists; `ExternalColumn::
pin_lance_latest` pins at it.

⚠️ **A hint, not the truth.** Lance treats the file as an optimisation, so a writer that has not
refreshed it can leave this one version behind. That is a consistent older snapshot, never a wrong
one, and the doc comment says so. A hint that names a manifest that is gone (garbage-collected) is
an error, never a fall-back to a guess.

## Acceptance criteria

1. `the_hint_names_the_newest_version`: for the real two-version `deleted` dataset the hint's
   manifest is the newest (`...613`, the one with the deletions) and `pin_lance_latest` sees them;
   a trailing slash on the dataset is fine.
2. `a_hint_that_lies_is_refused_never_guessed_around`: not JSON, no version, zero, negative,
   float, string, extra keys, a plus sign, no number, empty, version 99 with no manifest, a
   missing hint and one over 1 KiB (exactly 1 KiB is read) are each an error.
3. **Gates:** `./scripts/gates.sh` passes; the mutation sweep of the changed lines misses 0.

**Not covered:** Lance 2.1+ (a different column encoding, with no reader here); nested Parquet
columns (they carry repetition levels; no plan).
