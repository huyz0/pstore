# M156: token filters over a further text field

⚠️ **Not ready to start.** This is the scope [M155](../M155/SPEC.md) left out. It names the
change and the criteria it must meet. It is written in full, and reviewed, before any code.

## Serves
Workstream E of [the delivery plan](../../comparisons/turbopuffer-next-plan.md) ("multiple
string/string-array BM25 fields"). M152 recorded the risk: a token filter over a further text
field analyzes with the primary's analyzer, so `ContainsAllTokens` on `title` may tokenize it
differently from `title`'s BM25.

## Delta (to be written in full)
- `Predicate::bound` takes the index's per-field analyzers: a token predicate over a further
  field is bound to that field's analyzer, and any other to the primary's.
- Every binding site reads the index's `texts`: queries live and `as_of`, `delete_by_filter`,
  `patch_by_filter`, aggregations and write conditions.
- A share's `Part` carries one `FullText`, so a query whose filter holds a token predicate over
  a further field is not split, as a further-field text leg is not (M152), until
  [M154](../M154/SPEC.md).

## Acceptance criteria (to be made checkable)
1. A token filter over a further field with stemming matches as the same filter over a twin
   index whose primary holds that field, before and after the fold.
2. The same through `delete_by_filter`, `patch_by_filter` and a write condition.
3. A query with such a filter is not split.
4. A token filter over the primary is unchanged.
