import { describe, expect, test } from "bun:test";
import {
  BUILTIN_MEMBERS,
  BUILTIN_RECEIVERS,
  bucketCensus,
  bucketOf,
  UNRESOLVED_BUCKETS,
  type UnresolvedBucket,
} from "#shared/domain/unresolved";
import type { UnresolvedRef } from "#shared/store/read/coverage";

/**
 * The unresolved taxonomy (#5) as a pure function over reader-shaped rows: no database, no
 * extractor, so every rule is provable from one literal.
 *
 * Two things are asserted beyond "each bucket is reachable". First the rule *order*: an
 * ambiguous `evidenceKind` must not move an import out of `internal_module`, and an allowlisted
 * member must beat a `dynamic` evidence kind, because #12 will start writing `ambiguous` for
 * import failures and a bucket that moved with `evidence_kind` would rewrite the census. Second
 * `material`, which is a separate predicate and must never be inferred from a bucket name.
 */

let nextLine = 0;

interface RowInput {
  readonly rawTarget: string;
  readonly evidence: string | null;
  readonly evidenceKind?: string | null;
}

/** One `unresolved` row as `unresolvedForFiles` would hand it over. */
function row({ rawTarget, evidence, evidenceKind = null }: RowInput): UnresolvedRef {
  nextLine += 1;
  return {
    id: nextLine,
    fileId: 1,
    path: "src/server/global/whatsapp/registry.ts",
    line: nextLine,
    rawTarget,
    evidenceKind,
    evidence,
  };
}

/** The three registry-style call rows, all recorded `dynamic` by the resolver. */
function callRow(written: string): UnresolvedRef {
  return row({ rawTarget: written, evidence: `call:${written}`, evidenceKind: "dynamic" });
}

describe("bucketOf", () => {
  test("one row per bucket, and the six labels cover the rows", () => {
    const cases: readonly { readonly row: UnresolvedRef; readonly bucket: UnresolvedBucket }[] = [
      {
        row: row({ rawTarget: "fs", evidence: "import:fs", evidenceKind: "dynamic" }),
        bucket: "builtin_stdlib",
      },
      {
        row: row({ rawTarget: "lodash", evidence: "import:lodash", evidenceKind: "dynamic" }),
        bucket: "external_package",
      },
      {
        row: row({
          rawTarget: "../types.js",
          evidence: "import:../types.js",
          evidenceKind: "dynamic",
        }),
        bucket: "internal_module",
      },
      { row: callRow("push"), bucket: "builtin_stdlib" },
      { row: callRow("obj[0]"), bucket: "dynamic_property_call" },
      {
        row: row({
          rawTarget: "render",
          evidence: "call:render",
          evidenceKind: "receiver_type_unknown",
        }),
        bucket: "receiver_type_unknown",
      },
      {
        row: row({ rawTarget: "messageOf", evidence: "call:messageOf", evidenceKind: "ambiguous" }),
        bucket: "ambiguous",
      },
    ];

    for (const { row: fixture, bucket } of cases) {
      expect(bucketOf(fixture)).toBe(bucket);
    }
    const seen = [...new Set(cases.map((entry) => entry.bucket))].sort();
    expect(seen).toEqual([...UNRESOLVED_BUCKETS].sort());
  });

  test("an import is bucketed by its specifier before anything reads evidenceKind", () => {
    // #12 appends competing candidates after the specifier; the specifier is the text up to the
    // first space, so the suffix must not turn a relative import into an external package.
    expect(
      bucketOf(
        row({
          rawTarget: "./x.js",
          evidence: "import:./x.js candidates: a.ts | b.ts",
          evidenceKind: "ambiguous",
        }),
      ),
    ).toBe("internal_module");
    // The same row is material through `evidenceKind`, and only through that predicate.
    const census = bucketCensus([
      row({
        rawTarget: "./x.js",
        evidence: "import:./x.js candidates: a.ts | b.ts",
        evidenceKind: "ambiguous",
      }),
    ]);
    expect(census.buckets.internal_module).toBe(1);
    expect(census.material).toBe(1);
    // A bare `#` specifier is a package-internal subpath import, not a package.
    expect(bucketOf(row({ rawTarget: "#shared/paths", evidence: "import:#shared/paths" }))).toBe(
      "internal_module",
    );
  });

  test("a node or bun builtin specifier is builtin_stdlib with or without its prefix", () => {
    for (const specifier of ["node:fs", "bun:test", "fs", "path", "fs/promises", "crypto"]) {
      expect(
        bucketOf(
          row({ rawTarget: specifier, evidence: `import:${specifier}`, evidenceKind: "dynamic" }),
        ),
      ).toBe("builtin_stdlib");
    }
    expect(bucketOf(row({ rawTarget: "react", evidence: "import:react" }))).toBe(
      "external_package",
    );
  });

  test("an allowlisted member beats a dynamic evidence kind", () => {
    expect(bucketOf(callRow("map"))).toBe("builtin_stdlib");
    // The receiver is not allowlisted, so only the member can answer "builtin".
    expect(bucketOf(callRow("watchers.push"))).toBe("builtin_stdlib");
  });

  test("every BUILTIN_RECEIVERS entry buckets a dotted call as builtin_stdlib", () => {
    for (const receiver of BUILTIN_RECEIVERS) {
      // A member outside the member list, so no entry can pass on the member's account.
      expect(bucketOf(callRow(`${receiver}.someUnlistedMember`))).toBe("builtin_stdlib");
    }
    expect(BUILTIN_RECEIVERS.size).toBeGreaterThan(0);
  });

  test("every BUILTIN_MEMBERS entry buckets a bare call as builtin_stdlib", () => {
    for (const member of BUILTIN_MEMBERS) {
      // `receiver_type_unknown` evidence, so the member allowlist is the only path to "builtin".
      expect(
        bucketOf(
          row({
            rawTarget: member,
            evidence: `call:${member}`,
            evidenceKind: "receiver_type_unknown",
          }),
        ),
      ).toBe("builtin_stdlib");
    }
    expect(BUILTIN_MEMBERS.size).toBeGreaterThan(0);
  });

  test("an ambiguous evidence kind wins over everything but an import specifier", () => {
    const ambiguous = { rawTarget: "handler", evidenceKind: "ambiguous" } as const;
    expect(bucketOf(row({ ...ambiguous, evidence: "inherits" }))).toBe("ambiguous");
    expect(bucketOf(row({ ...ambiguous, evidence: "call:handler" }))).toBe("ambiguous");
    expect(bucketOf(row({ ...ambiguous, evidence: "call:push" }))).toBe("ambiguous");
    // An import specifier still wins: #12's `ambiguous` moves the row into material, not out of
    // its bucket.
    expect(bucketOf(row({ ...ambiguous, evidence: "import:react" }))).toBe("external_package");
    expect(bucketOf(row({ ...ambiguous, evidence: "import:./x.js" }))).toBe("internal_module");
  });

  test("anything that is not an import, an ambiguous row or a call is receiver_type_unknown", () => {
    expect(bucketOf(row({ rawTarget: "Base", evidence: "inherits" }))).toBe(
      "receiver_type_unknown",
    );
    expect(bucketOf(row({ rawTarget: "render", evidence: "call" }))).toBe("receiver_type_unknown");
    expect(bucketOf(row({ rawTarget: "render", evidence: "import" }))).toBe(
      "receiver_type_unknown",
    );
    expect(bucketOf(row({ rawTarget: "render", evidence: null }))).toBe("receiver_type_unknown");
    // A receiver the allowlists do not know, with no dynamic evidence, could still be user code.
    expect(
      bucketOf(row({ rawTarget: "store.legacyThing", evidence: "call:store.legacyThing" })),
    ).toBe("receiver_type_unknown");
  });
});

