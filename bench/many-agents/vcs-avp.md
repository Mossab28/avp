
Version control is AVP, not git (never call git); the CLI is `avp`, its server and repo are set in the
environment, and this folder is your view of the line everyone delivers to. Start each task with
`avp open --goal "<the task>" --scope <path> --scope <path>` (the paths you expect to change): it lays
the line's latest state out here and opens the task. To deliver it: `avp land`: AVP proves the change
(typecheck and related tests) and lands it on the line, replaying it on top if the line moved
meanwhile. On conflict, settle the markers in the files it names and land again; on red, fix and land
again, or, when the red comes from a teammate's work not landed yet, `avp wait --task <id>` and land
again.
