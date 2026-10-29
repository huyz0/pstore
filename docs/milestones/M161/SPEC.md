# M161: index list pages state what a walk sees while names change

Spec review round 1 passed with four minors, folded in: names also appear and vanish
without a create or drop; equal epochs are a snapshot of HEAD's names only, not of the
unfolded names different processes add; a walk keeps one `prefix`; and external indexes
are not listed.

## Serves
Workstream G of [the delivery plan](../../comparisons/turbopuffer-next-plan.md): "Prefix/
cursor pages have a documented concurrent-update model." [DELIVERY](../../comparisons/DELIVERY.md)'s
G row lists it as the one lifecycle item still pending.

## Delta
Today `GET /v1/indexes?prefix&cursor&page_size` (M9f) answers one page from one HEAD read:
HEAD's index names (with those known only by rejects or quarantine) unioned with the
serving process's unfolded names, in byte order, strictly after `cursor`, and
`next_cursor` the last name on the page. Each page reads HEAD again. Nothing says what a
walk of several pages sees when names are created or dropped between its pages, and a
page does not say which HEAD it was read from.

After:
- **Each page reports `epoch`**: the epoch of the HEAD it was read from (0 when the tenant
  has no HEAD). Every other field is unchanged, and the page still costs one read and no
  LIST.
- **The model, documented** on the response type and in `docs/deploy.md`, and asserted by
  tests. A walk is the pages from an absent `cursor` until `next_cursor` is null, all with the
  same `prefix`, each passing the last page's `next_cursor`. The names listed are HEAD's
  indexes and the names it knows only by rejects or quarantine (not external indexes,
  which are not listed), with the serving process's unfolded names. A page is a keyset page by name, not a snapshot:
  1. **No name twice.** Pages of one walk are disjoint and in increasing byte order,
     because each holds only names strictly after the cursor and the cursor is the
     previous page's last name.
  2. **Stable names exactly once.** A name listed by every page's read, from the first
     page's to the last's, is on exactly one page of the walk.
  3. **Changing names at most once.** A name that appears or vanishes during the walk (by
     a create, a drop, a quarantine discarded, or a fold of a process's unfolded rows) is
     listed at most once: a name that appeared after the walk passed its position is
     missed, and a name that vanished after its page is still on that page. A name dropped
     and created again is still at most once.
  4. **A cursor need not exist.** A `cursor` is a position, not a name that must still
     exist: the walk continues correctly when the cursor's index was dropped.
  5. **A snapshot when epochs agree.** When every page of a walk reports the same `epoch`,
     the walk's HEAD names are exactly that HEAD's matching names. The serving process's
     unfolded names are not part of any HEAD, are read at each page and are not marked
     in the response: a walk whose pages different processes serve may see different
     unfolded names on each, so equal epochs are a snapshot of HEAD's names only. Any
     name is still listed at most once, by rule 1.

Not changed: the request parameters, their validation, the order, the page size bounds,
the cost of a page, HEAD's encoding, and every other route.

## Acceptance criteria
1. A page's `epoch` equals the epoch of HEAD at its read: after a commit it is that
   commit's epoch, and for a tenant with no HEAD it is 0.
2. With 12 indexes and `page_size=5`, a walk in which, between pages, one name past the
   cursor is created, one past the cursor is dropped, one already listed is dropped, one
   before the cursor is created, and one past the cursor is dropped and created again:
   the walk lists every name that existed throughout exactly once, no name twice, the one
   created past the cursor, not the one dropped past it nor the one created before it, and
   the re-created name once. The pages' epochs increase where a commit came between them.
3. A walk whose cursor's index is dropped before the next page continues from that
   position, missing no stable name.
4. A walk with no change between its pages reports the same `epoch` on every page, and its
   names equal one unpaged listing.
5. A page still costs one blob read and no LIST, and the response holds `epoch`.
6. Gates: clippy, the workspace suite, coverage 95/95, mutation of the changed code.

## Test plan
- Server: `list_walk_under_changes` (2, 3), `list_reports_its_epoch` (1, 4, 5), using a
  second `Api` over the same store for the changes, so the listing process holds nothing
  unfolded.

## RA budget
Unchanged: one HEAD read per page, no LIST. Requests scale with pages, never with indexes
beyond the one HEAD object a page already reads.

## Risks
- A walk over a fast-changing tenant may never report equal epochs; the model says what
  holds then, and a client wanting a snapshot pages again. A snapshot listing (paging
  within one retained HEAD by epoch) is not offered, because a page would then read a HEAD
  that GC may reap.

## Tasks
- M161.1: the engine's names with their epoch, the page's `epoch`, the documented model,
  and the tests.
