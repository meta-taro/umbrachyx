# Umbrachyx

**An in-memory database that keeps every change as history.**

Umbrachyx is meant to read and write at memory speed. Behind every value sits
its full history: when it changed, from what, to what, who changed it, why,
and who approved the change. That history is kept per record and per cell,
and you can branch it, diff it and merge it, the way git handles files.

The name comes from *umbra* (shadow) and *tachyon* (the hypothetical particle
faster than light). The current value is the light. Its history is the shadow
that is already there before the light arrives.

> **Status: concept.** Nothing is implemented yet. This repository holds the
> design while it is worked out, starting with [`docs/concept.md`](docs/concept.md).
> Do not build on it.

## What it is for

The first target is **game operations**: items, in-game currency and player
state. These need memory-speed access, and they also raise questions a plain
cache cannot answer:

- "My item disappeared." *When, and through which change?*
- "This account gained currency it should not have." *By what path?*
- "We granted the wrong reward to everyone." *Undo exactly that grant, and
  nothing else.*
- "Try the new event against real data." *On a branch, without touching
  production.*
- "Delete my account." *Make that player's data unreadable for good, while the
  rest of the history stays intact.*

## Principles

1. **Do not trust the caller.** AI agents are fast, numerous and fallible, and
   they can be hijacked. Every caller holds a scoped capability (which rows,
   which columns, until when, for what purpose), and least privilege is built
   into the structure rather than left to configuration.
2. **Agents write to branches.** A person reviews the diff and merges it. Any
   point in time can be restored.
3. **Every value knows where it came from:** who wrote it, from which session
   or instruction, and who approved it.
4. **Deleting means destroying the key.** Data is encrypted per subject. To
   erase a subject, its key is destroyed: the shape of the history stays, and
   the content can never be read again.
5. **Trust comes from evidence, not age.** Formal models, deterministic
   simulation testing and published test results replace "it has run for
   years".

## Relationship to dbboard

[dbboard](https://github.com/meta-taro/dbboard) is a desktop SQL editor and
viewer from the same author. The two are separate products in separate
repositories. dbboard will reach Umbrachyx through an adapter, and its history
views (a row's history, a cell-level diff, who last changed a row) are being
designed so that they work against Umbrachyx as well.

## License

[MIT](LICENSE)
