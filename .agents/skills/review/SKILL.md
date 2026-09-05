---
name: review
description: Review a change with a bounded number of rounds. Use before every commit that changes code — defines what the reviewer is given, what it is deliberately denied, and the round budget that stops the loop.
---

# Review

Run `scripts/review.sh context` to build the packet, then have an agent that did **not**
write the change review it.

## What the reviewer is given, and denied

| Given | Denied |
|---|---|
| The task or intent, in one line | **The author's reasoning.** A reviewer told why something is fine tends to agree. |
| The staged diff | The conversation that produced it |
| Which gates already passed | — |
| On a verify round: **only the delta** since the last round | The whole diff again |

## ⚠️ The round budget — why an unbounded review loop never terminates

Measured in the project this harness was distilled from: **one task reached eleven
review rounds.**

> It always finds something. Reviewers verify by mutation and the marginal round is
> never empty. **What decays is severity** — on the ten-round task, rounds 7–10 found
> only false sentences in the prose describing the review, each correction voiding the
> verdicts and buying the next round.

So "loop until clean" has no fixed point. Three bounds:

| Bound | Value | On breach |
|---|---|---|
| **Review rounds per change** | **2 nominal, 4 hard** | Stop. `scripts/review.sh` refuses round 5. |
| Red→green attempts per task | **3** | Stop and report. Three failures means the task or the spec is wrong, not the code. |
| Changes between checkpoints | **5** | Emit a progress report and **continue** — a report, not a question. |

⚠️ **The budget must refuse the round, not just the commit.** In the source project the
cap refused the *commit* while nothing refused the next *round*, so an author could
spend pairs of agents indefinitely and each round felt justified because it found
something.

⚠️ **The remedy for a blocking finding in round two is SPLIT, not a third round.** A
change that cannot survive two rounds is too big.

## Severity, and why chasing minors extends the loop

**Only `blocking` and `major` stop a commit.** A `pass` carrying `minor` findings
**lands** — minors go in the commit body or become a backlog row.

⚠️ Fixing a minor is permitted and **usually wrong**, because the new round's surface is
the prose the fix just added. That is the exact mechanism that produced rounds 9–11 in
the source project.

> **An empty findings list is a valid and expected outcome.** Do not hunt for minors to
> justify the round.

## What to look for that the gates cannot see

The deterministic gates already ran — do not re-check them. Look for:

1. **Task-versus-diff mismatch.** Does it do what was asked, and only that?
2. **Would this test fail if the code were wrong?** A test that executes code without
   constraining it passes coverage and catches nothing.
3. **Does a request rate scale with the wrong thing?** Records, documents, or index
   count instead of nodes and bytes.
4. **Is an architectural invariant quietly broken?** Round-trip depth, a LIST on a hot
   path, an `unsafe` outside `pstore-kernel`, a cross-AZ byte.
5. **Was a threshold moved, or a test weakened, to make something pass?**

→ [`../../../docs/research/09-rust-stack/engineering-standards.md`](../../../docs/research/09-rust-stack/engineering-standards.md) § 7
