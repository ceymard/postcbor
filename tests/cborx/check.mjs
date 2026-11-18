// Checks postbor against real cbor-x, both directions (bundleStrings must be refused):
//   1. what cbor-x writes (records, pack, bundleStrings) normalizes in postbor to the plain encoding of the same objects;
//   2. what cbor_pack(..., 'cbor-x') writes decodes in cbor-x to the original objects.
// Run with a cbor-x install on NODE_PATH (no package is added to this repository), e.g.
//   NODE_PATH=/path/to/node_modules node tests/cborx/check.mjs
import { execFileSync } from "node:child_process";
import { createRequire } from "node:module";
import { isDeepStrictEqual } from "node:util";

const require = createRequire(process.env.NODE_PATH + "/");
const { Encoder, encode } = require("cbor-x");
console.log("cbor-x", require("cbor-x/package.json").version);

// Values as rel sends them: rows of plain objects sharing keys, nested rows, many shapes.
const row = (i) => ({ id: i, name: `user ${i}`, active: i % 2 === 0, tags: ["a", "b"], score: i / 4 });
const values = [
  [row(1), row(2), row(3)],
  { rows: Array.from({ length: 50 }, (_, i) => row(i)), total: 50 },
  [{ a: 1 }, { a: { a: { a: 2 } } }, { b: [{ a: 3 }] }],
  Array.from({ length: 300 }, (_, i) => ({ ["k" + i]: i, x: i })),
  [{}, { "": null }, { a: "repeated text repeated", b: "repeated text repeated" }],
  // More shapes than cbor-x has record ids: ids are reused.
  Array.from({ length: 8300 }, (_, i) => ({ ["s" + i]: i })),
  { nested: [row(9), { inner: [row(10), row(11)] }] },
];

// cbor-x keeps NaN-free numbers, strings, booleans, null, arrays and plain objects as plain CBOR without records.
const plain = (v) => Buffer.from(encode(v, { useRecords: false })).toString("hex");
const encoders = {
  "records (new Encoder())": new Encoder(),
  "records, pack": new Encoder({ pack: true }),
};
// bundleStrings writes the strings after the value (not one CBOR item): postbor must refuse it, never store it wrong.
const rejected = { "records, bundleStrings": new Encoder({ bundleStrings: true }) };

const run = (example, lines) =>
  execFileSync("cargo", ["run", "-q", "--release", "-p", "postbor-core", "--example", example], { input: lines.join("\n") + "\n" })
    .toString().trim().split("\n");

let failures = 0;
for (const [name, enc] of Object.entries(encoders)) {
  const packed = values.map((v) => Buffer.from(enc.encode(v)).toString("hex"));
  const ours = run("normalize_lines", packed);
  const want = run("normalize_lines", values.map(plain));
  values.forEach((_, i) => {
    if (ours[i] !== want[i]) {
      failures++;
      console.log(`FAIL ${name} #${i}: got ${ours[i].slice(0, 80)}, want ${want[i].slice(0, 80)}`);
    }
  });
  console.log(`${name}: ${values.length - values.filter((_, i) => ours[i] !== want[i]).length}/${values.length} read back equal`);
}

for (const [name, enc] of Object.entries(rejected)) {
  const out = run("normalize_lines", values.map((v) => Buffer.from(enc.encode(v)).toString("hex")));
  const accepted = out.filter((l) => !l.startsWith("ERR")).length;
  // A value without strings to bundle encodes without the bundle and is fine.
  console.log(`${name}: ${out.length - accepted}/${out.length} refused`);
  out.forEach((l, i) => {
    if (!l.startsWith("ERR") && l !== run("normalize_lines", [plain(values[i])])[0]) {
      failures++;
      console.log(`FAIL ${name} #${i}: accepted with a different value`);
    }
  });
}

const ourPacked = run("pack_lines", values.map(plain));
const decoder = new Encoder();
values.forEach((v, i) => {
  const got = decoder.decode(Buffer.from(ourPacked[i], "hex"));
  if (!isDeepStrictEqual(got, v)) {
    failures++;
    console.log(`FAIL cbor_pack #${i}: cbor-x decodes ${JSON.stringify(got).slice(0, 120)}`);
  }
});
console.log(`cbor_pack('cbor-x') read by cbor-x: ${values.length} values, ${failures} failures in total`);
process.exit(failures ? 1 : 0);
