//! The one hash the node derives identities, seeds and jitter from.

use pstore_node::fnv1a;

/// ⚠️ FNV-1a 64's **published** values, computed independently of this crate. Three copies of
/// this loop used to live in `swim::derive_id`, `gossip::start` and `policy::jitter`, and only
/// the last was tested -- so a `^=` that became `|=` in the other two changed every derived
/// node id and every loss seed, and the nightly mutation sweep was the only thing that noticed.
#[test]
fn fnv1a_matches_the_published_values() {
    assert_eq!(fnv1a(b""), 0xcbf2_9ce4_8422_2325);
    assert_eq!(fnv1a(b"a"), 0xaf63_dc4c_8601_ec8c);
    assert_eq!(fnv1a(b"foobar"), 0x8594_4171_f739_67e8);
}
