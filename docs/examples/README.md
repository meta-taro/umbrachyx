# Examples

Three small systems built on Umbrachyx, each a walkthrough with made-up
data. Every command is run by the test suite exactly as written
(`tests/demo.rs` in the `umbrachyx` crate), so the output shown is what
the current code prints. Commit ids and times differ on every run and are
shown as `{id}` and `{time}`.

| Example | What it shows |
|---|---|
| [A game's inventory](game-inventory.md) | An agent's grant is reviewed, corrected and approved while the game keeps writing; a table that keeps no past |
| [A support desk](support-desk.md) | One branch per ticket, so each proposal is approved or left alone on its own; a customer erased afterwards |
| [Settings rolled out in stages](staged-settings.md) | Each stage approved by name, and one stage taken back without touching what came after |

To follow one by hand, build the tool and give the example a log of its
own:

```sh
cargo build --release
export PATH="$PWD/target/release:$PATH"
export UMBRACHYX_DB="$PWD/example.log"
```

In all three the agent is played by hand with `--who`. A real agent holds
a token for its own branch and talks to `umbrachyx mcp`, which gives it
no way to write anywhere else or to merge
([`docs/agents.md`](../agents.md)). A person can do the reviewing from a
database viewer instead of the command line: `umbrachyx serve` lists the
changes waiting, shows each one's diff, and takes the approval or the
rejection (`UMBRA.PENDING`, `UMBRA.DIFF`, `UMBRA.APPROVE`,
`UMBRA.REJECT`).

[`docs/demo.md`](../demo.md) is a fourth walkthrough, around five
questions about a game's data.
