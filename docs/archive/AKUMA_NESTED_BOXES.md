# Nested boxes for agent fleets and image builds

**Status: proposal.** Let a process inside a box create and prepare a child
sandbox without asking Box 0 to do it. Existing nested box registration gives
us a hierarchy, but neither bubblewrap in a box nor an OCI image build in a
box works today. The aim is to bootstrap a dedicated kernel once in Box 0,
start a supervisor and agents in separately rooted boxes, and do subsequent
agent work, sandbox creation, and image builds within that hierarchy. Box 0
remains the bootstrap and recovery authority; normal workloads need not run
there.

## Why now

Bubblewrap already required much of the machinery: a private mount namespace,
namespace-local bind mounts, temporary filesystems, `pivot_root`, a builder
permit (`NsCreator`), and a final lock that removes that permit. Fork and exec
carry the namespace. The box registry also has parent/descendant access rules
and validates nested roots. These are useful pieces, not proof that nesting is
already safe: `clone_namespace` explicitly refuses callers outside Box 0 and
constructs a fresh namespace with `/`, rather than copying a boxed caller's
actual filesystem view. Simply removing that check could expose the host root.

Today a boxed process can register a child box, but `MOUNT_IN_NS` is host-root
only. `box build` runs its builder in Box 0; for each Dockerfile `RUN`, it
registers a box, mounts its OCI overlay and `/proc`, executes the command, and
commits the upper layer. Consequently an agent in a separate root cannot build
an image with `RUN` steps using the current path. Supporting that path would
make building images inside an agent's own workspace practical, rather than
requiring a privileged Box 0 builder for every build.

## Proposed boundary

Nested sandbox creation must be a separately gated kernel feature, dependent on
the existing container and private-namespace facilities. With the gate off,
preserve the current refusal of `CLONE_NEWNS` from a box and the host-only
`MOUNT_IN_NS` policy; existing nested box registration remains available under
its current rules. Choose the default per kernel deliberately and expose the
gate in its build configuration. A dedicated agent kernel can enable it without
changing the isolation policy of smaller or existing deployments.

1. Create a child from the caller's **effective mount namespace and root**,
   including bind mounts and read-only restrictions. Resolve and pin all
   source paths through the parent's view; never fall back to the global mount
   table when a child has a private root. Preserve the parent view across
   `pivot_root`, `..`, symlinks, mount replacement, and unmount. Record actual
   ancestry for private namespaces as well as registered boxes. A child may
   not widen its view by nesting again.
2. Delegate a narrow builder authority for the **new child only**, bound to
   its creator and construction lifetime. It can compose and lock that child;
   it cannot mutate the parent's mounts, a sibling, an already running or
   locked child, or global mounts. Generalize `NsCreator` and the
   `MOUNT_IN_NS` authorization around that invariant instead of granting
   boxed callers host-root authority. Decide the authority required to create
   children even after capability drops; do not silently equate uid 0 with a
   full Linux capability or user-namespace model.
3. Adapt `box build` and `box run` to compose a descendant's overlay from
   layer and upper directories visible to the parent. Pin and validate those
   objects in the parent's namespace, preserve read-only mounts, and respect
   the overlay's current same-underlying-filesystem requirement. Give each
   agent a writable image store and build context, or explicitly provision
   shared read-only layers; a shared writable store needs its own authorization,
   concurrency, quota and cleanup design. The child must not gain access to
   arbitrary host paths by supplying overlay or bind source names.
4. Keep network and other boundaries explicit. Private mount namespaces do
   not provide Linux network, PID or user namespaces; shared smoltcp denies
   newly created private-namespace sockets access to host IPv4 loopback, while
   intentionally inherited sockets retain their original policy. Define how
   this policy composes across descendants and other stack types, and close
   unintended inherited descriptors. Capability-aware DAC protects other
   owners' files after a drop, but uid-0-owned files remain accessible to uid 0;
   separate roots and carefully chosen binds remain essential. Consider
   process, memory, storage and network limits before treating an agent fleet
   as mutually hostile.

## Dedicated KVM kernels

On a dedicated KVM guest running headless Akuma, a minimal Box 0 bootstrap can
provision a supervisor, per-agent roots, workspace binds, image stores and
network policy. The supervisor then launches agents in distinct child boxes;
each can create a bubblewrap sandbox for Codex or descendant build boxes without
returning the work to Box 0. Sibling isolation and teardown follow the box
hierarchy, with the supervisor retaining only the authority it needs. The
kernel and bootstrap remain trusted, and sharing a kernel is not a VM boundary.

Denver and Taoxichuan are attractive smaller-kernel targets for the same
pattern, but this is a target architecture, not a claim that their present
feature sets implement it. Audit and wire their container, private-namespace,
VFS, process and network facilities, along with the required userspace and
storage support, before promising parity. Keep headless Akuma as the first
end-to-end target.

## Validation and open work

Test parent → child → grandchild with distinct roots and nested bind mounts;
prove that child mount, pivot, unmount, symlink and `..` operations cannot
reach or alter a parent, sibling or host mount. Exercise lifetime and cleanup,
locked namespaces, inherited descriptors, capability drops, hierarchy access,
and both allowed and denied operations on amd64 and AArch64. Run real
bubblewrap and Codex inside an already boxed separate root, and build an image
there with `FROM`, `COPY`, and `RUN`, checking the result and a sibling's
inaccessibility. Include read-only sources and reject overlay sources outside
the parent's view. Run the kernel build matrix and explicit feature-gate tests
for every kernel that claims support.

Open design decisions include the delegation API and its revocation, safe
namespace snapshot semantics, image-store sharing and quotas, network stack
inheritance, and what happens to descendants when a parent exits. Existing
uid-map stubs, shared-stack behavior, resource limits and overlay limitations
must be described accurately; this work alone does not implement full Linux
containers or Docker-in-Docker.

Context: [`BWRAP_ON_BOXES.md`](BWRAP_ON_BOXES.md),
[`BOX0_ONLY_AUDIT.md`](BOX0_ONLY_AUDIT.md),
[`../reference/subsystems/containers.md`](../reference/subsystems/containers.md),
[`../../userspace/box/docs/BOX_BUILD.md`](../../userspace/box/docs/BOX_BUILD.md).
