# M106 — Verified

One line per acceptance criterion in [SPEC.md](SPEC.md). Gate: `scripts/check-verified.py`.

⚠️ **Where this ran:** a Linux x86-64 cloud container, against `MemoryStore`. Byte and key counts
are exact. Nothing here is a wall-clock number.

1. **Another tenant's index costs nothing.** `an_index_of_a_tenant_the_node_does_not_serve_costs_it_nothing`
   (`cargo test -p pstore-server --test idle`): with 100 other tenants of 2 indexes each in the
   store, the serving node holds 1 engine. It touched no key but A's HEAD, A's WAL and the
   served index, by any method, found or not. Its cache holds the same bytes as in the bare
   arm, above zero.
2. **An idle sibling costs no read and no cache.**
   `an_idle_index_of_a_served_tenant_costs_its_head_entry_and_nothing_else`: with 200 idle
   sibling indexes the same predicate holds.
3. **It costs the HEAD entry.** `an_idle_index_of_a_served_tenant_costs_its_head_entry_and_nothing_else`
   prints `HEAD: 310 bytes bare, 39360 with 200 idle siblings, 195 bytes per idle index`, and
   asserts the figure is above zero. That is the encoded size, for an index of one segment. The
   decoded copy the engine holds is not measured: there is no counting allocator without
   `unsafe`.
4. **The controls fail every clause.** In
   `an_index_of_a_tenant_the_node_does_not_serve_costs_it_nothing`, one extra query on tenant
   1000 fails all three clauses: the engine count (`2 engines`), the key set (`a foreign key
   03e8/tnt/1000/...`) and the cache bytes. In
   `an_idle_index_of_a_served_tenant_costs_its_head_entry_and_nothing_else`, one on sibling
   `sib7` fails two: the key set (`an idle sibling's key .../idx/sib7/...`) and the cache
   bytes. Each clause is asserted separately. This is the predicate's red.
5. [M6i](../M6i/VERIFIED.md) criterion 7 links here.
   `grep -c "Measured by \[M106\]" docs/milestones/M6i/VERIFIED.md` → 1.
6. **Gates.** `./scripts/gates.sh`, by the pre-commit hook on this commit.
