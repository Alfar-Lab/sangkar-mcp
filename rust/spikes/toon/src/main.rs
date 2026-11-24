//! Phase 0 spike (TASK-010): can `serde_toon_format` 0.2.0 (TOON's v4.1 working draft) render the
//! envelopes that `@toon-format/toon` 4.4.0 renders today? It reads one JSON value on stdin and
//! writes TOON on stdout — nothing else — so `scripts/parity/toon-roundtrip.ts` can judge the
//! encoder against the reference decoder alone.
//!
//! Nothing here is meant to ship: the package is standalone (`[workspace]` in `Cargo.toml`) and the
//! only options exercised are the crate defaults, because the decision is whether the *defaults*
//! are good enough for the CLI renderer, not whether some tuning makes one fixture pass.
//!
//! Usage: `sangkar-spike-toon < envelope.json > envelope.toon`. Empty input is not an envelope and
//! is answered with empty output and exit 0, which is what the shell's `--json` pipe hands over
//! when a caller closes stdin.

use std::io::{Read as _, Write as _};

fn main() {
    let mut input = String::new();
    if let Err(error) = std::io::stdin().read_to_string(&mut input) {
        eprintln!("toon-spike: stdin is not valid UTF-8: {error}");
        std::process::exit(2);
    }

    if input.trim().is_empty() {
        return;
    }

    let value: serde_json::Value = match serde_json::from_str(&input) {
        Ok(value) => value,
        Err(error) => {
            eprintln!("toon-spike: stdin is not JSON: {error}");
            std::process::exit(2);
        }
    };

    // Default `EncodeOptions`: two-space indent, comma delimiter. No builder, no canonical profile.
    let toon = match serde_toon::to_string(&value) {
        Ok(toon) => toon,
        Err(error) => {
            eprintln!("toon-spike: encode failed: {error}");
            std::process::exit(3);
        }
    };

    let mut stdout = std::io::stdout().lock();
    if let Err(error) = stdout
        .write_all(toon.as_bytes())
        .and_then(|()| stdout.flush())
    {
        eprintln!("toon-spike: write failed: {error}");
        std::process::exit(4);
    }
}
