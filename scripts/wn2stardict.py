"""WordNet 3.1 database files -> StarDict (ifo, idx, dict.dz, sametypesequence=m).

Each headword gets one entry whose text lists its senses by part of
speech, one per line: "n. definition; "example"". Synonyms are shown on
the same line in brackets.
"""
import sys, os, struct, zlib, io, collections
src, out = sys.argv[1], sys.argv[2]
POS = {'noun': 'n.', 'verb': 'v.', 'adj': 'adj.', 'adv': 'adv.'}
entries = collections.defaultdict(list)  # word -> [(pos, order, text)]
for part, label in POS.items():
    with open(os.path.join(src, f'data.{part}'), encoding='latin-1') as f:
        for line in f:
            if line.startswith('  '):
                continue
            head, _, gloss = line.partition('|')
            fields = head.split()
            w_cnt = int(fields[3], 16)
            words = [fields[4 + 2 * i].replace('_', ' ') for i in range(w_cnt)]
            # Drop sense-key adjective markers like "(a)".
            words = [w.split('(')[0] for w in words]
            gloss = gloss.strip()
            # WordNet glosses are "definition; "example"; "example"".
            parts = [p.strip() for p in gloss.split(';')]
            defn = parts[0]
            examples = [p for p in parts[1:] if p.startswith('"')]
            text = defn
            if examples:
                text += ' — ' + ' '.join(examples[:2])
            for w in words:
                others = [o for o in words if o != w]
                syn = f' [{", ".join(others[:4])}]' if others else ''
                entries[w].append((label, text + syn))

# StarDict sorts with g_ascii_strcasecmp, then strcmp as a tiebreak.
def key(w):
    return (w.lower().encode(), w.encode())
words = sorted(entries, key=key)

idx = io.BytesIO()
dict_plain = io.BytesIO()
for w in words:
    senses = entries[w]
    lines = []
    for pos in ['n.', 'v.', 'adj.', 'adv.']:
        group = [t for p, t in senses if p == pos]
        for i, t in enumerate(group, 1):
            lines.append(f'{pos} {i}. {t}' if len(group) > 1 else f'{pos} {t}')
    body = '\n'.join(lines).encode('utf-8')
    off = dict_plain.tell()
    dict_plain.write(body)
    idx.write(w.encode('utf-8') + b'\0' + struct.pack('>II', off, len(body)))
plain = dict_plain.getvalue()

# dictzip: gzip with an 'RA' extra field; every chunk an independent raw
# deflate stream so a reader inflates only the chunk it needs.
CHLEN = 58315
chunks = [plain[i:i + CHLEN] for i in range(0, len(plain), CHLEN)]
comp = []
z = zlib.compressobj(9, zlib.DEFLATED, -15)
for i, c in enumerate(chunks):
    # One stream, a full flush at every chunk boundary (resets the
    # window), so a chunk inflates on its own from a byte boundary.
    last = i == len(chunks) - 1
    comp.append(z.compress(c) + z.flush(zlib.Z_FINISH if last else zlib.Z_FULL_FLUSH))
    assert len(comp[-1]) < 65536
ra = struct.pack('<HHH', 1, CHLEN, len(comp)) + b''.join(struct.pack('<H', len(c)) for c in comp)
extra = b'RA' + struct.pack('<H', len(ra)) + ra
assert len(extra) < 65536, len(extra)
gz = io.BytesIO()
gz.write(b'\x1f\x8b\x08' + bytes([0x04]) + struct.pack('<I', 0) + b'\x02\x03')  # FEXTRA, mtime 0, xfl, OS unix
gz.write(struct.pack('<H', len(extra)) + extra)
for c in comp:
    gz.write(c)
gz.write(struct.pack('<II', zlib.crc32(plain) & 0xffffffff, len(plain) & 0xffffffff))

name = 'wordnet'
os.makedirs(out, exist_ok=True)
# WordNet's licence is the numbered header of every data file.
with open(os.path.join(src, 'data.noun'), encoding='latin-1') as f:
    header = [next(f) for _ in range(29)]
with open(os.path.join(out, 'WORDNET-LICENSE'), 'w') as f:
    for line in header:
        f.write(line[len(line) - len(line.lstrip()) :].split(' ', 1)[-1] if line.strip() else '\n')
open(os.path.join(out, name + '.idx'), 'wb').write(idx.getvalue())
open(os.path.join(out, name + '.dict.dz'), 'wb').write(gz.getvalue())
open(os.path.join(out, name + '.ifo'), 'w').write(
    "StarDict's dict ifo file\nversion=2.4.2\n"
    f"wordcount={len(words)}\nidxfilesize={idx.tell()}\n"
    "bookname=WordNet 3.1\nauthor=Princeton University\n"
    "description=WordNet 3.1 (Princeton University). Converted for gideon.\n"
    "sametypesequence=m\n")
print(len(words), 'words', len(plain), 'bytes plain', gz.tell(), 'bytes dz', idx.tell(), 'bytes idx', len(comp), 'chunks')
