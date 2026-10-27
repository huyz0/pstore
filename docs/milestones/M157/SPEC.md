# M157: splitting a query over several text fields

⚠️ **Not ready to start.** This is the scope [M154](../M154/SPEC.md) left out. It names the
change and the criteria it must meet. It is written in full, and reviewed, before any code.

## Serves
Workstream E of [the delivery plan](../../comparisons/turbopuffer-next-plan.md) ("multiple
string/string-array BM25 fields"). [M153](../M153/SPEC.md) answers a query whose text legs
read two or more fields correctly by never splitting it. This milestone lets such a query be
split.

## Delta (to be written in full)
- A share's phase 1 answers a `Stats` per field its legs read, and phase 2's sum carries one
  per field. Today each carries one.
- `Part.terms` is per field, each analysed under its field's analyzer.
- A new protocol number for the phased and summed exchanges whose statistics are per field.
  A peer that does not know it answers 409, and the coordinator runs that share itself.
- `one_part` no longer refuses a query over several fields.

## Acceptance criteria (to be made checkable)
1. A query over the primary and a further field, through phased shares, is split and answers
   exactly as unsplit.
2. The same through summed shares.
3. Against a peer that answers the new protocol with 409, the query still answers exactly.
4. A query over one field sends M154's protocol numbers and bytes.