describe("bucketCensus", () => {
  test("the five rows reported at whatsapp/registry.ts are non-material", () => {
    // As reported: one relative import and four calls, every one recorded `dynamic` because no
    // declared target existed. The two bare `push` rows plus `watchers.push` are the stdlib tail
    // the allowlist collapses; `Set` stays a dynamic property call, not a hidden caller.
    const rows = [
      row({ rawTarget: "./types.js", evidence: "import:./types.js", evidenceKind: "dynamic" }),
      callRow("Set"),
      callRow("push"),
      callRow("push"),
      callRow("watchers.push"),
    ];

    const census = bucketCensus(rows);
    expect(census.buckets).toEqual({
      builtin_stdlib: 3,
      external_package: 0,
      internal_module: 1,
      dynamic_property_call: 1,
      receiver_type_unknown: 0,
      ambiguous: 0,
    });
    expect(census.material).toBe(0);
  });

  test("the buckets sum to the row count for a mixed fixture", () => {
    const rows = [
      row({ rawTarget: "./types.js", evidence: "import:./types.js", evidenceKind: "ambiguous" }),
      row({ rawTarget: "lodash", evidence: "import:lodash", evidenceKind: "dynamic" }),
      row({ rawTarget: "node:fs", evidence: "import:node:fs", evidenceKind: "dynamic" }),
      callRow("push"),
      callRow("cache.get"),
      callRow("obj[0]"),
      row({ rawTarget: "store.load", evidence: "call:store.load" }),
      row({ rawTarget: "handler", evidence: "call:handler", evidenceKind: "ambiguous" }),
      row({ rawTarget: "Base", evidence: "inherits" }),
    ];

    const census = bucketCensus(rows);
    const sum = UNRESOLVED_BUCKETS.reduce((total, bucket) => total + census.buckets[bucket], 0);
    expect(sum).toBe(rows.length);
    expect(census.buckets).toEqual({
      builtin_stdlib: 3,
      external_package: 1,
      internal_module: 1,
      dynamic_property_call: 1,
      receiver_type_unknown: 2,
      ambiguous: 1,
    });
    // The import row is material through `evidenceKind` while staying in `internal_module`; the
    // ambiguous call and the typed-unknown receiver are material by bucket.
    expect(census.material).toBe(4);
  });

  test("no rows is an all-zero census, not an absent one", () => {
    const census = bucketCensus([]);
    expect(census.buckets).toEqual({
      builtin_stdlib: 0,
      external_package: 0,
      internal_module: 0,
      dynamic_property_call: 0,
      receiver_type_unknown: 0,
      ambiguous: 0,
    });
    expect(census.material).toBe(0);
  });
});
