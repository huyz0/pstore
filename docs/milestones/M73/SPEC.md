# M73 — Filters and deletes over an external index

**Serves:** backlog row 75. M69's `ExternalIndex::search` scores every row. This adds a
caller-supplied row predicate, applied to the codes **before** the window is chosen, so a filter
cannot leave the window full of rows it then drops.

## Delta

`ExternalIndex::search_where(store, query, k, window, allow)` where `allow: &dyn Fn(usize) -> bool`
is true for a row that may be returned (a filter's survivors, minus deleted rows). Rows it rejects
are never scored, never read from the customer's object, and never returned. `search` is
`search_where` allowing every row.

The attribute data is the caller's: this milestone does not read attribute columns from the file
(a row-to-attribute map is a later milestone). A predicate that allows nothing returns no rows and
reads nothing from the object.

## Acceptance criteria

1. `a_filter_prunes_before_the_window` (`pstore-index`, `--test external`): on the 2,600-row real
   Parquet fixture, with a predicate keeping every 5th row, `window` = all rows returns the exact
   filtered cosine top 10, every returned row satisfies the predicate, and a **small** window (20)
   still returns 10 rows (a post-filter of a window of 20 would return about 4).
2. `a_predicate_that_allows_nothing_reads_nothing`: no rows returned and no bytes of the customer's
   object moved.
3. `search_is_search_where_allowing_all`: identical results.
4. **Gates:** `./scripts/gates.sh` passes; the mutation sweep of the changed lines misses 0.

**Not covered:** reading attribute columns from the Parquet file; a persisted delete vector.
