---
name: tdd
description: Write the test first and observe it fail before writing the code. Use when writing any code — covers the red-green cycle, what to assert, and why coverage alone does not answer the question.
---

# Test-first

## The cycle

```
1. Write the test.  Run it.  WATCH IT FAIL, and read the failure message.
2. Write the least code that makes it pass.
3. Run the whole suite.
4. Refactor with the suite green.
```

⚠️ **Step 1's "watch it fail" is the whole point.** A test never seen red is not known
to test anything — it may be asserting something already true, or nothing at all. This
is the same rule as [`gate-design`](../gate-design/SKILL.md) step 2, for the same reason.

## What to assert

**Assert the property, not the implementation.** A test that mirrors the code changes
whenever the code changes and catches nothing.

| Instead of | Assert |
|---|---|
| "the function returns 3 fields" | the invariant the caller depends on |
| "the loop ran 4 times" | the observable outcome |
| "it did not panic" | what it produced |

For this project specifically, the highest-value tests are the **architectural
invariants** — round-trip depth ≤3, RA(write) = 1 W, zero LIST on hot paths, query
memory O(k + resident). Those are named tests, and they are the ones never weakened to
land a change.
→ [engineering-standards](../../../docs/research/09-rust-stack/engineering-standards.md) § 7

## Choosing the tier

| Tier | Use for | Cost |
|---|---|---|
| Unit | Pure functions: key derivation, placement, encoding | µs |
| **Property** (`proptest`) | Anything with an algebraic law — encode/decode round-trips, merge ordering, commit linearizability | ms |
| **Differential fuzz** | SIMD kernels against a scalar reference. ⚠️ A wrong lane index returns **bad recall, not a crash** — fuzzing is the primary defence, not a nicety | minutes |
| **Simulation** | Distributed invariants under adversarial schedules, partitions, gray faults | seconds |
| Integration | The client path over `pstore-fake-s3` | seconds |

Prefer the cheapest tier that can express the property, and **prefer a property test to
three examples** — it covers the cases you did not think of.

## ⚠️ Coverage does not answer the question

95% line coverage is a hygiene floor, not an assurance argument. It says a line *ran*;
it does not say a test would have *noticed* if the line changed.

`cargo mutants` answers that question, and is the gate that makes the percentage mean
something. **A test that executes code and asserts nothing scores on coverage and fails
on mutation** — that is exactly the distinction being bought.

> **Never write a test whose purpose is to touch a line.** If it asserts nothing, delete
> or strengthen it. Adding another is the wrong response.

## Bounds

⚠️ **Three red→green attempts per task.** Three failures means the task or the spec is
wrong, not the code. Stop and say so rather than trying a fourth way.
