"""Lance fixtures (pylance 13). Run from the repo root with pylance, pyarrow, numpy installed.

Writes datasets under crates/pstore-format/tests/data/lance/ and the floats each holds.
"""
import os, shutil
import numpy as np, pyarrow as pa, lance

out = 'crates/pstore-format/tests/data/lance/'
shutil.rmtree(out, ignore_errors=True)
os.makedirs(out)
rng = np.random.default_rng(81)
D = 8


def table(v, nullable=False):
    arr = pa.FixedSizeListArray.from_arrays(pa.array(v.ravel()), D)
    if nullable:
        arr = pa.array([None if i % 7 == 0 else list(r) for i, r in enumerate(v)], type=pa.list_(pa.float32(), D))
    return pa.table({'id': pa.array(range(len(v))), 'emb': arr})


v = rng.standard_normal((2600, D)).astype('<f4')
open(out + 'vectors.f32', 'wb').write(v.tobytes())
schema = table(v[:1]).schema
# one dataset, one file, several record batches (so several pages)
batches = [table(v[i:i + 500]).to_batches()[0] for i in range(0, 2600, 500)]
lance.write_dataset(pa.RecordBatchReader.from_batches(schema, batches), out + 'one', data_storage_version='2.0')
# several files
lance.write_dataset(table(v), out + 'many', data_storage_version='2.0', max_rows_per_file=1000)
# nullable column
lance.write_dataset(table(v[:300], nullable=True), out + 'nullable', data_storage_version='2.0')
# deleted rows
ds = lance.write_dataset(table(v[:300]), out + 'deleted', data_storage_version='2.0')
ds.delete('id < 10')
# a newer file format
lance.write_dataset(table(v[:300]), out + 'v21', data_storage_version='2.1')
