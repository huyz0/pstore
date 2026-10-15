"""Lance 2.1 fixtures (M90, pylance 13). Run from the repo root with pylance, pyarrow, numpy installed.

Writes datasets under crates/pstore-format/tests/data/lance21/ and the floats each holds (`<name>.f32`).
Separate from generate_lance.py, which rewrites every 2.0 dataset's file names when it runs.
"""
import os, shutil
import numpy as np, pyarrow as pa, lance

out = 'crates/pstore-format/tests/data/lance21/'
shutil.rmtree(out, ignore_errors=True)
os.makedirs(out)
rng = np.random.default_rng(90)


def table(v, nullable=False):
    d = v.shape[1]
    arr = pa.FixedSizeListArray.from_arrays(pa.array(v.ravel()), d)
    if nullable:
        arr = pa.array([None if i % 7 == 0 else list(r) for i, r in enumerate(v)], type=pa.list_(pa.float32(), d))
    return pa.table({'id': pa.array(range(len(v))), 'emb': arr})


def make(name, rows, dims, nullable=False):
    v = rng.standard_normal((rows, dims)).astype('<f4')
    open(out + name + '.f32', 'wb').write(v.tobytes())
    lance.write_dataset(table(v, nullable), out + name, data_storage_version='2.1')


# MiniBlock (rows under 256 bytes): one short chunk, a padded chunk, two full chunks, many chunks
make('one', 1, 3)
make('padded', 129, 3)
make('exact', 256, 8)
make('many', 2600, 8)
# FullZip (rows of 256 bytes or more)
make('full', 300, 100)
# nulls are written with definition levels: refused
make('nulls', 300, 8, nullable=True)
