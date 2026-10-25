//! Register after the early wire-bound control has been observed red.
#![allow(
    clippy::indexing_slicing,
    reason = "fixture splits exhaustively cover every valid offset"
)]
use super::h3_guard::Frames;

fn length(value: u32) -> Vec<u8> {
    if value < 64 {
        vec![value as u8]
    } else if value < 16384 {
        (value as u16 | 0x4000).to_be_bytes().to_vec()
    } else {
        (value | 0x8000_0000).to_be_bytes().to_vec()
    }
}

#[test]
fn encoded_frame_bounds_are_checked_from_every_fragment_of_the_length_prefix() {
    for kind in [1_u8, 4, 0x2b] {
        for (size, allowed) in [(32 * 1024, true), (32 * 1024 + 1, false)] {
            let mut declaration = vec![kind];
            declaration.extend(length(size));
            for split in 0..=declaration.len() {
                let mut frames = Frames::bidirectional();
                let first = frames.push(&declaration[..split]);
                if split < declaration.len() {
                    assert!(first.is_ok());
                }
                let result = first.and_then(|()| frames.push(&declaration[split..]));
                assert_eq!(
                    result.is_ok(),
                    allowed,
                    "kind={kind},size={size},split={split}"
                );
            }
        }
    }
    for (size, allowed) in [(32 * 1024 * 1024 + 4, true), (32 * 1024 * 1024 + 5, false)] {
        let mut declaration = vec![0];
        declaration.extend(length(size));
        assert_eq!(Frames::bidirectional().push(&declaration).is_ok(), allowed);
    }
}

#[test]
fn frame_scanning_skips_data_and_bounds_control_stream_frames() {
    let mut frames = Frames::bidirectional();
    assert!(frames.push(&[0, 4, 1, 0xff, 0xff, 0xff]).is_ok());
    let mut oversized = vec![1];
    oversized.extend(length(32769));
    assert!(frames.push(&oversized).is_err());
    let mut control = Frames::unidirectional();
    assert!(control.push(&[0]).is_ok());
    assert!(control.push(&oversized).is_err());
    // Encoder/decoder streams have their own format, not HTTP frame lengths.
    for kind in [2, 3] {
        let mut qpack = Frames::unidirectional();
        assert!(qpack.push(&[kind]).is_ok());
        assert!(qpack.push(&oversized).is_ok());
    }
}

#[test]
fn fragmented_long_varints_and_failed_streams_cannot_bypass_frame_bounds() {
    for size in [32768_u64, 32769] {
        let mut declaration = vec![0xc0, 0, 0, 0, 0, 0, 0, 1]; // non-minimal encoding of HEADERS
        declaration.extend((size | 0xc000_0000_0000_0000).to_be_bytes());
        for split in 0..declaration.len() {
            let mut frames = Frames::bidirectional();
            assert!(frames.push(&declaration[..split]).is_ok());
            assert_eq!(frames.push(&declaration[split..]).is_ok(), size == 32768);
            if size == 32769 {
                assert!(frames.push(&[]).is_err());
                assert!(frames.push(&[0, 0]).is_err());
            }
        }
    }
    let mut control = Frames::unidirectional();
    for byte in [0xc0, 0, 0, 0, 0, 0, 0, 0] {
        assert!(control.push(&[byte]).is_ok());
    }
    assert!(control.push(&[1, 0]).is_ok()); // empty HEADERS frame after a fragmented control-stream type
    let mut data = Frames::bidirectional();
    assert!(data.push(&[0, 4, 1]).is_ok());
    assert!(data.push(&[0xff]).is_ok());
    assert!(data.push(&[0xff, 0xff, 1, 0]).is_ok());
}
