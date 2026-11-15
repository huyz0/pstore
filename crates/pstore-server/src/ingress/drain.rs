//! A refused request's body, read and discarded before the refusal is sent (M238).
//!
//! ⚠️ **Why a refusal would otherwise be lost.** M128 refuses a bad credential, a missing
//! scope, an overloaded tenant, a content encoding or an oversized length before it polls the
//! body, and the connection then closes with the body unread. A reverse proxy still writing
//! that body is reset, and answers its client `502` rather than the refusal (M237 measured it).
//! Read to its end, the body leaves the proxy free to read the answer.
//!
//! ⚠️ **Off by default** (`drain_refused_bodies`): on, a peer holding no credential makes the
//! server read up to `body_bytes` it then throws away, and hold the connection up to the
//! body-read deadline. Nothing read here is kept, reserved or parsed.

use super::{Ingress, declared_over};
use axum::body::Body;
use axum::extract::{Request, State};
use axum::middleware::Next;
use axum::response::Response;
use futures_util::StreamExt;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

/// The outermost public layer: with `drain_refused_bodies` on, a response other than 2xx
/// waits until what is left of its body is read, at most `body_bytes` more and within one
/// more body-read deadline. A body declared longer than `body_bytes`, or one whose client
/// awaits `100 Continue`, is not read: today's close. ⚠️ The drain runs after admission has
/// released the request, so no admission count bounds how many drains run at once; each is
/// bounded only by its bytes and deadline.
pub(crate) async fn drain_refused(
    State(ingress): State<Arc<Ingress>>,
    req: Request,
    next: Next,
) -> Response {
    let limits = *ingress.limits();
    if !limits.drain_refused_bodies()
        || declared_over(req.headers(), limits.body_bytes())
        || expects_continue(req.headers())
    {
        return next.run(req).await;
    }
    let (parts, body) = req.into_parts();
    let held = Arc::new(Mutex::new(body.into_data_stream()));
    let lent = Arc::clone(&held);
    let inner = Body::from_stream(futures_util::stream::poll_fn(move |cx| {
        lent.lock()
            .unwrap_or_else(PoisonError::into_inner)
            .poll_next_unpin(cx)
    }));
    let res = next.run(Request::from_parts(parts, inner)).await;
    if !res.status().is_success() {
        discard(&held, limits.body_bytes(), limits.body_read_timeout()).await;
    }
    res
}

/// `true` when the client waits for `100 Continue` before it sends its body (code review):
/// reading the body would ask it to upload what is then thrown away, and a client that has
/// sent nothing cannot be reset mid-body.
fn expects_continue(headers: &axum::http::HeaderMap) -> bool {
    headers.get_all(axum::http::header::EXPECT).iter().any(|v| {
        v.to_str()
            .is_ok_and(|s| s.trim().eq_ignore_ascii_case("100-continue"))
    })
}

/// Reads and drops what `held` has left, until it ends, fails, passes `max` bytes or runs
/// past `timeout`.
async fn discard(held: &Mutex<axum::body::BodyDataStream>, max: usize, timeout: Duration) {
    let read = async {
        let mut n = 0_usize;
        while n < max {
            let next = futures_util::future::poll_fn(|cx| {
                held.lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .poll_next_unpin(cx)
            })
            .await;
            match next {
                Some(Ok(chunk)) => n = n.saturating_add(chunk.len()),
                Some(Err(_)) | None => return,
            }
        }
    };
    // Past the deadline the body is abandoned, and the connection closes as it did before.
    let _abandoned = tokio::time::timeout(timeout, read).await;
}
