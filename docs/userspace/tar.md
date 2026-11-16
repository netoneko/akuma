# tar

Ported `tar` implementation with streaming extraction.

Docs live at [`userspace/tar/docs/`](../../userspace/tar/docs/):
- `IMPLEMENTATION_PLAN.md`, `STREAMING_EXTRACTION.md`.

Extraction stays inside its root: parent components resolve with chroot semantics
(`format::resolve_in_root`), and a failed entry makes `tar` exit 1. The incident record is box's
[`ISSUES_2026_11_16.md`](../../userspace/box/docs/ISSUES_2026_11_16.md) (B1, B5, B6).
