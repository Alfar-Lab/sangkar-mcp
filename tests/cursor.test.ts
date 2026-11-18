import { describe, expect, test } from "bun:test";
import {
  CURSOR_KEY_MIN_BYTES,
  CURSOR_TAG_BYTES,
  type Cursor,
  type CursorPayload,
  type CursorResult,
  createCursors,
  sign,
  verify,
} from "#shared/cursor";
import { AppError } from "#shared/errors";

const KEY = "cursor-test-key-0123456789abcdef-0123456789";
const OTHER_KEY = "cursor-test-key-fedcba9876543210-fedcba987654";

/** What a tool would pass back: the generation it is resuming in, its own name and arguments. */
const EXPECTED = {
  generation: "g-7f3a91",
  tool: "trace",
  argsHash: "sha256:0000000000000000000000000000000000000000000000000000000000000000",
} as const;

function payload(overrides: Partial<CursorPayload> = {}): CursorPayload {
  return { ...EXPECTED, position: 50, ...overrides };
}

function cursors(key: string = KEY): Cursor {
  return createCursors({ key });
}

/** Narrows a result to its refusal half, failing loudly if the cursor was accepted. */
function refused(result: CursorResult): { code: string; hint: string } {
  if (result.ok) {
    throw new Error(`expected a refusal, got position ${result.position}`);
  }
  return { code: result.code, hint: result.hint };
}

function accepted(result: CursorResult): number {
  if (!result.ok) {
    throw new Error(`expected acceptance, got ${result.code}`);
  }
  return result.position;
}

/** Replaces one character, so the token keeps its shape and only its content changes. */
function swapChar(text: string, index: number): string {
  const current = text[index] as string;
  return `${text.slice(0, index)}${current === "A" ? "B" : "A"}${text.slice(index + 1)}`;
}

describe("round trip", () => {
  test("a signed cursor verifies back to the same position", () => {
    const token = sign(payload({ position: 50 }), KEY);
    expect(typeof token).toBe("string");
    expect(token).toContain(".");
    expect(accepted(verify(token, EXPECTED, KEY))).toBe(50);
  });

  test("every position a page boundary can address survives the round trip", () => {
    // 0 is the first continuation; 99998 is the `query_graph` max_rows ceiling (TOOL-SURFACE.md).
    for (const position of [0, 1, 57, 200, 99998]) {
      const token = sign(payload({ position }), KEY);
      expect(accepted(verify(token, EXPECTED, KEY))).toBe(position);
    }
  });

  test("the bound pair from createCursors signs and verifies the same tokens", () => {
    const bound = cursors();
    const token = bound.sign(payload({ position: 7 }));
    expect(token).toBe(sign(payload({ position: 7 }), KEY));
    expect(accepted(bound.verify(token, EXPECTED))).toBe(7);
  });

  test("signing is deterministic: no clock, no salt, no server memory", () => {
    expect(sign(payload(), KEY)).toBe(sign(payload(), KEY));
  });

  test("the payload carries generation, tool, argsHash and position", () => {
    const token = sign(payload({ position: 3 }), KEY);
    const body = token.slice(0, token.indexOf("."));
    expect(JSON.parse(Buffer.from(body, "base64url").toString("utf8"))).toEqual({
      generation: EXPECTED.generation,
      tool: EXPECTED.tool,
      argsHash: EXPECTED.argsHash,
      position: 3,
    });
  });
});

describe("cursor_invalid", () => {
  test("a tampered tag is refused as cursor_invalid", () => {
    const token = sign(payload(), KEY);
    const result = verify(swapChar(token, token.length - 1), EXPECTED, KEY);
    expect(refused(result)).toEqual({
      code: "cursor_invalid",
      hint: expect.stringContaining("re-run the original call"),
    });
  });

  test("a tampered tag of a single flipped base64url digit in the middle is still invalid", () => {
    const token = sign(payload(), KEY);
    const tagStart = token.indexOf(".") + 1;
    const result = verify(swapChar(token, tagStart + 4), EXPECTED, KEY);
    expect(refused(result).code).toBe("cursor_invalid");
  });

  test("a tampered payload invalidates the tag, never silently moves the page", () => {
    const token = sign(payload({ position: 10 }), KEY);
    // Rewriting the position in the payload cannot produce a matching tag without the key.
    const forged = Buffer.from(JSON.stringify(payload({ position: 999999 })), "utf8").toString(
      "base64url",
    );
    const result = verify(`${forged}.${token.slice(token.indexOf(".") + 1)}`, EXPECTED, KEY);
    expect(refused(result).code).toBe("cursor_invalid");
  });

  test("a token signed with a different key is invalid here", () => {
    const foreign = sign(payload(), OTHER_KEY);
    expect(refused(verify(foreign, EXPECTED, KEY)).code).toBe("cursor_invalid");
  });

  test("malformed tokens are refused without throwing", () => {
    for (const token of ["", ".", "abc", "abc.", ".abc", "a.b.c", "!!!.???", "garbage!!"]) {
      expect(refused(verify(token, EXPECTED, KEY)).code).toBe("cursor_invalid");
    }
  });

  test("an oversized token is refused before it is decoded", () => {
    const big = `${"A".repeat(4096)}.${"B".repeat(22)}`;
    expect(refused(verify(big, EXPECTED, KEY)).code).toBe("cursor_invalid");
  });

  test("a tag of the wrong width is invalid, not a timingSafeEqual crash", () => {
    const token = sign(payload(), KEY);
    const [body] = token.split(".");
    const shortTag = Buffer.alloc(CURSOR_TAG_BYTES - 1, 7).toString("base64url");
    expect(refused(verify(`${body}.${shortTag}`, EXPECTED, KEY)).code).toBe("cursor_invalid");
  });
});

