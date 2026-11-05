# Demo: five questions about a game's data

This walks through five questions that are hard to answer with most
databases, using a small, made-up game: players with gems and items. Every
command below is run by the test suite exactly as written (`tests/demo.rs`
in the `umbrachyx` crate), so the output shown is what the current code
prints.

Commit ids and times differ on every run. They are shown as `{id}` and
`{time}`.

## Setup

Build the command-line tool and point it at a new log file:

```sh
cargo build --release
export PATH="$PWD/target/release:$PATH"
export UMBRACHYX_DB="$PWD/demo.log"
```

Every write is a commit: one or more keys set (`key=value`) or removed
(`--remove key`), with who made it, why, and, when someone signed it off,
who approved it. A commit is acknowledged only once it is synced to the log.

```console
$ umbrachyx commit main player/42/gems=120 player/42/item/sword=legendary player/7/gems=30 --who agent:onboarding --why "starter grant"
committed {id} to main
$ umbrachyx commit main player/42/gems=95 --who game:shop --why "bought a potion"
committed {id} to main
$ umbrachyx commit main player/42/gems=5095 --who agent:support --why "compensation for ticket 8812"
committed {id} to main
$ umbrachyx commit main player/7/item/sword=legendary --remove player/42/item/sword --who game:trade --why "trade between players 42 and 7"
committed {id} to main
```

## 1. "An item disappeared." When, and in which change?

Player 42 says their sword is gone.

```console
$ umbrachyx get main player/42/item/sword
(absent)
$ umbrachyx history main player/42/item/sword
{time}  {id}  removed  by game:trade  "trade between players 42 and 7"
{time}  {id}  legendary  by agent:onboarding  "starter grant"
```

A removal is kept as a change of its own, so the history says which commit
took the sword away, who made it and why. The trade moved it to player 7 in
the same commit.

## 2. "This account has more gems than it should." Where did they come from?

```console
$ umbrachyx history main player/42/gems
{time}  {id}  5095  by agent:support  "compensation for ticket 8812"
{time}  {id}  95  by game:shop  "bought a potion"
{time}  {id}  120  by agent:onboarding  "starter grant"
```

Every value comes with the change that set it. Here a support agent added
5,000 gems with no one approving it, which is the line to look into. Reading
the history of a key costs as many steps as the key has changes, not as
many as the database has commits.

## 3. "We gave everyone the wrong reward." Take back only that.

An event grants a reward, it is approved, and players keep playing.

```console
$ umbrachyx commit main player/7/gems=1030 player/42/gems=6095 --who agent:events --why "halloween reward" --approved-by ops:lead
committed {id} to main
$ umbrachyx commit main player/42/item/cape=halloween --who game:shop --why "bought a cape"
committed {id} to main
$ umbrachyx log main --limit 2
{id}  {time}  game:shop  "bought a cape"
{id}  {time}  agent:events  "halloween reward"  approved by ops:lead
```

The reward was 100 times too large. Undo that one commit, one step back
from the latest on `main` (its id works too):

```console
$ umbrachyx revert main main~1 --who ops:lead --why "reward was 100x too large"
reverted with {id}
$ umbrachyx get main player/42/gems
5095
$ umbrachyx get main player/7/gems
30
$ umbrachyx get main player/42/item/cape
halloween
```

Only the keys the reward changed went back; the cape bought afterwards
stays. Had a player already spent the reward, the undo would have stopped
and listed that key as a conflict instead of overwriting it.

## 4. "Try a new event on real data without touching production."

A branch starts where `main` is and copies nothing. Writes to it are not
seen on `main`.

```console
$ umbrachyx branch event/winter --from main
created event/winter from main
$ umbrachyx commit event/winter player/7/gems=530 player/42/gems=5595 --who agent:event-designer --why "try a winter reward"
committed {id} to event/winter
$ umbrachyx get main player/7/gems
30
$ umbrachyx diff main event/winter
~ player/42/gems: 5095 -> 5595
~ player/7/gems: 30 -> 530
```

When the event is ready, merge it. Had `main` changed the same keys in the
meantime, the merge would have stopped and listed them.

```console
$ umbrachyx merge main event/winter --who ops:lead --why "ship the winter reward"
merged {id}
$ umbrachyx get main player/7/gems
530
```

## 5. "Delete my account." Make that player's data unreadable for good.

Every value is encrypted under a key for its subject: `player/42/gems`
belongs to `player/42`. The keys live in a file next to the log
(`demo.log.keys` here), and erasing a subject destroys its key. Its
values, current and past, on every branch, can never be read again, while
the history keeps its shape: which keys changed, when, by whom and why.

```console
$ umbrachyx erase player/42 --who ops:privacy --why "account deletion request 1207"
erased player/42
$ umbrachyx get main player/42/gems
(erased)
$ umbrachyx history main player/42/item/sword
{time}  {id}  removed  by game:trade  "trade between players 42 and 7"
{time}  {id}  erased  by agent:onboarding  "starter grant"
```

Other players are untouched, and a diff shows that player 42's gems
changed without showing what they were. The erasure itself is recorded.

```console
$ umbrachyx get main player/7/gems
530
$ umbrachyx diff main~2 main
~ player/42/gems: erased
~ player/7/gems: 1030 -> 530
$ umbrachyx erasures
{time}  player/42  by ops:privacy  "account deletion request 1207"
```

Destroying the key removes it from the key file the database reads. It
does not reach copies kept below the file system, such as blocks a
solid-state disk has not reused yet, or backups of the key file.
