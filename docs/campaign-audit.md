# Campaign process audit

This audit uses the installed full-game levels 01–19, with a freshly initialized process runtime, actual map walls and raw cell types, normal-difficulty object placements, and all key flags enabled. It queries staged process acceptance for one linked trigger per procedure. An accepted procedure has passed runtime compilation and trigger gating; it does **not** prove that the player can reach it, that presentation assets can be preflighted, or that the level is playable from start to finish. Locked procedures and routes requiring earlier state changes need separate checks.

| Levels | Exit check |
| --- | --- |
| 01–11, 13–15, 17–18 | Every directly linked exit procedure accepted in the fresh runtime. |
| 12 | Exit process 29 is locked. Its authored direct unlocker, process 27 at `floor 35 31`, is rejected: its final `move 29 33 0 -510 64` has a trailing numeric argument whose meaning remains unresolved. The process also locks 26, changes a model, and moves several walls, so skipping it would lose gameplay state. |
| 16 | Exit process 9 is locked initially. Its unlocker, process 7 at `destroy 27 47`, prepares six staged actions with the installed data. The host must still preflight and commit those actions and the boss route remains to be verified. |
| 19 | Process 30 is the exit but has no ordinary map trigger; it is activated through the addon-boss rule. This audit does not establish that encounter's completion. |

A separate initial-state scan found 50 linked procedures rejected as unsupported, including cosmetic or optional ones; this count excludes locked procedures whose action support could not be isolated through a fresh trigger. It is not a count of blocked exits. Notable candidates for further investigation are:

- Level 10 process 8 and level 11 process 18 use extra numeric `move` arguments, as does the confirmed level 12 unlocker. Their route importance is not established here.
- Several linked processes combine wall texture `change` commands with wall motion or other actions, including level 10 process 4, level 11 process 20, level 15 process 5, level 18 processes 2, 4, 5, 9, 13 and 16, and level 19 process 5. Their rejection and route impact require identifying the exact unsupported action and testing the intended traversal.
- Level 09 process 20 combines door rotation with `call 21`; the call and its effects need investigation before treating this route as functional.
- Level 01 processes 6 and 9 contain `light` actions; level 14 process 2 and level 16 process 10 contain `source` actions. Their rejection is observed, but this scan does not establish that they block progression.

Procedures may also be rejected because a referenced object or wall has changed since map load, because an action or property is unsupported, or because a linked trigger expects a different runtime phase. The audit deliberately does not classify all 50 by cause without tracing those cases.
