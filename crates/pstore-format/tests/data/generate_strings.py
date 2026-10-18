# M100: boolean and string attribute columns (pyarrow 26, numpy). Run from the repository root:
#   python3 -m venv v && v/bin/pip install pyarrow numpy && v/bin/python crates/pstore-format/tests/data/generate_strings.py
import pyarrow as pa, pyarrow.parquet as pq, numpy as np
out = 'crates/pstore-format/tests/data/'
N = 300
r = range(N)
null = lambda i: i % 7 == 3
cols = {
    'name': pa.array([f's{i}' for i in r]),
    'namen': pa.array([None if null(i) else f's{i}' for i in r]),
    'flag': pa.array([i % 2 == 1 for i in r]),
    'flagn': pa.array([None if null(i) else i % 2 == 1 for i in r]),
    # a distinct 300-byte string a row: the 4 KiB dictionary limit makes a chunk fall back to PLAIN
    'long': pa.array([(f'{i:04d}' * 75) for i in r]),
    'bin': pa.array([bytes([i % 256]) for i in r], type=pa.binary()),
}
t = pa.table(cols)
kw = dict(write_statistics=False, row_group_size=100, data_page_size=512)
pq.write_table(t, out + 'strings-plain.parquet', compression='NONE', use_dictionary=False, data_page_version='1.0', **kw)
pq.write_table(t, out + 'strings-dict-snappy.parquet', compression='snappy', use_dictionary=True,
               dictionary_pagesize_limit=4096, data_page_version='1.0', **kw)
pq.write_table(t, out + 'strings-v2-zstd.parquet', compression='zstd', use_dictionary=True, data_page_version='2.0', **kw)
# vectors (good.vectors.f32's 300 x 8) beside a string and a boolean attribute
D = 8
vecs = np.frombuffer(open(out + 'good.vectors.f32', 'rb').read(), dtype='<f4').reshape(N, D)
emb = pa.array([row.tobytes() for row in vecs], type=pa.binary(D * 4))
label = pa.array([None if null(i) else ('odd' if i % 2 else 'even') for i in r])
flag = pa.array([i % 3 == 0 for i in r])
pq.write_table(pa.table({'embedding': emb, 'label': label, 'flag': flag}), out + 'vectors-labels.parquet',
               # the vectors stay uncompressed, to be read in place (M84)
               compression={'embedding': 'NONE', 'label': 'snappy', 'flag': 'snappy'},
               use_dictionary=['label', 'flag'], write_statistics=False, row_group_size=N,
               data_page_size=512, data_page_version='1.0')
