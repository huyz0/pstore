# M114 — a member can send the largest sealed datagram on macOS

**Serves:** CI's `portable (macos-latest)` job, red on `main` at `a4a0af3` and `7e3bb01`:
`swim::the_largest_sealed_part_is_received` panicked at `tests/swim.rs:361` with
`Os { code: 40, kind: Uncategorized, message: "Message too long" }`. No backlog row: a defect.

## What is true today

- **A sealed frame is up to 65,507 bytes** (`MAX_DATAGRAM + SEAL`), the most one IPv4 UDP
  datagram carries. The receive side already sizes its buffer for it.
- **The send side never sizes its socket.** `swim::member` binds a `tokio` `UdpSocket` and
  sends with `send_to`, whose failure is ignored. macOS refuses a UDP datagram larger than
  the socket's send buffer, and its default is 9,216 bytes (`net.inet.udp.maxdgram`). So on
  macOS any frame over 9,216 bytes is dropped at the sender, silently. Linux's default
  (about 208 KiB) hides this.
- **The test fails earlier, at its own socket.** It sends the frame from a raw `std` socket
  with the same default, so on macOS `send_to` errors before the member is involved.

## Delta

1. `swim::bind` binds the member's socket and sets its send buffer to `SEND_BUFFER`,
   256 KiB: four largest frames, so a round that sends to several peers does not refuse
   the next. `member` uses it. A refusal from the OS is an error from `member`, not a
   silent drop later.
2. The test's raw socket gets the same send buffer, through `socket2`, before it sends.
3. `socket2` becomes a direct dependency of `pstore-node`. It is already in the lock file,
   at the same version, through `tokio`, so no new crate is built.

**Not changed:** the wire format, `MAX_DATAGRAM`, the receive path, and what a member does
with a send that fails.

## Acceptance criteria

1. **A member's socket can send a largest frame.** `swim::a_member_socket_holds_the_largest_frame`
   binds through `swim::bind` and finds its send buffer at least `SEND_BUFFER`. Red first:
   on the old socket Linux reports 212,992 bytes, under 262,144.
2. **The largest sealed part is received.** `swim::the_largest_sealed_part_is_received`
   passes, with only its sending socket's setup added.
3. **macOS.** CI's `portable (macos-latest)` job passes the `swim` test binary on the commit
   after M114. ⚠️ The datagram's own red is observable only there: Linux's default already
   holds one frame, so criterion 2 cannot fail here on the old code.
4. `./scripts/gates.sh` passes, and `cargo deny check` still passes with the new edge.
