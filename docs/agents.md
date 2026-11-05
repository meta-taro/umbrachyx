# Agents

An AI agent reaches Umbrachyx through MCP, holding a capability token that
a person issued to it. This page shows how to set one up, and what a
hijacked agent can and cannot do with it, checked by tests that run in CI
on every change.

## Setting up an agent

A person does all of this; the agent never sees the private key.

```sh
# Once per database: the root key. The private half goes to the file,
# readable only by you; the public half is printed.
umbrachyx keygen root.key

# Per agent: what it may touch, for what, until when, and the branch its
# writes go to. The token is printed; keep it in a file.
umbrachyx token issue --key root.key \
  --subject agent:support --branch agent/support \
  --prefix player/42/ --columns gems,item --operations read,write \
  --purpose "answer ticket 8812" --expires-in 8h > support.token

# Anyone holding a token can narrow it before handing it on, without the
# private key. It can never be widened.
umbrachyx token narrow --public-key ed25519/… --operation read < support.token > read-only.token

# Who a token was issued to.
umbrachyx token show --public-key ed25519/… < support.token
```

The agent's host starts the session. For Claude Code, in `.mcp.json`:

```json
{
  "mcpServers": {
    "umbrachyx": {
      "command": "umbrachyx",
      "args": ["mcp", "--public-key", "ed25519/…", "--token-file", "/path/to/support.token"],
      "env": { "UMBRACHYX_DB": "/path/to/umbrachyx.log" }
    }
  }
}
```

The agent then has four tools:

| Tool | Does |
|---|---|
| `get` | One key's value, from the agent's branch once it exists, otherwise `main` |
| `history` | Every change to one key: value, commit, who, why, who approved |
| `diff` | What differs between two branches or commits, leaving out keys outside the token |
| `commit` | Sets and removes keys as one commit, with a reason, on the agent's branch |

There is no tool to merge. A person reviews the agent's branch and merges
it, naming who approved it:

```sh
umbrachyx diff main agent/support
umbrachyx merge main agent/support --who ops:lead --why "ticket 8812 refund" --approved-by ops:lead
```

## What a hijacked agent cannot do

A prompt injection can make an agent try anything its tools allow. The
tests in
[`crates/umbrachyx/tests/hijacked_agent.rs`](../crates/umbrachyx/tests/hijacked_agent.rs)
give an agent a valid token for player 42's `gems` and `item`, try each
attack below through the MCP session, then open the log again and compare
every branch, key and value with what was there before.

| Attack | Result | Test |
|---|---|---|
| Read another player's row, its history, or the row on another agent's branch | Refused: outside the token | `reading_another_players_row_is_refused` |
| Read a column the token does not name (`email`) | Refused: outside the token | `reading_a_column_outside_the_token_is_refused` |
| Use a key that only looks like the player's (`player/420/gems`, `player/42`) | Refused: outside the token | `a_key_that_only_looks_like_the_players_is_refused` |
| Diff a branch where other players changed, to learn about them | Their keys are left out | `a_diff_does_not_reveal_other_players_changes` |
| Write to `main`, by naming it as the branch | Refused: `commit` takes no branch | `writing_to_main_is_refused` |
| Write to another agent's branch | Refused: `commit` takes no branch | `writing_to_another_agents_branch_is_refused` |
| Write another player's row or a column outside the token, alone or mixed with an allowed key | Refused: nothing written, not even the branch | `writing_outside_the_token_writes_nothing_at_all` |
| Merge, revert, branch, issue tokens or approve: any tool not offered | Protocol error: no such tool | `merging_or_any_tool_not_offered_is_refused` |
| Send malformed messages | Protocol errors; the session goes on | `malformed_messages_change_nothing_and_the_session_goes_on` |
| Send a message over 1 MiB | Refused without being held whole; the session goes on | `a_line_past_the_size_limit_is_refused_and_the_session_goes_on` in [`tests/mcp.rs`](../crates/umbrachyx/tests/mcp.rs) |
| Append a block to its own token claiming another name, `main` as its branch, every player and column | Allows nothing more; its one allowed write lands on its own branch under its own name | `a_token_widened_by_its_holder_allows_nothing_more` |
| Use a token from another database, an altered token, or made-up bytes | The session does not start | `a_forged_or_altered_token_does_not_start_a_session` |
| Keep going after the token expired | Refused: expired | `an_expired_token_reads_and_writes_nothing` |

In every case, `main` and the other agent's branch are unchanged.

## What this does not cover yet

- **A key inside the prefix is a name, not a path.** `player/42/../7/gems`
  is inside a token for `player/42/`: it names a key nobody wrote, not
  player 7's gems. Prefixes should end with the separator, as `player/42/`
  does; `player/42` would also match `player/420`.
- **What an agent may write within its scope is not checked.** A token for
  player 42's gems lets the agent set them to any value on its branch. The
  review before a merge is what catches that.
- **The person who merges is trusted.** Merging needs no token; whoever
  can run the command-line tool on the log can merge.
- **One agent per database file at a time.** The session opens the log
  itself, and the log takes one writer.
