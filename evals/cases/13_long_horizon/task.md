Fix all the failing unit tests across the three modules in `src/lib.rs`.

The crate has four failing tests spanning independent functions:
- `src/stats.rs`: `mean` and `median` are wrong.
- `src/strings.rs`: `to_snake_case` is wrong.
- `src/finance.rs`: `monthly_payment` (non-zero rate) is wrong.

Start by running `cargo test` to see what fails, then fix each module in turn
and re-run the tests until everything passes. This is a long, multi-step task:

1. Set a goal with `goal_set` describing the overall objective.
2. Track the four fixes with `todo_write` (one item per failing function).
3. Fix each bug, re-running `cargo test` after each fix to confirm progress.
4. Update the todo list as each item is completed.
5. Only finish when the full test suite passes.
