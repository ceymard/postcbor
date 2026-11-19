"""Cross-check against cbor2: random valid CBOR must be accepted, and its normalized form must decode to the same value.

Run: uv run --with cbor2 tests/crosscheck.py [count]
"""
import math, random, subprocess, sys

import cbor2

COUNT = int(sys.argv[1]) if len(sys.argv) > 1 else 5000
rnd = random.Random(1234)


def value(depth=0):
    kinds = ["int", "bigint", "float", "text", "bytes", "bool", "null", "undefined"]
    if depth < 4:
        kinds += ["array", "map", "tag"]
    k = rnd.choice(kinds)
    if k == "int":
        return rnd.choice([0, 1, -1, 23, 24, 255, 256, 65535, 65536, 2**32, 2**63, 2**64 - 1, -2**64]) + rnd.randint(-2, 2) if rnd.random() < 0.5 else rnd.randint(-10**6, 10**6)
    if k == "bigint":
        return rnd.choice([1, -1]) * rnd.randint(2**64, 2**200)
    if k == "float":
        return rnd.choice([0.0, -0.0, 1.5, 0.1, 1e300, 5.960464477539063e-08, 65504.0, math.inf, -math.inf, rnd.uniform(-1e9, 1e9)])
    if k == "text":
        return "".join(rnd.choice("ab é😀\"\\\n") for _ in range(rnd.randint(0, 8)))
    if k == "bytes":
        return bytes(rnd.randint(0, 255) for _ in range(rnd.randint(0, 8)))
    if k == "bool":
        return rnd.random() < 0.5
    if k == "null":
        return None
    if k == "undefined":
        return cbor2.undefined
    if k == "array":
        return [value(depth + 1) for _ in range(rnd.choice([0, 1, 3, 17, 30]))]
    if k == "map":
        keys = [value(4) for _ in range(rnd.choice([0, 1, 3, 17, 30]))]
        out = {}
        for key in keys:
            try:
                hash(key)
            except TypeError:
                continue
            # Python dicts merge keys like 1, 1.0 and True; CBOR keeps them apart, so skip such keys here.
            if any(key == existing for existing in out):
                continue
            out[key] = value(depth + 1)
        return out
    return cbor2.CBORTag(rnd.choice([7, 99, 1000, 2**40]), value(depth + 1))


def same(a, b):
    """Type-strict equality: 1 and 1.0 differ, NaN equals NaN, -0.0 differs from 0.0.
    cbor2 decodes containers inside tags as tuples and frozendicts; those count as arrays and maps."""
    def family(x):
        if isinstance(x, (list, tuple)):
            return list
        if isinstance(x, dict) or type(x).__name__ == "frozendict":
            return dict
        return type(x)
    if family(a) is not family(b):
        return False
    if isinstance(a, float):
        return (math.isnan(a) and math.isnan(b)) or (a == b and math.copysign(1, a) == math.copysign(1, b))
    if family(a) is list:
        return len(a) == len(b) and all(same(x, y) for x, y in zip(a, b))
    if family(a) is dict:
        return len(a) == len(b) and all(any(same(k, k2) and same(v, v2) for k2, v2 in b.items()) for k, v in a.items())
    if isinstance(a, cbor2.CBORTag):
        return a.tag == b.tag and same(a.value, b.value)
    return a == b


def encodings(v):
    yield cbor2.dumps(v)
    yield cbor2.dumps(v, canonical=True)


inputs, originals = [], []
for _ in range(COUNT):
    v = value()
    for enc in encodings(v):
        inputs.append(enc.hex())
        originals.append(v)

out = subprocess.run(
    ["cargo", "run", "-q", "--release", "-p", "postcbor-core", "--example", "normalize_lines"],
    input="\n".join(inputs) + "\n", capture_output=True, text=True, check=True,
).stdout.splitlines()

failures = 0
for hexin, v, got in zip(inputs, originals, out):
    if got.startswith("ERR"):
        failures += 1
        print("rejected valid input:", hexin, got)
    elif not same(cbor2.loads(bytes.fromhex(got)), v):
        failures += 1
        print("value changed:", hexin, "->", got)
    if failures > 20:
        break
print(f"{len(inputs)} inputs, {failures} failures")
sys.exit(1 if failures else 0)
