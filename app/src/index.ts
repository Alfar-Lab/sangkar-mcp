import { log } from "#shared/log";

/**
 * Composition root: the only module allowed to import tools and the indexer. Nothing is
 * registered yet, so every mode reports itself unimplemented and exits 2 — stdout stays
 * clean for the protocol, the diagnostic goes to stderr.
 */
export async function main(mode: string, argv: string[]): Promise<number> {
  log.warn("not implemented", { mode, argv });
  return 2;
}
