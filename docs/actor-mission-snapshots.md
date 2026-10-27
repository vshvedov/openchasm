# Actor and mission snapshots

`ActorState`, `EnemyAi`, and `MissionState` expose explicit mutable-state DTOs and validate complete snapshots before changing a subsystem. Actor records use stable authored source IDs and verify authored monster ID and resolved definition against the newly loaded map. AI records use those source IDs, preserve mode, last-seen position, cooldown, run phase, attack phase and invisibility flag, and validate attack slots, frame counts and elapsed phase against loaded clip metadata. Legacy AI can be attacking without an animation phase. A zero-health actor may retain its prior AI mode until the next tick, because combat follows the AI update.

Mission snapshots preserve first-message history, remaining end timer and completion. Restore validates process/message references and timer range against the loaded plan. Static content, definitions and animation assets are not included.

These are separate subsystem primitives, not a complete save. A future loader must restore actor state before AI into a temporary Session, verify content and difficulty, validate other subsystem references and commit the whole Session atomically. The actual binary codec and file persistence remain separate work.
