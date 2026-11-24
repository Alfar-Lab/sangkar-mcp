import { alpha } from "./chain";

export function legacyEntry(n) {
  return alpha(n) + 1;
}

export class LegacyBuffer {
  constructor(size) {
    this.size = size;
  }

  resize(size) {
    return this.size + size;
  }
}
