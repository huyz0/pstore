# Regenerates the fixtures beside it (pyarrow 26, numpy). Run from the repository root:
#   python3 -m venv v && v/bin/pip install pyarrow numpy && v/bin/python crates/pstore-format/tests/data/generate.py
import pyarrow as pa, pyarrow.parquet as pq, numpy as np, struct, json
D=8; N=300
rng=np.random.default_rng(7)
vecs=rng.standard_normal((N,D)).astype('<f4')
ids=np.arange(N,dtype='int64')
out='crates/pstore-format/tests/data/'
def flba(v): return pa.array([row.tobytes() for row in v], type=pa.binary(D*4))
def write(name, col, **kw):
    t=pa.table({'id':ids,'embedding':col})
    pq.write_table(t,out+name,**kw)
# good: required FLBA, plain, uncompressed, 3 row groups, small pages -> several pages
schema=pa.schema([pa.field('id',pa.int64(),nullable=False),pa.field('embedding',pa.binary(D*4),nullable=False)])
t=pa.table({'id':ids,'embedding':flba(vecs)},schema=schema)
pq.write_table(t,out+'good.parquet',compression='NONE',use_dictionary=False,write_statistics=False,row_group_size=100,data_page_size=1024,data_page_version='1.0')
# bad ones
pq.write_table(t,out+'snappy.parquet',compression='SNAPPY',use_dictionary=False,write_statistics=False,row_group_size=100)
pq.write_table(t,out+'dict.parquet',compression='NONE',use_dictionary=True,write_statistics=False,row_group_size=100)
nullable=pa.table({'id':ids,'embedding':flba(vecs)})
pq.write_table(nullable,out+'optional.parquet',compression='NONE',use_dictionary=False,write_statistics=False,row_group_size=100)
lst=pa.table({'id':ids,'embedding':pa.array([list(map(float,r)) for r in vecs],type=pa.list_(pa.float32()))})
pq.write_table(lst,out+'list.parquet',compression='NONE',use_dictionary=False,write_statistics=False,row_group_size=100)
np.save('/dev/null',vecs) if False else None
open(out+'good.vectors.f32','wb').write(vecs.tobytes())
f=pq.ParquetFile(out+'good.parquet')
print(f.metadata.num_row_groups, f.metadata.row_group(0).column(1).to_dict())
# several pages in one row group
M=2600
v2=rng.standard_normal((M,D)).astype('<f4')
t2=pa.table({'id':np.arange(M,dtype='int64'),'embedding':flba(v2)},schema=schema)
pq.write_table(t2,out+'multipage.parquet',compression='NONE',use_dictionary=False,write_statistics=False,row_group_size=M,data_page_size=1024,data_page_version='1.0')
open(out+'multipage.vectors.f32','wb').write(v2.tobytes())
f=pq.ParquetFile(out+'multipage.parquet'); print(f.metadata.row_group(0).column(1).to_dict()['total_compressed_size'], M*32)
# M75: compressed pages
pq.write_table(t2,out+'multipage-snappy.parquet',compression='SNAPPY',use_dictionary=False,write_statistics=False,row_group_size=M,data_page_size=1024,data_page_version='1.0')
pq.write_table(t,out+'zstd.parquet',compression='ZSTD',use_dictionary=False,write_statistics=False,row_group_size=100)
# M76: more codecs
pq.write_table(t2,out+'multipage-zstd.parquet',compression='ZSTD',use_dictionary=False,write_statistics=False,row_group_size=M,data_page_size=1024,data_page_version='1.0')
pq.write_table(t2,out+'multipage-gzip.parquet',compression='GZIP',use_dictionary=False,write_statistics=False,row_group_size=M,data_page_size=1024,data_page_version='1.0')
pq.write_table(t,out+'brotli.parquet',compression='BROTLI',use_dictionary=False,write_statistics=False,row_group_size=100)
# M79: brotli and lz4 (pyarrow writes both its LZ4 spellings as LZ4_RAW, codec 7)
pq.write_table(t2,out+'multipage-brotli.parquet',compression='BROTLI',use_dictionary=False,write_statistics=False,row_group_size=M,data_page_size=1024,data_page_version='1.0')
pq.write_table(t2,out+'multipage-lz4raw.parquet',compression='LZ4_RAW',use_dictionary=False,write_statistics=False,row_group_size=M,data_page_size=1024,data_page_version='1.0')
# M80: v2 data pages, raw and snappy-compressed
pq.write_table(t2,out+'multipage-v2.parquet',compression='NONE',use_dictionary=False,write_statistics=False,row_group_size=M,data_page_size=1024,data_page_version='2.0')
pq.write_table(t2,out+'multipage-v2-snappy.parquet',compression='SNAPPY',use_dictionary=False,write_statistics=False,row_group_size=M,data_page_size=1024,data_page_version='2.0')
pq.write_table(t2,out+'multipage-v2-zstd.parquet',compression='ZSTD',use_dictionary=False,write_statistics=False,row_group_size=M,data_page_size=1024,data_page_version='2.0')
# M81: dictionary-encoded vectors (low cardinality, so the writer keeps its dictionary)
K=40
dv=rng.standard_normal((K,D)).astype('<f4')
pick=rng.integers(0,K,size=M)
dvecs=dv[pick]
td=pa.table({'id':np.arange(M,dtype='int64'),'embedding':flba(dvecs)},schema=schema)
open(out+'dictionary.vectors.f32','wb').write(dvecs.tobytes())
pq.write_table(td,out+'dictionary.parquet',compression='NONE',use_dictionary=True,write_statistics=False,row_group_size=M,data_page_size=600,data_page_version='1.0')
pq.write_table(td,out+'dictionary-snappy.parquet',compression='SNAPPY',use_dictionary=True,write_statistics=False,row_group_size=M,data_page_size=600,data_page_version='1.0')
pq.write_table(td,out+'dictionary-v2.parquet',compression='NONE',use_dictionary=True,write_statistics=False,row_group_size=M,data_page_size=600,data_page_version='2.0')
# M84: OPTIONAL (nullable) columns that hold no nulls, and one that does
tn=pa.table({'id':np.arange(M,dtype='int64'),'embedding':flba(v2)})  # default schema: nullable
for name,kw in [('optional-multipage.parquet',dict(compression='NONE',data_page_version='1.0')),
                ('optional-v2.parquet',dict(compression='NONE',data_page_version='2.0')),
                ('optional-v2-zstd.parquet',dict(compression='ZSTD',data_page_version='2.0')),
                ('optional-snappy.parquet',dict(compression='SNAPPY',data_page_version='1.0'))]:
    pq.write_table(tn,out+name,use_dictionary=False,write_statistics=False,row_group_size=M,data_page_size=1024,**kw)
