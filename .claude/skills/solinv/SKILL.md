---
name: solinv
description: Use after editing a Solana program (Anchor, Pinocchio or native Rust: a programs/<name>/src, or a src with an entrypoint) and before saying the work is done. Runs solinv diff to find the account checks the edit removed or widened (signer, owner, type, seeds, has_one, constraint, a handler's require!, a native `if !a.is_signer()`), and puts each back unless the user asked for that change.
---

# Keep the account checks an edit drops

An edit that makes an error go away by deleting a `has_one`, loosening `Account<'info, T>` to
`UncheckedAccount`, or adding `|| something` to a constraint compiles and passes the tests, and
opens the program to any caller. solinv compares the program before and after your edit and
names each check that is gone.

## When

After every edit to a file under a Solana program's source, before you report the task done.
Run it even when you think nothing changed: it takes a second.

## Steps

1. **Run the diff.** Call the `diff` tool of the `solinv` MCP server with no arguments: it
   compares the working tree, uncommitted edits included, with `HEAD`. If the edit is already
   committed, pass `base` (the commit before your work). Without the MCP server, run for each
   program directory `<dir>`:

   ```bash
   base=$(mktemp -d) && git archive HEAD -- <dir> | tar -x -C "$base" \
     && solinv diff --base-src "$base/<dir>" --head-src <dir> --format json; rm -rf "$base"
   ```

2. **For each finding with `status` `removed` or `widened`**, read `message`, `before.text` and
   `after.text`:
   - The user asked for this check to go or to loosen (in so many words, or the task cannot be
     done any other way and they agreed): keep it, and say so in your reply.
   - Otherwise put the check back as `before.text` has it, keeping the rest of your edit. If the
     check was what made something fail, the check is not the bug: find the cause, or report
     the failure to the user with the check still in place.
   - Do not make the finding go away another way: no `/// CHECK:` comment, no renamed slot or
     instruction, no moving the check where solinv cannot see it. A check you moved into a
     helper on purpose is reported as removed; say where it went and that it runs on every path.

3. **`warning` findings** (a slot or instruction renamed, a type changed, a native program's
   check gone): read them and mention any you cannot explain. Do not change code for them
   unless one is a check you dropped by mistake.

4. **Run the diff again** until `summary.removed` and `summary.widened` count only the checks
   you kept on purpose.

5. **In your reply**, one line: `solinv: <n> removed, <m> widened — restored <which>, kept
   <which> (why)`. If `isError` is true (no program found, not a git repository), say that
   solinv could not compare, and why.
