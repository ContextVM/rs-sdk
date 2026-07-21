# gen-canonical-vectors

Generates `tests/fixtures/canonical_vectors.json`, the **cross-SDK byte-parity anchor**
for CEP-8 explicit-gating invocation identity. The Rust golden test in
`src/payments/canonical.rs` asserts that `compute_canonical_invocation_hash` reproduces
every vector's canonical string and hash byte-for-byte, so this fixture is what keeps
rs-sdk and ts-sdk computing the *same* invocation hash for the *same* request.

## Why a generated fixture

The ts-sdk pins no canonicalization vectors, so parity is anchored by a fixture produced
from a pinned JS run and committed here. The generator **imports the ts-sdk's real
`computeCanonicalInvocationHash`** (from `../../../sdk/src/payments/canonical-identity.ts`)
rather than re-implementing it, so the goldens reflect exactly what the ts-sdk emits and
cannot drift from a hand-copied algorithm. The canonical JCS string stored beside each
hash is recomputed with the same pinned `canonicalize` and cross-checked to hash to the
real function's output.

## Runtime: bun (not node)

The generator imports a TypeScript source file, which plain `node` cannot resolve, so it
runs under **bun** (the ts-sdk's own runtime). bun also resolves the imported file's own
`canonicalize` / `@noble/hashes` imports against the sibling `../sdk` checkout, so the
goldens come from the ts-sdk's own resolved libraries.

## Pinned versions (and why)

`package.json` pins the ts-sdk's exact versions:

- **`canonicalize@2.1.0`** is the ts-sdk's RFC 8785 JCS engine. rs-sdk uses `json-canon`
  (via `ryu-js`), proven byte-identical to it across the interoperable surface.
- **`@noble/hashes@2.2.0`** is the ts-sdk's SHA-256.

Do not loosen these. The fixture's bytes are only meaningful relative to a fixed pair of
libraries.

The generator also imports these two libraries directly (from this tool's own `node_modules`) to
recompute each `expectedCanonical` string and cross-check it against the real function's hash; the
same pins back the node fallback below. Both import paths (this tool's own and the imported ts
file's) resolve to identical pinned versions.

## Regenerate

```sh
# once, so the imported ts source can resolve its own canonicalize / @noble/hashes:
cd ../sdk && bun install

# then, from this directory:
cd - && bun install        # canonicalize@2.1.0 + @noble/hashes@2.2.0 for this tool
bun run generate           # writes ../../tests/fixtures/canonical_vectors.json
```

The generator aborts if the spec-example hash drifts from its known anchor, if the
recomputed canonical string does not hash to the real function's output, or if any
`_meta` / null-vs-undefined / key-order relational invariant fails.

### Fallback (no bun, or no `../sdk` checkout)

Vendor a verbatim copy of `computeCanonicalInvocationHash` as plain `.mjs` (header
comment citing the exact ts-sdk source and commit, re-diffed on every regeneration), run
it under `node` against this tool's own pinned `canonicalize@2.1.0` + `@noble/hashes@2.2.0`.
The `package.json` pins above serve this path.

## A fixture change is a wire event

`canonical_vectors.json` determines which paid authorization a retry matches. **Any change
to it is wire-visible** and must be reviewed as a protocol change, not a test tweak. It is
generated, byte-stable output; regenerate it with the command above, never by hand.

## Provenance

Generated from the ts-sdk's `canonical-identity.ts`, which has been byte-stable since
0.13.8 (`2aaa38a`); the committed fixture here was produced against the sibling checkout
at 0.13.9, where that file is unchanged.
