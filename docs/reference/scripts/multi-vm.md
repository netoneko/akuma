# Multi-VM / hang hunting scripts

Grade: — (index)

| Script | What it does |
|---|---|
| [`run_multiple.sh`](../../../scripts/run_multiple.sh) | Launches N parallel Akuma boots (own disk, own port band, own log) with a log-stall watchdog, for hunting hangs that don't reproduce every boot. `scripts/run_multiple.sh 8`. Background: [`../../archive/STABILITY_URGENT_ISSUES.md`](../../archive/STABILITY_URGENT_ISSUES.md). |
| [`run_two_vms.sh`](../../../scripts/run_two_vms.sh) | Boots the two-VM agent demo (a `meow` VM + a `llama.cpp` server VM wired together over SLIRP). Used by [`../../../acceptance/03_two_vms_agent_workflow.md`](../../../acceptance/03_two_vms_agent_workflow.md); background in [`../../archive/TWO_VMS_AGENT_DEMO.md`](../../archive/TWO_VMS_AGENT_DEMO.md). |

Back to [`README.md`](README.md).
