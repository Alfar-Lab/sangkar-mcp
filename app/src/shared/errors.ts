/**
 * The closed error-code union. Adding a code is a visible diff (TECH-SPEC.md §11); a new
 * failure mode is expressed by one of these, never by a free-form string.
 */
export const APP_ERROR_CODES = [
  "wrong_project_db",
  "unsupported_filesystem",
  "schema_too_new",
  "cursor_invalid",
  "cursor_expired",
  "cursor_mismatch",
  "budget_too_large",
  "bootstrapping",
  "not_found",
  "ambiguous",
  "index_locked",
  "internal",
] as const;

export type AppErrorCode = (typeof APP_ERROR_CODES)[number];

export interface AppErrorOptions {
  hint?: string;
  retryable?: boolean;
  retryAfterMs?: number;
  cause?: unknown;
}

/** The single error type that crosses a boundary (TECH-SPEC.md §11). */
export class AppError extends Error {
  readonly code: AppErrorCode;
  readonly hint: string | undefined;
  readonly retryable: boolean;
  readonly retryAfterMs: number | undefined;

  constructor(code: AppErrorCode, message: string, options: AppErrorOptions = {}) {
    super(message);
    this.name = "AppError";
    this.code = code;
    this.hint = options.hint;
    this.retryable = options.retryable ?? false;
    this.retryAfterMs = options.retryAfterMs;
    if (options.cause !== undefined) {
      this.cause = options.cause;
    }
  }
}

/** The envelope's `error` object; optional members are omitted, never present as undefined. */
export interface ErrorPayload {
  code: AppErrorCode;
  message: string;
  hint?: string;
  retryable?: boolean;
  retry_after_ms?: number;
}

export function isAppError(e: unknown): e is AppError {
  return e instanceof AppError;
}

/**
 * Converts anything thrown into the envelope's error payload. Unknown failures become
 * `internal`, so a handler bug cannot leak a non-contract code to the agent.
 */
export function toErrorPayload(e: unknown): ErrorPayload {
  if (e instanceof AppError) {
    const payload: ErrorPayload = { code: e.code, message: e.message };
    if (e.hint !== undefined) {
      payload.hint = e.hint;
    }
    if (e.retryable) {
      payload.retryable = true;
    }
    if (e.retryAfterMs !== undefined) {
      payload.retry_after_ms = e.retryAfterMs;
    }
    return payload;
  }
  return { code: "internal", message: e instanceof Error ? e.message : String(e) };
}
