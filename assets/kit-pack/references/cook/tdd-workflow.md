# TDD Workflow (reference)

Tests-first flow for the cook stage, pulled on demand — not part of the base cook body.

1. Write a failing test that pins the desired behavior. Run it; confirm it fails for the right reason (the behavior is missing, not a typo in the test).
2. Write the minimum code to make that test pass. Nothing more.
3. Run the test. Green means the behavior exists; red means keep going on the same step.
4. Refactor with the test as a safety net — the test must stay green through the cleanup.
5. Repeat per behavior. One test, one behavior — do not batch unrelated assertions into one test.

## When to use TDD

- The behavior is well-specified and the interface is stable.
- A bug fix — write the reproducing test first so the fix is provably complete.

## When not to

- Exploratory spikes where the interface is still in flux — write tests once the shape settles.
