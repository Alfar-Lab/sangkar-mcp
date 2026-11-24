import { alpha, parseInput as firstEncoding } from "./chain";

/** The duplicate of `src/chain.ts::parseInput`, on purpose: `trace parseInput` is ambiguous. */
export function parseInput(text: string): number {
  return firstEncoding(text) + alpha(1);
}

export function encodeTwice(text: string): number {
  return parseInput(text) * 2;
}
