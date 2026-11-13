# Example: a support desk where an agent proposes and a person decides

A support agent reads tickets and proposes account credits. Each proposal
is one branch, so a person can approve one and leave another alone. Later
a customer asks for their data to be deleted.

Every command below is run by the test suite as written. `{id}` and
`{time}` stand for a commit id and a time.

## The data

```console
$ umbrachyx commit main customer/31/credit=0 customer/31/email=c31@example.test customer/44/credit=0 ticket/8812/status=open ticket/9001/status=open --who app:billing --why "import"
committed {id} to main
```

## One branch per ticket

```console
$ umbrachyx branch agent/support/8812 --from main
created agent/support/8812 from main
$ umbrachyx commit agent/support/8812 customer/31/credit=20 ticket/8812/status=refunded --who agent:support --why "ticket 8812: charged twice in March"
committed {id} to agent/support/8812
$ umbrachyx branch agent/support/9001 --from main
created agent/support/9001 from main
$ umbrachyx commit agent/support/9001 customer/44/credit=500 ticket/9001/status=refunded --who agent:support --why "ticket 9001: asks for a year of fees back"
committed {id} to agent/support/9001
```

Nothing has changed for customers yet:

```console
$ umbrachyx get main customer/31/credit
0
$ umbrachyx get main customer/44/credit
0
```

## The first proposal is approved

```console
$ umbrachyx diff main agent/support/8812
~ customer/31/credit: 0 -> 20
~ ticket/8812/status: open -> refunded
$ umbrachyx merge main agent/support/8812 --who lead:support --why "ticket 8812: the March invoices show two charges" --approved-by lead:support
merged {id}
```

## The second is not

`main` has moved since this branch began, so comparing the two would also
show the first ticket's credit as a difference. What the proposal itself
changes is the branch against the commit it started from, one step back:

```console
$ umbrachyx diff agent/support/9001~1 agent/support/9001
~ customer/44/credit: 0 -> 500
~ ticket/9001/status: open -> refunded
```

Five hundred is beyond what the desk may give. The reviewer does not
merge it, and `main` stays as it was. The proposal, with the agent's
reason, stays on its branch for whoever asks later what the agent tried.

```console
$ umbrachyx get main customer/44/credit
0
$ umbrachyx log main --limit 2
{id}  {time}  lead:support  "ticket 8812: the March invoices show two charges"  approved by lead:support
{id}  {time}  app:billing  "import"
```

## "Who credited customer 31, and who signed it off?"

```console
$ umbrachyx history main customer/31/credit
{time}  {id}  20  by lead:support  "ticket 8812: the March invoices show two charges"  approved by lead:support
{time}  {id}  0  by app:billing  "import"
$ umbrachyx history agent/support/8812 customer/31/credit
{time}  {id}  20  by agent:support  "ticket 8812: charged twice in March"
{time}  {id}  0  by app:billing  "import"
```

On `main` the credit belongs to the person who approved it; the agent's
own commit and its reason are on the ticket's branch.

## "Delete my data."

Every value is encrypted under a key for its subject: `customer/31/email`
and `customer/31/credit` belong to `customer/31`. Erasing the subject
destroys the key. The values can never be read again, on any branch,
while the history keeps its shape.

```console
$ umbrachyx erase customer/31 --who ops:privacy --why "deletion request 1207" --approved-by lead:support
erased customer/31
$ umbrachyx get main customer/31/email
(erased)
$ umbrachyx history main customer/31/credit
{time}  {id}  erased  by lead:support  "ticket 8812: the March invoices show two charges"  approved by lead:support
{time}  {id}  erased  by app:billing  "import"
$ umbrachyx get agent/support/8812 customer/31/credit
(erased)
```

The ticket is a different subject and still reads, as does every other
customer:

```console
$ umbrachyx get main ticket/8812/status
refunded
$ umbrachyx get main customer/44/credit
0
$ umbrachyx erasures
{time}  customer/31  by ops:privacy  "deletion request 1207"  approved by lead:support
```
