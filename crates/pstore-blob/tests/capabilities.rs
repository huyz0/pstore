//! Criterion 6: which backends may be told a write is durable.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "assertions in tests are the reporting mechanism"
)]

use pstore_blob::{Capabilities, Support};

fn caps(cas: Support, create: Support) -> Capabilities {
    Capabilities {
        backend: "probe".to_owned(),
        cas,
        create_if_absent: create,
        delete_is_free: true,
        max_batch_delete: 1000,
        coalesce_gap: 0,
    }
}

#[test]
fn only_a_fully_supported_backend_admits_durable_writes() {
    let states = [
        Support::Supported,
        Support::Divergent("ignores the wildcard".to_owned()),
        Support::Unsupported,
    ];
    // ⚠️ All nine combinations, because the tempting implementation is "not Unsupported" --
    // and `Divergent` is the dangerous state, not the safe one: the call returns success and
    // is wrong. Nine cases is the difference between testing the predicate and testing one
    // row of it.
    for cas in &states {
        for create in &states {
            let c = caps(cas.clone(), create.clone());
            let want = *cas == Support::Supported && *create == Support::Supported;
            assert_eq!(
                c.admits_durable_writes(),
                want,
                "cas={cas:?} create_if_absent={create:?}"
            );
            assert_eq!(c.first_divergence().is_none(), want);
        }
    }
}

#[test]
fn the_divergence_names_the_primitive_that_is_wrong() {
    let c = caps(Support::Supported, Support::Unsupported);
    assert_eq!(
        c.first_divergence().map(|(n, _)| n),
        Some("create_if_absent")
    );
    let c = caps(Support::Divergent("no".to_owned()), Support::Supported);
    assert_eq!(
        c.first_divergence().map(|(n, _)| n),
        Some("compare_and_swap")
    );
    assert!(
        caps(Support::Supported, Support::Supported)
            .first_divergence()
            .is_none()
    );
}

// ⚠️ Feature-gated because `ObjectStoreBackend` is. Mutation sweeps must therefore run with
// `--all-features`, or the mutants in that file are reported MISSED for the uninteresting
// reason that nothing compiled them — a false negative that reads exactly like a weak test.
#[cfg(feature = "object_store")]
#[test]
fn an_unprobed_backend_is_divergent_and_carries_the_declared_defaults() {
    // ⚠️ **Divergent, not Supported.** A backend nobody has probed is one nobody knows
    // about, and `admits_durable_writes` must refuse it -- which is what makes
    // `scripts/conformance.sh` a prerequisite rather than a nicety.
    let c = pstore_blob::ObjectStoreBackend::unprobed("nowhere");
    assert!(!c.admits_durable_writes());
    assert_eq!(
        c.first_divergence().map(|(n, _)| n),
        Some("compare_and_swap")
    );

    // The three fields the matrix records as **declared** rather than measured. They are
    // pinned here because "declared" has to mean a specific value someone chose, or the
    // matrix's own distinction between measured and declared says nothing.
    assert_eq!(
        c.max_batch_delete, 1000,
        "S3 DeleteObjects takes 1000 at a time"
    );
    assert_eq!(
        c.coalesce_gap,
        1024 * 1024,
        "G* is OQ-2; 1 MiB until measured"
    );
    assert!(
        !c.delete_is_free,
        "billed until a probe or a price list says otherwise"
    );
}
