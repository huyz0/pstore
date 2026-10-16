# M95: scalar attribute columns (pyarrow 26, numpy). Run from the repository root:
#   python3 -m venv v && v/bin/pip install pyarrow numpy && v/bin/python crates/pstore-format/tests/data/generate_scalars.py
import pyarrow as pa, pyarrow.parquet as pq, numpy as np, decimal
out = 'crates/pstore-format/tests/data/'
N = 300
r = np.arange(N)
nulls = (r % 7 == 3)
def masked(a, ty):
    return pa.array([None if nulls[i] else a[i].item() for i in range(N)], type=ty)
cols = {
    'i32': pa.array((r * 1000 - 150000).astype('int32')),
    'i64': pa.array((r * 4_000_000_000 - 5_000_000_000).astype('int64')),
    'f32': pa.array((r * 0.25 - 10).astype('float32')),
    'f64': pa.array(r / 3.0),
    'i64n': masked((r * 10).astype('int64'), pa.int64()),
    'i32n': masked((r - 100).astype('int32'), pa.int32()),
    'f64n': masked(r / 4.0, pa.float64()),
    'i8': pa.array((r % 100 - 50).astype('int8')),
    'u32': pa.array((3_000_000_000 + r).astype('uint32')),
    'u64': pa.array(np.uint64(2**63) + r.astype('uint64')),
    'date': pa.array(r.astype('int32'), type=pa.date32()),
    'ts_ns': pa.array(r.astype('int64'), type=pa.timestamp('ns')),
    'ts_ms': pa.array(r.astype('int64'), type=pa.timestamp('ms')),
    'dec': pa.array([decimal.Decimal(int(i)) / 100 for i in r], type=pa.decimal128(9, 2)),
    'flag': pa.array([bool(i % 2) for i in r]),
    'name': pa.array([f'n{i}' for i in r]),
    'allnull': pa.array([None] * N, type=pa.int64()),
}
t = pa.table(cols)
kw = dict(use_dictionary=False, write_statistics=False, row_group_size=N, data_page_size=512, data_page_version='1.0', store_decimal_as_integer=True)
pq.write_table(t, out + 'scalars.parquet', compression='NONE', **kw)
pq.write_table(t, out + 'scalars-zstd.parquet', compression='zstd', **kw)
# INT96 (the legacy timestamp), one column
pq.write_table(pa.table({'ts96': pa.array(r.astype('int64'), type=pa.timestamp('ns'))}), out + 'int96.parquet',
               compression='NONE', use_dictionary=False, write_statistics=False, use_deprecated_int96_timestamps=True)
# v2 pages: the levels sit outside the compressed values, so only a null makes a page unreadable
kw2 = dict(kw, data_page_version='2.0')
pq.write_table(t, out + 'scalars-v2-zstd.parquet', compression='zstd', **kw2)
# vectors (good.vectors.f32's 300 x 8) beside an attribute with nulls, in one file
D = 8
vecs = np.frombuffer(open(out + 'good.vectors.f32', 'rb').read(), dtype='<f4').reshape(N, D)
emb = pa.array([row.tobytes() for row in vecs], type=pa.binary(D * 4))
grp = pa.array([None if nulls[i] else int(i % 5) for i in range(N)], type=pa.int32())
pq.write_table(pa.table({'embedding': emb, 'grp': grp, 'score': pa.array(r / 10.0)}), out + 'vectors-attrs.parquet',
               compression='NONE', use_dictionary=False, write_statistics=False, row_group_size=N, data_page_size=512, data_page_version='1.0')