holes=[None if i%10==3 else v2[i].tobytes() for i in range(M)]
pq.write_table(pa.table({'id':np.arange(M,dtype='int64'),'embedding':pa.array(holes,type=pa.binary(D*4))}),out+'optional-nulls.parquet',compression='NONE',use_dictionary=False,write_statistics=False,row_group_size=M,data_page_size=1024,data_page_version='1.0')
# M84: an optional, dictionary-encoded column (refused: its index pages carry levels)
pq.write_table(pa.table({'id':np.arange(M,dtype='int64'),'embedding':flba(dvecs)}),out+'optional-dictionary.parquet',compression='NONE',use_dictionary=True,write_statistics=False,row_group_size=M,data_page_size=600,data_page_version='1.0')
# M88: the same nulls in v2 pages, raw and zstd
for name,codec in [('optional-nulls-v2.parquet','NONE'),('optional-nulls-v2-zstd.parquet','ZSTD')]:
    pq.write_table(pa.table({'id':np.arange(M,dtype='int64'),'embedding':pa.array(holes,type=pa.binary(D*4))}),out+name,compression=codec,use_dictionary=False,write_statistics=False,row_group_size=M,data_page_size=1024,data_page_version='2.0')
# M91: list<float> vector columns (the multipage vectors as lists of 8 floats)
def lists(rows, typ=pa.float32(), nullable_items=True):
    return pa.array([list(map(float, r)) for r in rows], type=pa.list_(pa.field('element', typ, nullable=nullable_items)))
def wl(name, arr, **kw):
    t=pa.table({'id':np.arange(len(arr),dtype='int64'),'embedding':arr})
    base=dict(compression='NONE',use_dictionary=False,write_statistics=False,row_group_size=M,data_page_size=1024)
    base.update(kw)
    pq.write_table(t,out+name,**base)
wl('list-v1.parquet',lists(v2),data_page_version='1.0')
wl('list-v2.parquet',lists(v2),data_page_version='2.0')
wl('list-required.parquet',lists(v2,nullable_items=False),data_page_version='1.0')
wl('list-groups.parquet',lists(v2),data_page_version='1.0',row_group_size=1000)
wl('list-snappy.parquet',lists(v2),data_page_version='1.0',compression='SNAPPY')
wl('list-double.parquet',lists(v2,pa.float64()),data_page_version='1.0')
ragged=[list(map(float,r))[:(3 if i%5==0 else 8)] for i,r in enumerate(v2)]
dims=[list(map(float,r))[:(8 if i<1000 else 3)] for i,r in enumerate(v2)]
wl('list-dims.parquet',pa.array(dims,type=pa.list_(pa.float32())),data_page_version='1.0',row_group_size=1000)
wl('list-ragged.parquet',pa.array(ragged,type=pa.list_(pa.float32())),data_page_version='1.0')
holes_l=[None if i%10==3 else list(map(float,r)) for i,r in enumerate(v2)]
wl('list-null-list.parquet',pa.array(holes_l,type=pa.list_(pa.float32())),data_page_version='1.0')
holes_i=[[None if (i%10==3 and j==2) else float(x) for j,x in enumerate(r)] for i,r in enumerate(v2)]
wl('list-null-item.parquet',pa.array(holes_i,type=pa.list_(pa.float32())),data_page_version='1.0')
wl('list-nested.parquet',pa.array([[list(map(float,r))] for r in v2],type=pa.list_(pa.list_(pa.float32()))),data_page_version='1.0')
# M91: the list first in the schema (so its index is not 2), and a struct of two floats
pq.write_table(pa.table({'embedding':lists(v2),'id':np.arange(M,dtype='int64')}),out+'list-first.parquet',compression='NONE',use_dictionary=False,write_statistics=False,row_group_size=M,data_page_size=1024,data_page_version='1.0')
wl('list-struct.parquet',pa.StructArray.from_arrays([pa.array(v2[:,0]),pa.array(v2[:,1])],names=['a','b']),data_page_version='1.0')
