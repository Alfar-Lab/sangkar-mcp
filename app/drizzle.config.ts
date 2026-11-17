import { defineConfig } from "drizzle-kit";

/**
 * Generates `src/shared/store/migrations/` from `schema.ts`. Only `generate` is used: the SQL
 * is applied by the project's own forward-only runner and ledger (`DATABASE.md` §3, TASK-006),
 * never by `drizzle-kit push` or the bundled migrator.
 */
export default defineConfig({
  dialect: "sqlite",
  schema: "./src/shared/store/schema.ts",
  out: "./src/shared/store/migrations",
  strict: true,
  verbose: true,
});
