// The module this import names does not exist, on purpose: one import that cannot resolve.
import { ghostHelper } from "./ghost-module";
import { alpha } from "./chain";

export function usesGhost(): number {
  return alpha(ghostHelper());
}