describe("cursor_expired", () => {
  test("a token minted in one generation is expired under another", () => {
    const token = sign(payload({ generation: "g-7f3a91" }), KEY);
    const result = verify(token, { ...EXPECTED, generation: "g-91c4de" }, KEY);
    expect(refused(result).code).toBe("cursor_expired");
  });

  test("expiry carries the re-run hint and no position, so no wrong page can be built", () => {
    const token = sign(payload({ position: 50 }), KEY);
    const result = verify(token, { ...EXPECTED, generation: "g-91c4de" }, KEY);
    const { code, hint } = refused(result);
    expect(code).toBe("cursor_expired");
    expect(hint).toBe("re-run the original call");
    expect(result).not.toHaveProperty("position");
  });

  test("a stale generation outranks a changed call: it reads as expired, not mismatch", () => {
    const token = sign(payload(), KEY);
    const result = verify(
      token,
      { generation: "g-91c4de", tool: "search", argsHash: "sha256:other" },
      KEY,
    );
    expect(refused(result).code).toBe("cursor_expired");
  });

  test("a cursor signed by the bound pair expires after the generation advances", () => {
    const bound = cursors();
    const token = bound.sign(payload());
    expect(accepted(bound.verify(token, EXPECTED))).toBe(50);
    const advanced = bound.verify(token, { ...EXPECTED, generation: "g-91c4de" });
    expect(refused(advanced).code).toBe("cursor_expired");
  });
});

describe("cursor_mismatch", () => {
  test("a token for another tool is refused as cursor_mismatch", () => {
    const token = sign(payload({ tool: "trace" }), KEY);
    const result = verify(token, { ...EXPECTED, tool: "search" }, KEY);
    expect(refused(result)).toEqual({
      code: "cursor_mismatch",
      hint: expect.stringContaining("re-run the original call"),
    });
  });

  test("a token for other arguments is refused as cursor_mismatch", () => {
    const token = sign(payload(), KEY);
    const result = verify(token, { ...EXPECTED, argsHash: "sha256:ff" }, KEY);
    expect(refused(result).code).toBe("cursor_mismatch");
  });

  test("the same tool with one changed argument is still a mismatch", () => {
    const token = sign(payload({ argsHash: "sha256:aa" }), KEY);
    expect(refused(verify(token, { ...EXPECTED, argsHash: "sha256:ab" }, KEY)).code).toBe(
      "cursor_mismatch",
    );
  });
});

describe("key handling", () => {
  test("the bound pair refuses a key shorter than the minimum", () => {
    expect(() => createCursors({ key: "short" })).toThrow(AppError);
    try {
      createCursors({ key: "" });
    } catch (e) {
      expect(e).toBeInstanceOf(AppError);
      expect((e as AppError).code).toBe("internal");
      expect((e as AppError).hint).toContain("SANGKAR_CURSOR_KEY");
    }
  });

  test("the minimum is enforced on bytes, at exactly the boundary", () => {
    const exact = "k".repeat(CURSOR_KEY_MIN_BYTES);
    expect(() => createCursors({ key: exact })).not.toThrow();
    expect(() => createCursors({ key: "k".repeat(CURSOR_KEY_MIN_BYTES - 1) })).toThrow(AppError);
    // 16 two-byte characters are 32 bytes of key material, so they are accepted.
    expect(() => createCursors({ key: "é".repeat(CURSOR_KEY_MIN_BYTES / 2) })).not.toThrow();
  });

  test("the unbound functions check the key too", () => {
    expect(() => sign(payload(), "short")).toThrow(AppError);
    expect(() => verify(sign(payload(), KEY), EXPECTED, "short")).toThrow(AppError);
  });
});

describe("payload validation", () => {
  test("signing a malformed payload is a server bug, reported as internal", () => {
    const bad: CursorPayload[] = [
      payload({ generation: "" }),
      payload({ tool: "" }),
      payload({ argsHash: "" }),
      payload({ position: -1 }),
      payload({ position: 1.5 }),
    ];
    for (const candidate of bad) {
      expect(() => sign(candidate, KEY)).toThrow(AppError);
    }
  });
});
