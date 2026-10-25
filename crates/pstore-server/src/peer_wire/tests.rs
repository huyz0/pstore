//! Malformed framing must fail before allocation or dispatch.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]
use super::{MAX_FRAME, encode, read_frame};
use serde_json::{Value, json};
use std::sync::Arc;
use tokio::io::AsyncWriteExt;
use tokio::sync::Semaphore;

async fn receive(bytes: Vec<u8>, budget: Arc<Semaphore>) -> Result<super::Frame, super::WireError> {
    let (mut writer, mut reader) = tokio::io::duplex(bytes.len().max(1));
    writer.write_all(&bytes).await.unwrap();
    writer.shutdown().await.unwrap();
    read_frame(&mut reader, budget)
        .await?
        .ok_or(super::WireError::InvalidFrame)
}

#[tokio::test]
async fn http_chunks_admit_exactly_one_complete_frame_and_return_cancelled_buffers() {
    let value = json!({"version":1,"id":7,"tenant":"9","body":{"phase":"scan"}});
    let wire = encode(&value).unwrap();
    for split in 0..=wire.len() {
        let budget = Arc::new(Semaphore::new(wire.len()));
        let mut body = super::Accumulator::default();
        body.push(&wire[..split], budget.clone()).await.unwrap();
        body.push(&wire[split..], budget.clone()).await.unwrap();
        let frame = body.finish().unwrap();
        assert_eq!(frame.decode::<Value>().unwrap(), value);
        drop(frame);
        assert_eq!(budget.available_permits(), wire.len());
    }
    let budget = Arc::new(Semaphore::new(wire.len()));
    let mut incomplete = super::Accumulator::default();
    incomplete.push(&wire[..5], budget.clone()).await.unwrap();
    assert_eq!(budget.available_permits(), 4);
    assert!(incomplete.finish().is_err());
    assert_eq!(budget.available_permits(), wire.len());
    let mut trailing = super::Accumulator::default();
    trailing.push(&wire, budget.clone()).await.unwrap();
    assert!(trailing.push(&[0], budget.clone()).await.is_err());
    drop(trailing);
    assert_eq!(budget.available_permits(), wire.len());
    let mut oversized = super::Accumulator::default();
    assert!(
        oversized
            .push(&((MAX_FRAME + 1) as u32).to_be_bytes(), budget.clone())
            .await
            .is_err()
    );
    assert_eq!(budget.available_permits(), wire.len());
}

#[tokio::test]
async fn frames_preserve_named_data_and_return_the_byte_budget() {
    let value = json!({"version":1,"id":u64::MAX,"tenant":u128::MAX.to_string(),
        "body":{"phase":"open","scores":[0,u32::MAX],"attributes":{"value":"λ"}}});
    let wire = encode(&value).unwrap();
    assert_eq!(
        u32::from_be_bytes(wire[..4].try_into().unwrap()) as usize,
        wire.len() - 4
    );
    let budget = Arc::new(Semaphore::new(wire.len()));
    let frame = receive(wire.clone(), budget.clone()).await.unwrap();
    assert_eq!(budget.available_permits(), 4);
    assert_eq!(frame.decode::<Value>().unwrap(), value);
    drop(frame);
    assert_eq!(budget.available_permits(), wire.len());
}

#[tokio::test]
async fn invalid_lengths_truncation_encoding_and_trailing_data_are_refused() {
    let budget = Arc::new(Semaphore::new(64));
    for wire in [
        vec![0, 0],
        0_u32.to_be_bytes().to_vec(),
        ((MAX_FRAME + 1) as u32).to_be_bytes().to_vec(),
        vec![0, 0, 0, 8, 1, 2],
    ] {
        assert!(receive(wire, budget.clone()).await.is_err());
        assert_eq!(budget.available_permits(), 64);
    }
    // 0xc1 is reserved, and two values in one frame are not one message.
    for wire in [vec![0, 0, 0, 1, 0xc1], vec![0, 0, 0, 2, 1, 2]] {
        let frame = receive(wire, budget.clone()).await.unwrap();
        assert!(frame.decode::<Value>().is_err());
    }
    assert_eq!(budget.available_permits(), 64);
    assert!(encode(&"x".repeat(MAX_FRAME + 1)).is_err());
}

#[tokio::test]
async fn wire_buffer_admission_waits_until_previous_frames_drop() {
    let encoded = encode(&json!({"kept":true})).unwrap();
    let size = encoded.len() - 4;
    let budget = Arc::new(Semaphore::new(size));
    let first = receive(encoded.clone(), budget.clone()).await.unwrap();
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    let waiting = tokio::spawn({
        let budget = budget.clone();
        async move {
            ready_tx.send(()).unwrap();
            receive(encoded, budget).await
        }
    });
    ready_rx.await.unwrap();
    tokio::task::yield_now().await;
    assert!(!waiting.is_finished());
    drop(first);
    let second = waiting.await.unwrap().unwrap();
    assert_eq!(budget.available_permits(), 0);
    drop(second);
    assert_eq!(budget.available_permits(), size);
}

#[test]
fn encoder_refuses_one_byte_beyond_a_complete_maximum_frame() {
    // A MessagePack str32 uses five bytes before its payload.
    let maximum = 32 * 1024 * 1024;
    let value = "x".repeat(maximum - 5);
    let frame = encode(&value).unwrap();
    assert_eq!(frame.len(), maximum + 4);
    assert_eq!(
        u32::from_be_bytes(frame[..4].try_into().unwrap()) as usize,
        maximum
    );
    let oversized = "x".repeat(maximum - 4);
    assert!(encode(&oversized).is_err());
}

#[tokio::test]
async fn deep_frames_and_closed_byte_admission_fail_closed() {
    // The scalar occupies one value level in addition to the enclosing arrays.
    for (depth, allowed) in [(0, true), (63, true), (64, false), (128, false)] {
        let mut value = json!(null);
        for _ in 0..depth {
            value = Value::Array(vec![value]);
        }
        let encoded = encode(&value).unwrap();
        let budget = Arc::new(Semaphore::new(1024));
        let frame = receive(encoded, budget.clone()).await.unwrap();
        assert_eq!(frame.decode::<Value>().is_ok(), allowed, "depth={depth}");
        drop(frame);
        assert_eq!(budget.available_permits(), 1024);
    }
    let encoded = encode(&json!({"refuse":"closed budget"})).unwrap();
    let budget = Arc::new(Semaphore::new(1024));
    budget.close();
    let mut accumulator = super::Accumulator::default();
    assert!(matches!(
        accumulator.push(&encoded, budget.clone()).await,
        Err(super::WireError::Closed)
    ));
    assert!(matches!(
        super::read_frame(&mut encoded.as_slice(), budget.clone()).await,
        Err(super::WireError::Closed)
    ));
    assert_eq!(budget.available_permits(), 1024);
}
