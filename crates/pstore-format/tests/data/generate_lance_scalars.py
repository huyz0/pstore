"""Lance scalar attribute fixtures (M101, pylance 13). Run from the repo root with pylance,
pyarrow and numpy installed.

Writes datasets under crates/pstore-format/tests/data/lance-scalars/. Every value is a function
of the row number r, so a test recomputes it rather than reading a second file:

  emb    float32 x 8, the rows of lance/vectors.f32 (the first ROWS of them)
  i32    r * 3 - 1500           int64  (r - 1500) << 40
  f32    r / 4                  f64    r / 8 - 100
  u64    2**63 + r              date   r - 1500 days
  ts_ms  r * 1000 - 7 ms        ts_us  r - 5 us, UTC
  ts_ns  r * 1001 - 2000 ns

and, refused by name: b (bool), s (string), i32n (null every 7th row), i16, ts_s, and in 2.1
small (r, an int32 the 2.1 writer bit-packs).
"""
import os, shutil
import numpy as np, pyarrow as pa, lance

out = 'crates/pstore-format/tests/data/lance-scalars/'
shutil.rmtree(out, ignore_errors=True)
os.makedirs(out)
D = 8
ROWS = 2600
v = np.frombuffer(open('crates/pstore-format/tests/data/lance/vectors.f32', 'rb').read(), '<f4')
v = v.reshape(-1, D)[:ROWS]


def table(lo, hi):
    r = np.arange(lo, hi, dtype='int64')
    return pa.table({
        'emb': pa.FixedSizeListArray.from_arrays(pa.array(v[lo:hi].ravel()), D),
        'i32': pa.array((r * 3 - 1500).astype('int32')),
        'int64': pa.array((r - 1500) << 40),
        'f32': pa.array((r / 4).astype('float32')),
        'f64': pa.array(r / 8 - 100),
        'u64': pa.array(r.astype('uint64') + np.uint64(1 << 63)),
        'date': pa.array((r - 1500).astype('int32'), type=pa.date32()),
        'ts_ms': pa.array(r * 1000 - 7, type=pa.timestamp('ms')),
        'ts_us': pa.array(r - 5, type=pa.timestamp('us', tz='UTC')),
        'ts_ns': pa.array(r * 1001 - 2000, type=pa.timestamp('ns')),
        'b': pa.array(r % 2 == 1),
        's': pa.array([f's{x}' for x in r]),
        'i32n': pa.array([None if x % 7 == 0 else int(x) for x in r], type=pa.int32()),
        'i16': pa.array(r.astype('int16')),
        'ts_s': pa.array(r, type=pa.timestamp('s')),
        'small': pa.array(r.astype('int32')),
    })


whole = table(0, ROWS)
batches = [table(i, min(i + 500, ROWS)).to_batches()[0] for i in range(0, ROWS, 500)]
# one file, several pages
lance.write_dataset(pa.RecordBatchReader.from_batches(whole.schema, batches), out + 'one',
                    data_storage_version='2.0')
# several fragments, and rows deleted from one
ds = lance.write_dataset(whole, out + 'many', data_storage_version='2.0', max_rows_per_file=1000)
ds.delete('i32 >= 1500 AND i32 < 1530')
# Lance 2.1: mini-block pages of several chunks
lance.write_dataset(whole, out + 'v21', data_storage_version='2.1')
# Lance 2.1 in several fragments, with rows deleted inside bit-packed chunks (M102)
ds = lance.write_dataset(whole, out + 'v21-deleted', data_storage_version='2.1',
                         max_rows_per_file=1000)
ds.delete('(small >= 1000 AND small < 1010) OR small IN (5, 1023, 1024, 1500, 2599)')
