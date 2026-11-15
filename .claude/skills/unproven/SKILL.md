---
name: unproven
description: Use when the user wants to understand or verify code an AI wrote (or co-wrote) — "which of these checks actually matter", "what would catch it if this were deleted", "write the delete-this-check table for this chapter / module", or when setting up or updating an `unproven.toml`. Says plainly what the AI wrote, deletes each check in turn with the `unproven` CLI, and records what notices: a fuzzer, a test, something else, or nothing.
---

# unproven

Code an AI wrote compiles and passes its tests, and it still may not do what it says. You come
to understand it by checking its claims, one check at a time: delete it, run everything, and see
what notices. The `unproven` CLI does the deleting and the running and decides each row; your job
is to choose the checks, write the rows, predict, and explain what came back. Never decide a row
yourself: the CLI's `ok` / `MISMATCH` is the answer.

## 0. Say what the AI wrote

Before anything else, write down, with numbers, how much of the code under study an AI wrote:

```bash
git log --format=%B | grep -ci "co-authored-by: claude"   # or the agent the repo used
git log --oneline | wc -l
git log --format="%h %ad %s" --date=short -S"fn the_handler(" | tail -1   # when it was written
git show -s --format=%B <that commit> | grep -i co-authored               # and by which model
```

Put this at the top of what you write (a chapter, a README section, a PR description): the date,
the model, the share of commits. Do not soften it and do not leave it out. Then say what checking
has found since, including what was wrong.

## 1. List the checks

Pick one unit (a handler, an instruction, a contract function). List every check in it: each
condition that returns an error or reverts, each binding of one account or argument to another,
each call to a `verify_*` / `require*` helper. For a Solana program, `solinv mutate plan --src
<file> --only-fn <fn> ...` lists candidates if solinv is installed, but read the function anyway:
it does not list every comparison (a numeric `if a < b { return Err }`) or helper call, and the
one it misses may be the one that matters.

## 2. Write a row for each

In `unproven.toml` (see `examples/unproven.toml` in the unproven repository):

```toml
[[claim]]
row = "Close: `position.user == user`"         # exactly as the page prints it
file = "programs/x/src/lib.rs"
in_fn = "fn process_close_position("          # the deletion stays inside this function
find = "if position.user != *user_ai.key.as_ref() {"
replace = "if false && position.user != *user_ai.key.as_ref() {"
fuzz = ["account-swap", "close_position", "account 1 swapped"]   # or tests_fail, or refused
```

Turn a check off with the smallest edit: `if false && <cond> {`, or delete a `verify_*(...)?;`
line. `in_fn` must occur once in the file and `find` once inside that function (to its matching
closing brace); if not, the CLI stops rather than guess, so add context to make them unique. Each row says what notices, in one of four ways:

- `fuzz = [...]`: a fuzzer finding containing every piece of text.
- `tests_fail = [...]`: exactly these tests fail, every other passes. Name each by its full
  path (`tests::x`) or by its bare name if no other failing test ends the same way.
- `refused = { probe = "<spec> | <variant>", log = "..." }`: nothing above notices, because
  something else refuses the call (the runtime, another check, the token program). The probe
  command prints one line per call, `PROBE <spec> | <variant> | ok=<bool> | <log>...`; the line
  is picked by spec and variant exactly, must say `ok=false`, and `log` names what refused it.
- none of these: **unproven**. Nothing in the repository notices the check is gone.

## 3. Predict, then measure

Write your prediction for a row before you run it. Then:

```bash
unproven unproven.toml --no-baseline --only "<part of the row>"
```

On `ok` the CLI prints what it was decided on (the finding, the failed tests, or the probe line);
copy the wording for the table from there. On `MISMATCH` it prints what it saw. Read it, find out why, and fix the row or your
understanding, not the CLI's verdict. The rows you predicted wrong are what you did not
understand yet; say so in what you write.

## 4. Traps (each of these has made a row read wrong)

- **Precedence.** `if false && a || b {` is `(false && a) || b`: the rest of the check stays. Turn
  off the whole condition, or delete the one comparison (`|| x != y` → `|| false`).
- **The wrong check refused it.** A swap can fail an earlier check (a health check, a size check)
  before it reaches the one you deleted, and the row then reads "refused" for the wrong reason.
  Read the log line in the probe output: which check refused it? Change the setup (a target that
  passes the earlier checks) until the deleted one is what is tested.
- **Nothing to move.** A row that routes a payout elsewhere proves nothing if the payout is 0.
  Give the setup a non-zero amount.
- **A fuzzer's "no" is not "no hole".** A fuzzer that swaps accounts finds missing bindings, not
  numeric rules (margins, limits, fees). A rule nothing writes down shows up as unproven or as
  refused; write a test that states the rule, and the row becomes `tests_fail`.
- **A test that only checks the message.** A row reads "test" when the test fails, even if it
  fails only because the error text changed while the call is still refused by something else.
  Say which in the table.
- **A wide input range hides nothing; a narrow one can.** If the fuzzer's amounts are small, an
  overflow past a deleted bound never happens. An unproven numeric check deserves one test with
  the extreme value before you call it harmless.
- **A finding that is not a hole.** A permissionless call whose signer check is deleted is caught
  (signer-skip) but lets in no one who was not already allowed. Keep the row, and say why it is
  not a hole.

## 5. Make the CLI prove it can fail

Before trusting a table, change one row's expectation to something wrong and run it: it must
print `MISMATCH`. A checker that has only ever said `ok` has not been tested.

## 6. Keep it measured

Add a CI job that runs `unproven unproven.toml` on every pull request that touches the code, the
tests, or the page. Then the table cannot drift from the code without the build saying so.

## 7. Report

List the rows by kind (the CLI's last line counts them), name every unproven check, and for each
one either write the test that would notice it, or say why it is not needed.
