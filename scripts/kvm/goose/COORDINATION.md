# Who is touching what (supervisor <-> goose-glm, via GitHub)

Update a row when you START work on a file and delete it when your commit is merged. Merge, never rebase; supervisor pushes `origin
rust-org-il-experiment`, glm pushes `litter taoxichuan-s4`. Each side merges the other's branch before starting a unit of work.

| since | who | files / area | what |
|---|---|---|---|
| 2026-10-31 | supervisor | `taoxichuan/src/{child,main,exec_twin,pipes,sock}.rs` | init as pid 1 on a pool thread; `BorrowCell` accessors (committed) |
| 2026-10-31 | supervisor | `taoxichuan/src/{net,futex}.rs`, new/extended crates | extraction plan X2 (clock/rng), X1 (futex), X3 (user access) - not started |
| - | glm | `taoxichuan/src/{main,sock}.rs` (epoll/eventfd arms) | eventfd/epoll - brief `epoll-babysit.md`, not sent |

Rules: nothing in `amd64/` changes without the supervisor; glm adds syscall arms next to the existing ones and does not move code (moving is the
supervisor's extraction work); a conflict is resolved toward keeping both sides, then rebuilt and run through the battery.
