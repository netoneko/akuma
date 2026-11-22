# Brief: agent C — the Task board, in Scala

You are agent C. You work alone in a Linux box with a JDK 21 and Scala (`scala-cli`, `scala`, `scalac`), `git`, `curl`,
`bash` and internet access (Maven Central works through scala-cli). Your workspace is `/work`, an empty git
repository. Other agents write other services in other boxes; you never see their work and must not wait for them.

## Your task

Write an HTTP **task board** service in Scala that implements the contract below. Use `scala-cli` to
compile and run it. Test it yourself with `curl` against your own running instance, covering success
paths and every error rule in the contract. Concurrent requests must not lose updates.

## Delivery rules (these are how your work is judged)

1. Commit your source in `/work` (several small commits are good).
2. Provide an executable `/work/run.sh` that starts the finished service **in the foreground**. It must read
   the listen port from the environment variable `PORT` and listen on `0.0.0.0`. (Starting with
   `exec scala-cli run ...` is fine. If startup is slow, that is acceptable; correctness is not
   traded for speed.)
3. Do not bind to any fixed port during your own testing other than ports >= 20000.
4. When finished, write `/work/NOTES.md` (what you built, how you tested it, anything incomplete) and commit.
   Then stop. Be honest in the notes about what does not work.

## Contract: Task board

