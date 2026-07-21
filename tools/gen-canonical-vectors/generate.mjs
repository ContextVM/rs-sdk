// Golden-vector generator for CEP-8 canonical invocation identity.
//
// Emits ../../tests/fixtures/canonical_vectors.json, the cross-SDK byte-parity
// anchor that pins rs-sdk's payments/canonical.rs to the ts-sdk's canonicalization.
// The expected hash for each vector comes from the ts-sdk's REAL
// computeCanonicalInvocationHash (imported from the sibling sdk checkout), so the
// fixture can never drift from what the ts-sdk actually emits. The expected
// canonical JCS string is recomputed here with the same pinned canonicalize@2.1.0
// and cross-checked to hash to the real function's output, so the two never disagree.
//
// Runtime: bun. Plain node cannot import the ts-sdk .ts source. See README.md.
//
// Every non-ASCII or control character in the inputs is built from a numeric code
// point (String.fromCodePoint / fromCharCode) so this source stays pure ASCII and the
// exact code points are unambiguous.

import { writeFileSync, mkdirSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';

import { computeCanonicalInvocationHash } from '../../../sdk/src/payments/canonical-identity.ts';
import canonicalizePkg from 'canonicalize';
import { sha256 } from '@noble/hashes/sha2.js';
import { bytesToHex } from '@noble/hashes/utils.js';

// canonicalize ships a CommonJS default export; normalize across module interop.
const canonicalize =
  typeof canonicalizePkg === 'function' ? canonicalizePkg : canonicalizePkg.default;

const HERE = dirname(fileURLToPath(import.meta.url));

// Marks a vector whose params are JSON-RPC `undefined` (no params key at all), as
// distinct from an explicit `null`. An absent paramsText in the fixture means the same.
const UNDEFINED_PARAMS = Symbol('undefined-params');

// Known-good anchor: the CEP-8 spec example. The run aborts if the real ts-sdk
// function ever stops producing this, which would signal an upstream change.
const SPEC_EXAMPLE_HASH =
  '0595375815c8e42e3b4194f4543fc3462fd727991da55541ad7f7457579d7391';

// Special code points, built numerically so the source is pure ASCII.
const PUA = String.fromCodePoint(0xe000); // Private Use Area (BMP)
const GLOBE = String.fromCodePoint(0x1f30d); // U+1F30D earth globe (non-BMP)
const THUMBS_UP = String.fromCodePoint(0x1f44d); // U+1F44D
const PARTY = String.fromCodePoint(0x1f389); // U+1F389
const E_ACUTE = String.fromCodePoint(0xe9); // U+00E9 precomposed e-acute
const COMBINING_ACUTE = String.fromCodePoint(0x301); // U+0301 combining acute
const CJK = String.fromCodePoint(0x65e5, 0x672c, 0x8a9e); // U+65E5 U+672C U+8A9E
// Quote, backslash, solidus, then LF TAB CR BS FF, then NUL US DEL.
const CONTROL_SOUP = String.fromCharCode(
  0x22, 0x5c, 0x2f, 0x0a, 0x09, 0x0d, 0x08, 0x0c, 0x00, 0x1f, 0x7f,
);

// Reproduces the ts-sdk's top-level `_meta` strip + canonicalize so we can store the
// canonical JCS string beside the hash for byte-level failure diagnostics. Mirrors
// ../../../sdk/src/payments/canonical-identity.ts; the cross-check below guarantees it
// stays faithful to the real function.
function canonicalStringOf(method, params) {
  let semanticParams = params;
  if (params && typeof params === 'object' && !Array.isArray(params)) {
    const { _meta: _omit, ...rest } = params;
    semanticParams = rest;
  }
  const canonical = canonicalize({ method, params: semanticParams });
  if (canonical === undefined) {
    throw new Error(`canonicalize() returned undefined for method '${method}'`);
  }
  return canonical;
}

// Vector inputs. `paramsText` is the RAW JSON text each SDK parses with its native
// parser, so number spellings and null-vs-undefined are unambiguous. Number-spelling
// vectors are hand-written (JSON.stringify would normalize them); unicode/escaping
// vectors are built with JSON.stringify so the escapes are exact. The non-interoperable
// unsafe-integer case (|n| > 2^53-1) is deliberately absent: it is a fail-closed rs-side
// error covered by a Rust-only test, and has no shared cross-SDK canonical form.
const VECTORS = [
  { name: 'spec-example', method: 'tools/call', paramsText: '{"name":"get_weather","arguments":{"location":"New York"}}' },
  { name: 'key-order', method: 'tools/call', paramsText: '{"a":1,"b":2,"name":"test"}' },
  { name: 'key-order-reversed', method: 'tools/call', paramsText: '{"name":"test","b":2,"a":1}' },
  { name: 'nested-and-array', method: 'tools/call', paramsText: '{"nested":{"z":1,"y":2,"x":3},"arr":[1,2,3]}' },
  { name: 'unicode-value', method: 'tools/call', paramsText: JSON.stringify({ text: `Hello ${GLOBE}` }) },
  { name: 'nonbmp-key-sort', method: 'tools/call', paramsText: JSON.stringify({ [PUA]: 3, [GLOBE]: 2, z: 1 }) },
  { name: 'empty-object-params', method: 'tools/call', paramsText: '{}' },
  { name: 'empty-array-params', method: 'tools/call', paramsText: '[]' },
  { name: 'array-params', method: 'tools/call', paramsText: '[1,2,3]' },
  { name: 'primitive-string', method: 'tools/call', paramsText: '"literal"' },
  { name: 'primitive-number', method: 'tools/call', paramsText: '42' },
  { name: 'primitive-bool', method: 'tools/call', paramsText: 'true' },
  { name: 'null-params', method: 'tools/call', paramsText: 'null' },
  { name: 'undefined-params', method: 'tools/call', paramsText: UNDEFINED_PARAMS },
  { name: 'meta-strip-present', method: 'tools/call', paramsText: '{"name":"add","arguments":{"a":1,"b":2},"_meta":{"progressToken":1}}' },
  { name: 'meta-strip-absent', method: 'tools/call', paramsText: '{"name":"add","arguments":{"a":1,"b":2}}' },
  { name: 'meta-strip-rich', method: 'tools/call', paramsText: '{"name":"add","arguments":{"a":1,"b":2},"_meta":{"progressToken":999,"stream":"writer"}}' },
  { name: 'nested-meta-kept', method: 'tools/call', paramsText: '{"arguments":{"_meta":{"x":1},"v":2}}' },
  { name: 'number-1e21', method: 'tools/call', paramsText: '{"n":1e21}' },
  { name: 'number-1e-7', method: 'tools/call', paramsText: '{"n":1e-7}' },
  { name: 'number-neg-zero-int', method: 'tools/call', paramsText: '{"n":-0}' },
  { name: 'number-neg-zero-float', method: 'tools/call', paramsText: '{"n":-0.0}' },
  { name: 'number-one-float', method: 'tools/call', paramsText: '{"n":1.0}' },
  { name: 'number-safe-int-max', method: 'tools/call', paramsText: '{"n":9007199254740991}' },
  { name: 'number-safe-int-min', method: 'tools/call', paramsText: '{"n":-9007199254740991}' },
  { name: 'number-mixed', method: 'tools/call', paramsText: '{"i":42,"f":3.141592653589793,"big":1e100,"small":1e-100}' },
  { name: 'number-max-double', method: 'tools/call', paramsText: '{"n":1.7976931348623157e308}' },
  { name: 'number-min-subnormal', method: 'tools/call', paramsText: '{"n":5e-324}' },
  { name: 'bool-null-values', method: 'tools/call', paramsText: '{"t":true,"f":false,"z":null}' },
  { name: 'deep-nesting', method: 'tools/call', paramsText: '{"a":{"b":{"c":{"d":{"e":1}}}}}' },
  { name: 'escaping', method: 'tools/call', paramsText: JSON.stringify({ s: CONTROL_SOUP }) },
  { name: 'combining-vs-precomposed', method: 'tools/call', paramsText: JSON.stringify({ precomposed: E_ACUTE, decomposed: `e${COMBINING_ACUTE}` }) },
  { name: 'mixed-scripts', method: 'tools/call', paramsText: JSON.stringify({ cjk: CJK, emoji: `${THUMBS_UP}${PARTY}`, accent: `caf${E_ACUTE}` }) },
  { name: 'empty-string-method', method: '', paramsText: '{"a":1}' },
];

function buildVector(v) {
  const hasParams = v.paramsText !== UNDEFINED_PARAMS;
  const params = hasParams ? JSON.parse(v.paramsText) : undefined;

  const expectedHash = computeCanonicalInvocationHash(v.method, params);
  const expectedCanonical = canonicalStringOf(v.method, params);

  // The canonical string we store must hash to what the real ts-sdk function
  // returned, so expectedCanonical and expectedHash can never silently disagree.
  const recomputed = bytesToHex(sha256(new TextEncoder().encode(expectedCanonical)));
  if (recomputed !== expectedHash) {
    throw new Error(
      `hash mismatch for '${v.name}': canonical string hashes to ${recomputed}, ts-sdk returned ${expectedHash}`,
    );
  }

  return {
    name: v.name,
    method: v.method,
    ...(hasParams ? { paramsText: v.paramsText } : {}),
    expectedCanonical,
    expectedHash,
  };
}

const vectors = VECTORS.map(buildVector);

// Relational self-checks over the generated set (fail loud rather than emit a bad fixture).
const byName = Object.fromEntries(vectors.map((v) => [v.name, v]));
function assert(cond, msg) {
  if (!cond) throw new Error(`self-check failed: ${msg}`);
}
assert(byName['spec-example'].expectedHash === SPEC_EXAMPLE_HASH, 'spec-example hash drifted from the known anchor');
assert(
  byName['meta-strip-present'].expectedHash === byName['meta-strip-absent'].expectedHash,
  'a top-level _meta must not affect the hash',
);
assert(
  byName['meta-strip-rich'].expectedHash === byName['meta-strip-absent'].expectedHash,
  'a multi-key top-level _meta must not affect the hash',
);
assert(
  byName['nested-meta-kept'].expectedHash !== byName['meta-strip-absent'].expectedHash,
  'a nested _meta must affect the hash',
);
assert(
  byName['null-params'].expectedHash !== byName['undefined-params'].expectedHash,
  'null params must differ from undefined params',
);
assert(
  byName['key-order'].expectedHash === byName['key-order-reversed'].expectedHash,
  'the hash must be key-order independent',
);

const output = {
  _comment:
    "Generated by tools/gen-canonical-vectors from the ts-sdk's real computeCanonicalInvocationHash (see tools/gen-canonical-vectors/README.md). Cross-SDK byte-parity anchor for CEP-8 explicit-gating invocation identity: any change here is wire-visible. Do not edit by hand; regenerate with `bun run generate`.",
  vectors,
};

const outDir = join(HERE, '..', '..', 'tests', 'fixtures');
const outPath = join(outDir, 'canonical_vectors.json');
mkdirSync(outDir, { recursive: true });
writeFileSync(outPath, `${JSON.stringify(output, null, 2)}\n`);

console.log(`Wrote ${vectors.length} vectors to ${outPath}`);
console.log(`spec-example hash: ${byName['spec-example'].expectedHash}`);
