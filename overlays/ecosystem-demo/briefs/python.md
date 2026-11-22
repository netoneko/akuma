# Brief: agent D — the acceptance tests, in Python

You are agent D. You work alone in a Linux box with Python 3, `pytest`, `requests`, `git`, `curl` and `bash`.
Your workspace is `/work`, an empty git repository. You run on a different machine from the two services under test.
You never see their source and must not try to; you test them only over HTTP, as a client.

## Your task

Two services, written independently by other agents, implement the contract below. Write an integration
test suite in Python that checks both against the contract, **run it** against the live services, and
report the results honestly. The base URLs are given to you in the environment:

- `BOARD_URL` — the task board
- `GIT_URL` — the Git service (repositories `rust`, `scala` and `python` exist, bare, default branch `main`,
  at `$GIT_URL/<name>.git`)

Never hard-code addresses; read the environment. Tests that create data should use disposable
data and tolerate the service already holding data from other tests. Cover: health, positive paths,
field defaults, every error rule (bad JSON, missing/unknown fields, empty patch, wrong ids, unknown
paths and methods, wrong content type of success responses), message ordering, status values, and for
git: clone, commit, push to `main`, fetch from a second clone, a push to a repository that does not exist
being refused. Use the `git` CLI for git.

## Delivery rules (these are how your work is judged)

1. Commit your suite in `/work` (several small commits are good).
2. Provide an executable `/work/run_tests.sh` that runs the whole suite against `BOARD_URL` and `GIT_URL`
   and exits nonzero if any test fails.
3. Run it, and write `/work/REPORT.md`: per service, what passed and what failed, with the evidence. If a test
   fails, decide carefully whether the service or your test is wrong, and say which and why. Commit the report. Then stop.

## Contract

