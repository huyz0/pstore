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

# M87: Arrow IPC files in the shapes a deletion file could take (and mostly is not), read by the
# hand-written reader's tests.
import pyarrow.ipc as ipc
arrow = out + 'arrow/'
os.makedirs(arrow, exist_ok=True)
rows = [8, 5, 3, 1, 2, 4, 6, 7, 0, 9]
def write(name, table, **kw):
    with ipc.new_file(arrow + name, table.schema, options=ipc.IpcWriteOptions(**kw)) as w:
        for b in table.to_batches(): w.write_batch(b)
u32 = pa.table({'row_id': pa.array(rows, type=pa.uint32())})
write('plain.arrow', u32)
write('zstd.arrow', u32, compression='zstd')
write('lz4.arrow', u32, compression='lz4')
write('empty.arrow', pa.table({'row_id': pa.array([], type=pa.uint32())}))
write('u64.arrow', pa.table({'row_id': pa.array(rows, type=pa.uint64())}))
write('two_columns.arrow', pa.table({'row_id': pa.array(rows, type=pa.uint32()), 'x': pa.array(rows, type=pa.uint32())}))
write('nulls.arrow', pa.table({'row_id': pa.array([1, None, 3], type=pa.uint32())}))
with ipc.new_file(arrow + 'two_batches.arrow', u32.schema) as w:
    w.write_batch(pa.record_batch([pa.array([1, 2], type=pa.uint32())], schema=u32.schema))
    w.write_batch(pa.record_batch([pa.array([3, 4], type=pa.uint32())], schema=u32.schema))
with ipc.new_stream(arrow + 'stream.arrow', u32.schema) as w:
    w.write_table(u32)
# (appended after the above) the shapes that differ from a deletion file in exactly one count
write('sparse.arrow', pa.table({'row_id': pa.array([1, 2, 5], type=pa.uint32())}))
write('string.arrow', pa.table({'row_id': pa.array(['a', 'b', 'c'], type=pa.string())}))
write('null_and_u32.arrow', pa.table({'n': pa.nulls(3), 'row_id': pa.array([1, 2, 3], type=pa.uint32())}))
