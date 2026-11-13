# Example: a game's inventory, with grants that wait for approval

A game server writes players' items all day. After an outage, an agent is
asked to give every affected player five potions. The agent may prepare
the grant, but a person signs it off before any player sees it.

Every command below is run by the test suite as written. `{id}` and
`{time}` stand for a commit id and a time.

## The data

Items keep their history: support will be asked where a potion went.
Positions do not need one, so the `pos` table is declared to keep only
its latest values, which takes about a fifth of the memory per write.

```console
$ umbrachyx table pos latest --who ops:game --why "positions need no past"
declared pos latest
$ umbrachyx commit main player/7/item/potion=3 player/8/item/potion=1 player/9/item/potion=0 pos/7/zone=harbor --who game:server --why "state before the outage"
committed {id} to main
```

## The agent prepares the grant on its own branch

A branch starts where `main` is and copies nothing. What the agent writes
there is not seen by the game.

```console
$ umbrachyx branch agent/liveops --from main
created agent/liveops from main
$ umbrachyx commit agent/liveops player/7/item/potion=8 player/8/item/potion=6 player/9/item/potion=50 --who agent:liveops --why "5 potions each for the outage on the 12th"
committed {id} to agent/liveops
$ umbrachyx get main player/9/item/potion
0
```

## A person reads the change before it counts

```console
$ umbrachyx diff main agent/liveops
~ player/7/item/potion: 3 -> 8
~ player/8/item/potion: 1 -> 6
~ player/9/item/potion: 0 -> 50
```

Player 9 would get fifty. The reviewer sends it back, and the agent
corrects its own branch:

```console
$ umbrachyx commit agent/liveops player/9/item/potion=5 --who agent:liveops --why "player 9 was given 50, not 5"
committed {id} to agent/liveops
```

## Meanwhile the game kept writing

While the grant waited, player 7 drank a potion.

```console
$ umbrachyx commit main player/7/item/potion=2 pos/7/zone=market --who game:server --why "player 7 drank a potion"
committed {id} to main
$ umbrachyx merge main agent/liveops --who lead:liveops --why "outage compensation checked" --approved-by lead:liveops
conflict on player/7/item/potion: base 3, ours 2, theirs 8
nothing was written
```

The agent worked out "3 + 5 = 8" from a count that is no longer true.
Merging would have given the potion back. Both sides changed the key
since the branch began, so the merge stops, says so, and writes nothing.
Conflicts are found, never resolved by guessing.

The agent prepares the grant again from where `main` is now:

```console
$ umbrachyx branch agent/liveops-2 --from main
created agent/liveops-2 from main
$ umbrachyx commit agent/liveops-2 player/7/item/potion=7 player/8/item/potion=6 player/9/item/potion=5 --who agent:liveops --why "5 potions each for the outage on the 12th, counted from now"
committed {id} to agent/liveops-2
$ umbrachyx diff main agent/liveops-2
~ player/7/item/potion: 2 -> 7
~ player/8/item/potion: 1 -> 6
~ player/9/item/potion: 0 -> 5
$ umbrachyx merge main agent/liveops-2 --who lead:liveops --why "outage compensation checked" --approved-by lead:liveops
merged {id}
```

## Afterwards: who gave player 9 what?

```console
$ umbrachyx history main player/9/item/potion
{time}  {id}  5  by lead:liveops  "outage compensation checked"  approved by lead:liveops
{time}  {id}  0  by game:server  "state before the outage"
$ umbrachyx history agent/liveops player/9/item/potion
{time}  {id}  5  by agent:liveops  "player 9 was given 50, not 5"
{time}  {id}  50  by agent:liveops  "5 potions each for the outage on the 12th"
{time}  {id}  0  by game:server  "state before the outage"
```

On `main` the grant is credited to the person who approved it. The fifty
never reached a player, and it is not lost either: the agent's first
branch still says what it tried and how it was corrected.

Positions were written through all of this and hold only where each
player is now:

```console
$ umbrachyx get main pos/7/zone
market
$ umbrachyx tables
pos  latest
```
