import { parseInput as parseDuplicate } from "./duplicate";

export function alpha(n: number): number {
  return beta(n) + 1;
}

function beta(n: number): number {
  return gamma(n) * 2;
}

function gamma(n: number): number {
  return n - 1;
}

export function parseInput(text: string): number {
  return alpha(text.length) + parseDuplicate(text);
}
