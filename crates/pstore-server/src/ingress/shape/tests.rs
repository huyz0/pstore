#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

use super::*;

const WIDE: Shape = Shape {
    depth: DEPTH,
    elements: usize::MAX,
    members: usize::MAX,
    string_bytes: usize::MAX,
};

fn nested(levels: usize) -> String {
    format!("{}{}", "[".repeat(levels), "]".repeat(levels))
}

#[test]
fn depth_counts_open_arrays_and_objects() {
    let at = |depth| Shape { depth, ..WIDE };
    assert_eq!(scan(b"1", &at(1)), Ok(()), "a scalar body is depth 0");
    assert_eq!(scan(b"{}", &at(1)), Ok(()), "{{}} is depth 1");
    assert_eq!(scan(b"{\"a\":[]}", &at(1)), Err(Breach::Depth));
    assert_eq!(scan(b"{\"a\":[]}", &at(2)), Ok(()));
    // Siblings do not add up: depth is what is open at once.
    assert_eq!(scan(b"[[],[],{}]", &at(2)), Ok(()));
    assert_eq!(scan(nested(DEPTH).as_bytes(), &WIDE), Ok(()));
    assert_eq!(
        scan(nested(DEPTH + 1).as_bytes(), &WIDE),
        Err(Breach::Depth)
    );
}

#[test]
fn elements_and_members_count_one_container_each() {
    let elements = Shape {
        elements: 3,
        ..WIDE
    };
    assert_eq!(scan(b"[1,2,3]", &elements), Ok(()));
    assert_eq!(
        scan(b"[1, 2, 3, 4]", &elements),
        Err(Breach::Elements { depth: 1 })
    );
    assert_eq!(scan(b"[[1,2,3],[1,2,3],[]]", &elements), Ok(()));
    // The deepest level a body may reach is counted too.
    let deepest = format!("{}1,2,3,4{}", "[".repeat(DEPTH), "]".repeat(DEPTH));
    assert_eq!(
        scan(deepest.as_bytes(), &elements),
        Err(Breach::Elements { depth: DEPTH })
    );
    assert_eq!(
        scan(b"{\"a\":[[1,2,3,[]]]}", &elements),
        Err(Breach::Elements { depth: 3 })
    );
    assert_eq!(scan(b"[]", &elements), Ok(()));
    assert_eq!(
        scan(
            b"[ ]",
            &Shape {
                elements: 0,
                ..WIDE
            }
        ),
        Ok(())
    );
    let members = Shape { members: 2, ..WIDE };
    assert_eq!(scan(b"{\"a\":1,\"b\":{\"c\":2,\"d\":3}}", &members), Ok(()));
    assert_eq!(
        scan(b"{\"a\":1,\"b\":2,\"c\":3}", &members),
        Err(Breach::Members { depth: 1 })
    );
    // An object's values are not its members, and an array's objects are not elements.
    assert_eq!(scan(b"{\"a\":[1,2,3,4]}", &members), Ok(()));
    assert_eq!(
        scan(
            b"[{\"a\":1,\"b\":2}]",
            &Shape {
                elements: 1,
                ..members
            }
        ),
        Ok(())
    );
}

#[test]
fn strings_count_their_raw_bytes() {
    let short = Shape {
        string_bytes: 3,
        ..WIDE
    };
    assert_eq!(scan(b"[\"abc\"]", &short), Ok(()));
    assert_eq!(
        scan(b"[\"abcd\"]", &short),
        Err(Breach::String { depth: 1 })
    );
    assert_eq!(
        scan(b"{\"abcd\":1}", &short),
        Err(Breach::String { depth: 1 }),
        "keys too"
    );
    // An escape is counted as the bytes it is spelled with.
    assert_eq!(scan(br#"["a\""]"#, &short), Ok(()));
    assert_eq!(
        scan(br#"["ab\""]"#, &short),
        Err(Breach::String { depth: 1 })
    );
    // A two-byte character is two bytes.
    let one = Shape {
        string_bytes: 1,
        ..WIDE
    };
    assert_eq!(
        scan("[\"é\"]".as_bytes(), &one),
        Err(Breach::String { depth: 1 })
    );
}

#[test]
fn brackets_and_quotes_inside_strings_are_text() {
    let shallow = Shape {
        depth: 1,
        elements: 1,
        members: 1,
        ..WIDE
    };
    assert_eq!(scan(br#"{"a":"[[[{,,,}]]]"}"#, &shallow), Ok(()));
    assert_eq!(
        scan(br#"["\"[[,"]"#, &shallow),
        Ok(()),
        "an escaped quote stays inside"
    );
    assert_eq!(
        scan(br#"["\\",[]]"#, &Shape { depth: 1, ..WIDE }),
        Err(Breach::Depth),
        "an escaped backslash ends"
    );
}

#[test]
fn a_breach_is_reported_whatever_follows_it() {
    let shallow = Shape { depth: 2, ..WIDE };
    assert_eq!(scan(b"[[[ this is not json", &shallow), Err(Breach::Depth));
    assert_eq!(scan(b"[[]] trailing", &shallow), Ok(()), "not a validator");
    assert_eq!(scan(b"[\"unterminated", &shallow), Ok(()));
    assert_eq!(
        scan(b"]]]}}}", &shallow),
        Ok(()),
        "closing too much is the parser's to refuse"
    );
}
