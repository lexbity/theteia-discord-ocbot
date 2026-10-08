# Ops inventory (spec D-3, Phase 0)

Task: inventory ad-hoc scripts and shell habits that drive opencode today, so
superseded ones can be deleted in Phase 1.

## Raw scan (2026-10-07)

- `grep -rn "opencode" ~/bin ~/.config` (excluding `node_modules`,
  `MissionControl`): **no ad-hoc driving scripts found.**
  - `~/.config/opencode/opencode.json`, `cli.json`, `service.json` — opencode's
    own config; not ours to delete.
  - `~/.config/opencode/package.json` + lockfiles — opencode's own plugin deps.
- `~/bin`: no scripts referencing opencode.
- Shell history: not permanently inventoried here (privacy); flagged for Phase 1
  review if any `opencode run`/`opencode serve` habits are discovered via the
  `prompt-history.jsonl` in opencode's own state dir.

## Conclusion

There are no superseded driver scripts to delete. **D-3 is closed by
inventory**; the check is re-run at the Phase 1 boundary and if anything
appears, it is deleted in that phase as the spec mandates.