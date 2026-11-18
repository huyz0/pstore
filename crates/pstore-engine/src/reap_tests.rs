//! M281: one GC test across every kind of pin.
//!
//! A key the graveyard buries is reaped only when nothing pins it. Every pin is a row below:
//! the key is buried at epoch 1, the pin is applied, and the plan through epoch 1 must keep
//! it. The same key with the pin removed must be reaped, so a row cannot pass by never burying
//! anything. A new kind of key in HEAD fails to compile in `head_keys` until it is a row here.

use super::*;
use head::{ExternalKind, ExternalRef, SegmentRef};

const SEG: &str = "0046/tnt/70/seg/a/1.seg";

fn seg(key: &str) -> SegmentRef {
    SegmentRef {
        key: key.to_owned(),
        rows: 1,
        dicts: None,
        len: None,
        ann: None,
    }
}

/// A HEAD that buries `key` at epoch 1, at epoch 5.
fn burying(key: &str) -> Head {
    Head {
        epoch: Epoch(5),
        graveyard: BTreeMap::from([(1, vec![key.to_owned()])]),
        ..Head::default()
    }
}

fn reaped(h: &Head) -> Vec<String> {
    reap_plan(h, 1)
        .doomed
        .into_iter()
        .map(|k| k.as_str().to_owned())
        .collect()
}

/// A kind of pin: what it buries, and how it holds it.
struct Pin {
    kind: &'static str,
    buried: &'static str,
    apply: fn(&mut Head),
}

fn pins() -> Vec<Pin> {
    vec![
        // A segment an index still names: the commit path's own bug, caught here.
        Pin {
            kind: "a live segment",
            buried: SEG,
            apply: |h| {
                h.indexes.insert("a".to_owned(), vec![seg(SEG)]);
            },
        },
        // A branch (M16) shares its source's segment under the same key.
        Pin {
            kind: "a branch sharing the segment",
            buried: SEG,
            apply: |h| {
                h.indexes.insert("b".to_owned(), vec![seg(SEG)]);
                h.branched.insert("b".to_owned(), 0);
            },
        },
        // A branch letting go of a borrowed segment buries a marker: the segment waits for a
        // second burial still inside the window (M16, spec review B1).
        Pin {
            kind: "a later burial inside the window",
            buried: SEG,
            apply: |h| {
                h.graveyard
                    .entry(3)
                    .or_default()
                    .push(head::scoped("b", SEG));
            },
        },
        // as_of: a burial above the floor is inside the retention window.
        Pin {
            kind: "an as_of reader inside the window",
            buried: SEG,
            apply: |h| {
                if let Some(keys) = h.graveyard.remove(&1) {
                    h.graveyard.insert(2, keys);
                }
            },
        },
        Pin {
            kind: "a delete vector",
            buried: "0046/tnt/70/seg/a/1.seg.3.dv",
            apply: |h| {
                h.deletes.insert(
                    SEG.to_owned(),
                    ("0046/tnt/70/seg/a/1.seg.3.dv".to_owned(), 1),
                );
            },
        },
        Pin {
            kind: "a quarantine",
            buried: "0046/tnt/70/q/a/1.q",
            apply: |h| {
                h.quarantine
                    .insert("a".to_owned(), vec![("0046/tnt/70/q/a/1.q".to_owned(), 1)]);
            },
        },
        Pin {
            kind: "a listed outcome",
            buried: "0046/tnt/70/out/1.out",
            apply: |h| h.outcomes.push((1, "0046/tnt/70/out/1.out".to_owned())),
        },
        // ⚠️ Red before M281: an external index's codes were buried on a failed commit
        // whose write may have landed, and nothing checked HEAD still named them.
        Pin {
            kind: "an external index's codes",
            buried: "0046/tnt/70/ext/e/1.codes",
            apply: |h| {
                h.externals.insert(
                    "e".to_owned(),
                    ExternalRef {
                        kind: ExternalKind::Parquet,
                        source: "customer/data.parquet".to_owned(),
                        column: "v".to_owned(),
                        codes: "0046/tnt/70/ext/e/1.codes".to_owned(),
                        rows: 1,
                        source_len: 1,
                    },
                );
            },
        },
        // A replica (M22) or a copy (M166) holds its own segments, named in `indexes`.
        Pin {
            kind: "a replica's segment",
            buried: SEG,
            apply: |h| {
                h.indexes.insert("r".to_owned(), vec![seg(SEG)]);
                h.replications.insert(
                    "r".to_owned(),
                    head::Replication {
                        source: head::ReplicaSource {
                            store: String::new(),
                            tenant: pstore_types::TenantId(71),
                            index: "a".to_owned(),
                        },
                        running: true,
                        run: 1,
                        applied: None,
                        rejected: 0,
                    },
                );
            },
        },
    ]
}

#[test]
fn every_kind_of_pin_keeps_a_buried_key_and_nothing_else_does() {
    let mut held = Vec::new();
    for pin in pins() {
        let bare = burying(pin.buried);
        assert_eq!(
            reaped(&bare),
            vec![pin.buried.to_owned()],
            "{}: without the pin the key is reaped",
            pin.kind
        );
        let mut pinned = bare.clone();
        (pin.apply)(&mut pinned);
        if !reaped(&pinned).is_empty() {
            held.push(pin.kind);
        }
    }
    assert!(held.is_empty(), "reaped while pinned: {held:?}");
}

#[test]
fn every_key_head_names_is_live() {
    // Every pin at once: the live set is exactly the keys they name.
    let mut h = Head::default();
    for pin in pins() {
        (pin.apply)(&mut h);
    }
    let live: BTreeSet<&str> = head_keys(&h).collect();
    let expected: BTreeSet<&str> = [
        SEG,
        "0046/tnt/70/seg/a/1.seg.3.dv",
        "0046/tnt/70/q/a/1.q",
        "0046/tnt/70/out/1.out",
        "0046/tnt/70/ext/e/1.codes",
    ]
    .into();
    assert_eq!(live, expected);
}
