#!/usr/bin/env python3
"""Compare a predecessor delegate dump with the successor's, by arithmetic.

Every predecessor key (bar migration markers) must exist in the successor,
and every 32-byte byte string inside the predecessor's values (room owner
keys, member signing keys: `self_sk`, secrets) must appear somewhere in the
successor's values. Byte-identical values are reported as such; a value the
UI re-merged on import is judged by the 32-byte strings it carries.
"""
import io, json, sys
import cbor2

MARKERS = ("__migrate_pred_done__:", "__migrate_pred_wip__:", "__legacy_migration")


def values(dump):
    return {k: bytes.fromhex(v["hex"]) for k, v in dump.get("entries", {}).items() if "hex" in v}


def b32(obj, out):
    if isinstance(obj, (bytes, bytearray)):
        if len(obj) == 32:
            out.add(bytes(obj))
        else:
            try:  # nested CBOR inside a byte string (per-room blobs)
                b32(cbor2.loads(bytes(obj)), out)
            except Exception:
                pass
    elif isinstance(obj, list):
        if len(obj) == 32 and all(isinstance(x, int) and 0 <= x < 256 for x in obj):
            out.add(bytes(obj))
        else:
            for x in obj:
                b32(x, out)
    elif isinstance(obj, dict):
        for k, v in obj.items():
            b32(k, out)
            b32(v, out)
    elif isinstance(obj, cbor2.CBORTag):
        b32(obj.value, out)


def secrets_in(vals):
    out = set()
    for k, v in vals.items():
        if k.startswith(MARKERS):
            continue
        try:
            b32(cbor2.loads(v), out)
        except Exception:
            if len(v) == 32:
                out.add(v)
    return out


old = json.load(open(sys.argv[1]))
new = json.load(open(sys.argv[2]))
ov, nv = values(old), values(new)
if not ov:
    print("      predecessor dump holds no values: nothing was seeded, so nothing is proven")
    sys.exit(1)
bad = 0
for k in sorted(ov):
    if k.startswith(MARKERS) or k.startswith("signing_key_of:"):
        continue
    if k not in nv:
        print(f"      key {k!r}: MISSING in successor")
        bad += 1
    elif ov[k] == nv[k]:
        print(f"      key {k!r}: byte-identical ({len(ov[k])} bytes)")
    else:
        print(f"      key {k!r}: present, re-merged ({len(ov[k])} -> {len(nv[k])} bytes)")
for k, v in sorted(old.get("entries", {}).items()):
    if not k.startswith("signing_key_of:"):
        continue
    got = new.get("entries", {}).get(k, {}).get("signing_public_key")
    want = v.get("signing_public_key")
    ok = want is not None and got == want
    print(f"      {k}: predecessor signs as {want}, successor as {got} -> {'OK' if ok else 'MISMATCH'}")
    if not ok:
        bad += 1
extra = sorted(k for k in nv if k not in ov)
print(f"      successor-only keys: {extra}")
so, sn = secrets_in(ov), secrets_in(nv)
lost = so - sn
print(f"      32-byte values: predecessor {len(so)}, successor {len(sn)}, missing from successor {len(lost)}")
if lost:
    bad += 1
sys.exit(1 if bad else 0)
