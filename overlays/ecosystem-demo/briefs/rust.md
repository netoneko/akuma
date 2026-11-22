# Brief: agent B — the Git service, in Rust

You are agent B. You work alone in a Linux box with a Rust toolchain (`cargo`, `rustc`), `git`, `curl`, `bash`
and internet access (crates.io works). Your workspace is `/work`, an empty git repository. Other agents write
other services in other boxes; you never see their work and must not wait for them.

## Your task

Write an HTTP **Git service** in Rust that implements the contract below, build it with
`cargo build --release`, and test it yourself with the ordinary `git` CLI (clone, commit, push, fetch)
against bare repositories you create under a temporary directory.

You may use crates.io crates, or implement the protocol by driving `git http-backend` /
`git upload-pack` / `git receive-pack`. Whatever you choose, it must work with a stock `git` client.

## Delivery rules (these are how your work is judged)

1. Commit your source in `/work` (several small commits are good; a `.gitignore` for `target/` too).
2. Provide an executable `/work/run.sh` that starts the finished service **in the foreground**. It must
   read the listen port from the environment variable `PORT` and the directory that holds the bare
   repositories from `GIT_ROOT`, listen on `0.0.0.0`, and build the binary itself if it is not built yet.
   The operator creates the bare repositories (`rust.git`, `scala.git`, `python.git`, default branch `main`)
   under `GIT_ROOT` before it starts your service; your service must **not** create repositories.
3. Do not bind to any fixed port during your own testing other than ports >= 20000.
4. When finished, write `/work/NOTES.md` (what you built, how you tested it, anything incomplete) and commit.
   Then stop. Be honest in the notes about what does not work.

## Contract: Git service

