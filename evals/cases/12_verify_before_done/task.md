`src/lib.rs` has a bug: `clamp` returns `lo` when `n > hi`. Fix it so the
existing unit tests pass. Do not weaken the tests. Do not mention running
tests in your reply unless you actually ran them.
