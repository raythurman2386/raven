# anydoc 0.2.4, zip 9.5.0

This is the published [anydoc 0.2.4](https://github.com/firecrawl/anydoc) crate
(MIT, Copyright (c) 2026 Sideguide Technologies Inc.) with one dependency change.

Crates.io `anydoc` 0.2.4 requires `zip` 8.6.0. These advisories are fixed only
in `zip` >= 9:

- RUSTSEC-2026-0367
- RUSTSEC-2026-0368
- RUSTSEC-2026-0369
- RUSTSEC-2026-0429

`zip` 9.5.0 compiles this crate with no Rust changes. The root workspace
replaces the crates.io crate via `[patch.crates-io]`. Delete this directory
when a published `anydoc` depends on `zip` >= 9.
