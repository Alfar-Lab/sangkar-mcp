import { describe, expect, test } from "bun:test";
import { alpha, parseInput } from "../src/chain";
import { encodeTwice } from "../src/duplicate";

export function testAlpha(): number {
  return alpha(2);
}

export function testParseInput(): number {
  return parseInput("42");
}

describe("chain", () => {
  test("alpha counts one above beta", () => {
    expect(testAlpha()).toBe(5);
  });

  test("parseInput reads a number", () => {
    expect(testParseInput()).toBeGreaterThan(0);
  });

  test("encodeTwice doubles the duplicate", () => {
    expect(encodeTwice("7")).toBeGreaterThan(0);
  });
});
