import { defineConfig } from "drizzle-kit";

/**
 * Generates `rust/crates/sangkar-store/migrations/` from `schema.ts`. Only `generate` is used:
 * the SQL is applied by the project's own forward-only runner and ledger (`DATABASE.md` §3,
 * TASK-006), never by `drizzle-kit push` or the bundled migrator.
 *
 * The `out` path follows the SQL into the Rust crate (TASK-061); this whole file dies with
 * `drizzle-kit` in TASK-063.
 */
export default defineConfig({
  dialect: "sqlite",
  schema: "./src/shared/store/schema.ts",
  out: "./rust/crates/sangkar-store/migrations",
  strict: true,
  verbose: true,
});
