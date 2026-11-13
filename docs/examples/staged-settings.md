# Example: settings rolled out in stages

A site turns a new checkout on for a share of its visitors, a stage at a
time. An agent proposes each stage with the evidence for it; a person
approves each by name. When a stage goes wrong, only that stage is taken
back.

Every command below is run by the test suite as written. `{id}` and
`{time}` stand for a commit id and a time.

## The settings

```console
$ umbrachyx commit main flag/checkout-v2/percent=0 site/banner=none --who ops:release --why "initial settings"
committed {id} to main
```

## Stage 1

```console
$ umbrachyx branch rollout/checkout-v2 --from main
created rollout/checkout-v2 from main
$ umbrachyx commit rollout/checkout-v2 flag/checkout-v2/percent=10 --who agent:rollout --why "stage 1: 10% of visitors"
committed {id} to rollout/checkout-v2
$ umbrachyx diff main rollout/checkout-v2
~ flag/checkout-v2/percent: 0 -> 10
$ umbrachyx merge main rollout/checkout-v2 --who lead:web --why "stage 1 approved" --approved-by lead:web
merged {id}
```

## Stage 2, on the same branch

```console
$ umbrachyx commit rollout/checkout-v2 flag/checkout-v2/percent=50 --who agent:rollout --why "stage 2: 50%, errors stayed under 0.1% at 10%"
committed {id} to rollout/checkout-v2
$ umbrachyx diff main rollout/checkout-v2
~ flag/checkout-v2/percent: 10 -> 50
$ umbrachyx merge main rollout/checkout-v2 --who lead:web --why "stage 2 approved" --approved-by lead:web
merged {id}
```

Someone else changes an unrelated setting afterwards:

```console
$ umbrachyx commit main site/banner=sale --who ops:marketing --why "weekend sale banner"
committed {id} to main
```

## Stage 2 goes wrong

Checkout errors rise at 50%. Take back that one change, one step back
from the latest on `main` (its id works too):

```console
$ umbrachyx revert main main~1 --who lead:web --why "checkout errors rose at 50%"
reverted with {id}
$ umbrachyx get main flag/checkout-v2/percent
10
$ umbrachyx get main site/banner
sale
```

The flag is back at stage 1 and the banner, set after stage 2, is still
there. Had someone already changed the flag again, the revert would have
stopped and listed it as a conflict instead of overwriting it.

## The whole story of one setting

```console
$ umbrachyx history main flag/checkout-v2/percent
{time}  {id}  10  by lead:web  "checkout errors rose at 50%"
{time}  {id}  50  by lead:web  "stage 2 approved"  approved by lead:web
{time}  {id}  10  by lead:web  "stage 1 approved"  approved by lead:web
{time}  {id}  0  by ops:release  "initial settings"
$ umbrachyx log main
{id}  {time}  lead:web  "checkout errors rose at 50%"
{id}  {time}  ops:marketing  "weekend sale banner"
{id}  {time}  lead:web  "stage 2 approved"  approved by lead:web
{id}  {time}  lead:web  "stage 1 approved"  approved by lead:web
{id}  {time}  ops:release  "initial settings"
```

Every value the flag ever had is there with who put it there, why, and
who approved it, including the stage that was taken back.
