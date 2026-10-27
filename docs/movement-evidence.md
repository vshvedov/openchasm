# Original movement and mouse evidence

This records read-only analysis of the owner's Steam Chasm: The Rift 2022 executable, SHA-256 `72c189ca808e026d59844c835fbc21a780038989fd22ff150479c0490ac5c949`. Addresses are preferred-image virtual addresses (image base `0x140000000`), not portable runtime addresses. No executable code or game assets are included. The independently reconstructed engine was not used as proof of original behavior. These findings describe the remaster, not an assertion about the DOS executable.

## Actionable defaults

| Quantity | Verified normal-player behavior |
| --- | --- |
| Running | Target **5.1975 tiles/s**, subject to float rounding |
| Walking | Target **3.1185 tiles/s**, 0.6 times running |
| Autorun | Default **on**; run key XOR autorun selects running; holding run with autorun on walks |
| Run key in installed config | Left Shift, packed binding `0x4000e1` |
| Mouse | **0.003067961661145091 rad/count**, binary32 `0x3b490fdb`, approximately π/1024, times sensitivity |
| Mouse sensitivity | Executable default and installed config both **1** |
| Controlled acceleration/braking | **52.5 tiles/s²**, Euclidean cap on change toward the desired XY velocity, on ground and in air |
| Diagonal movement | Same target speed as axial movement |
| Physics integration | **500 μs** substeps; residual time is carried, not integrated as a shorter final step |
| Gravity | Binary32 **8.7 tiles/s²** (`0x410b3333`) |
| Jump | Adds **2.75 tiles/s** vertical velocity with sufficient clearance; **1** with clearance below **1.328125 tiles** |
| Jump repeat | Held jump can repeat when grounded; **400,000 μs** cooldown from each jump |
| Initial collision dimensions | Height **1920/2048 = 0.9375 tiles**, radius **64/256 = 0.25 tiles** |
| Living eye baseline | **0.75 tiles** above replicated feet, plus camera bob |

The run/walk values follow the executable's default `Movement Speed = 1` and normal player scale of 1. They are mathematical targets, not an empirical timing measurement through a level. The installed user config explicitly contains `Always Run = 1`, `Mouse Sensitivity = 1`, `Player Friction = 1`, `Invert Look = 0`, and `Jump = 0x8002c` (Space). `Movement Speed` belongs to a separate settings collection; its executable default is verified, but no runtime memory measurement was made. None of these settings were changed during this research.

## Keyboard input and direction

`0x140086510` gathers opposite movement actions into two signed axes. If both are zero, it adds no movement request. It reads `Always Run` from the runtime settings collection at `+0x1068` and the `Speed Up` action at input-context `+0x4b8`. At `0x1400865ed`, differing booleans select 1; equal booleans retain 0.6. The magnitude is combined with other input sources using maximum, while axis vectors are added (`0x140086603` onward). Input axes, magnitude, and mouse rotation accumulators are cleared by `0x140087be0`.

The camera/input update clamps requested magnitude to [0,1], stores it at camera `+0x28`, and computes movement direction with `atan2` from the yaw-rotated axes (`0x1400238b0`–`0x14002393e`). Magnitude is **not** taken from the length of the diagonal key vector. Thus keyboard diagonals have the same speed as cardinal directions. Opposite keys cancel before this stage.

The client writes this magnitude directly as a float into the command (`0x140006c6a`–`0x140006c72`). The receiving player command handler clamps it to [0,1] and copies it to player `+0xd0` (`0x14014a310`–`0x14014a367`), with no additional speed factor. Movement direction is encoded as a 16-bit angle, truncating `angle * 65536 / (2π)` at `0x140006c3f`–`0x140006c63`, and reconstructed with `angle16 / 65536 * 2π` at `0x14014a36d`–`0x14014a380`. Tiny direction quantization differences are therefore expected from a floating-point-only implementation.

The existing renderer's documented coordinates apply: forward `(cos(yaw), sin(yaw))`, screen right `(sin(yaw), -cos(yaw))`. Positive mouse X decreases yaw.

## Controlled XY velocity and time

The normal player updater `0x140149480` constructs the movement descriptor at `0x14014981f` onward:

- `m`: requested magnitude from player `+0xd0`.
- `A = 15 * Movement Speed`.
- `theta`: requested direction from player `+0xd4`.
- Gravity = 8.7.
- `F = 1.5 + 2 * clamp(Player Friction, 0, 1)`.
- Mode = 0, ground-drag descriptor = 0, bounce factor = 0.

`Movement Speed` is named at string VA `0x14074e138`; its initialization at `0x140155f54`–`0x140155f9a` explicitly selects entry 3, default string `1`, range [0,4]. Runtime entries have stride `0x78`; the updater reads this entry at `[0x1408f5fb8]+0x168`, then multiplies by 15 at `0x1401498c9`. `Player Friction` is user settings entry 46, runtime offset `0x1590`; the clamp and conversion to F are at `0x140149851`–`0x1401498a9`. Player scalar `s` at `+0x30` initializes to 1 at `0x140148e92`.

In generic physics update `0x140143e20`, for ordinary finite inputs:

```text
h = dt_microseconds / 1_000_000
speed = s * 0.3465000092983246 * A * m
wanted = speed * (cos(theta), sin(theta))
delta = wanted - controlled_xy
L = length(delta)
if L > 0:
    controlled_xy += (delta / L) * min(L, A * s * F * h)
```

The multiplier is binary32 `0x3eb16873` at `0x140761e64`. Target computation occurs at `0x140143ea7`–`0x140143efd`; Euclidean length, normalization, capped approach, and stores occur at `0x140143f02`–`0x140143f94`. This is a cap on the **whole velocity difference**, not independent caps on X and Y. The same formula brakes to zero with no movement and changes direction during strafing. For finite nonzero L, the branch unconditionally selects descriptor F and does not require ground contact. The apparent alternate ground branch tests negative vector length and does not alter ordinary finite behavior.

Default run speed is `15 * 0.3465 = 5.1975`; default F is 3.5, so approach rate is 52.5. Continuous ideal time from rest to full run is approximately 0.099 seconds; actual integration uses binary32 and discrete substeps. Raising `Movement Speed` scales both target speed and approach rate. At `Movement Speed = 0`, both become zero: an existing controlled velocity is not actively braked by this approach formula. Configuring friction to zero still gives F=1.5; it does not eliminate braking.

At `0x1401498d5` the player adds elapsed game-clock microseconds to an accumulator at `+0xc0`. It repeatedly calls the integrator with `500` at `0x140149a1d`–`0x140149a2b`, subtracts 500, and retains the remainder. The integrator explicitly divides time by 1,000,000 at `0x140143e6b`–`0x140143e7b`. There is no shorter trailing substep.

Position uses **updated** velocity. At `0x140143fc5`–`0x140144005`, requested displacement is `(controlled_xy + external_xy) * h`, and vertical displacement is `external_z * h`; collision resolution follows at `0x14014402a`. This is semi-implicit Euler, not trapezoidal integration.

## Jump, falling, and collision

The player stores controlled XY at `+0x1c/+0x20`, separately from external/impulse XYZ at `+0x10/+0x14/+0x18`. Gravity acts on the latter Z component before position integration (`0x140143e8b`–`0x140143e98`).

Jump eligibility is tested inside the 500 μs loop (`0x140149973` onward): jump held, feet Z exactly equal to current support height, and cooldown <= 0. Clearance is player `+0xdc` minus support height. Below 1.328125 it adds a vertical impulse of 1; otherwise it adds 2.75 (`0x140149998`–`0x140149a05`). The scalar multiplication/division cancels for normal scale 1. It adds to existing velocity; it does not simply assign the impulse value. The cooldown is set to 400,000 μs at `0x140149a0a`. Cooldown is decremented and clamped once per outer player update (`0x140149702` onward), before the substep loop, not once per substep. Releasing jump is not required for a later jump.

The jump clearance field is computed each outer player update at `0x140149575`–`0x1401495de`: default 2, or the byte at `0x140966dc0[x*64+y] / 64` for valid map coordinates. This is an absolute local upper height, so subtracting support Z gives the full available vertical span, **not headroom above the body**. Its preprocessing (`0x140178c60`) initializes the 64×64 table to 128 (2 tiles) and samples cell centres using `0x140178920`. That query starts with upper height 4096 raw vertical units and takes the minimum eligible nearby object underside above 1920; it divides by 32 before storing the table byte. The complete object eligibility/footprint rules are not implemented by this document, so a clone using only its geometric ceiling is an approximation around overhead models.

No terminal-fall-speed clamp is present in this normal-player integrator. Its generic quadratic drag calculation includes a vertical term, but `0x140144217`–`0x140144269` multiplies that term by `(mode != 0)`; normal player mode is zero, disabling vertical drag. A conservative simulation safety bound can be useful, but is not original terminal-velocity evidence.

The non-bouncing collision response is explicit at `0x140143bb8`–`0x140143ce2`. With contact normal `n`, it projects remaining displacement into the tangent plane. External velocity `e` is changed only if `dot(e,n)<0`, using `e -= n*dot(e,n)`. Controlled XY uses the same rule with `n.xy`: if `dot(v,n.xy)<0`, then `v -= n.xy*dot(v,n.xy)`. Consequently a wall preserves tangential velocity while removing inward velocity; an upward floor normal removes falling vertical velocity. This requires actual contact normals for angled walls; zeroing an arbitrary world axis is not equivalent.

The collision wrapper divides requested displacement into segments of at most 0.125 tiles (`0x1401438f0`–`0x140143975`), queries collision, clips residual motion, and repeats. It passes initial player height 1920/2048 and radius 64/256 to the collision query (`0x140143916`–`0x14014398b`); their packed initialization is `0x00400780` at `0x140148e8b`. Support height updates from collision results at `0x140143a8a`–`0x140143abf`. When support is above feet, post-collision stepping raises Z by at most `3*h` toward support (`0x14014406e`–`0x14014409a`). **The maximum permitted obstacle step height and all geometry eligibility conditions have not yet been established.**

The living eye-position path adds 0.75 and camera bob to replicated feet height (`0x140006e08`–`0x140006e2e`). This is distinct from collision body height. Head-bob waveform and all stance/damage modifications are outside this baseline.

External XY velocity has additional drag separate from controlled movement: quadratic per-component drag using body dimensions, followed by a vector speed reduction of `min(speed,6*h)` (`0x140144148`–`0x1401442e4`). These terms must not be applied to the controlled walking/running channel. Weapon knockback and all other external-force producers have not been fully traced.

## Mouse input and default bindings

The game input path calls an SDL dynamic-API thunk `0x140325b90`, which dispatches through `0x1407e97b0`. SDL initialization sets that pointer to `0x14035c190` at `0x140329029`–`0x140329030`. This getter copies integer relative X/Y accumulators at `0x1408feda8/0x1408fedac` and resets them to zero. Its structure and the local SDL dependency's dynamic API declaration identify the SDL relative-state path.

The game reads sensitivity from runtime settings `+0xff0`, multiplies by binary32 π/1024 at `0x1400866d7`–`0x1400866ed`, converts relative counts to float, and subtracts the resulting deltas from yaw and pitch accumulators at `0x14008673e` and `0x140086767`. `Invert Look` reverses the pitch sign. The local camera adds those accumulated deltas at `0x1400066e3` onward. There is **no game-level elapsed-time, framebuffer-size, FOV, or monitor-scale factor** in this mouse path and no smoothing history in this traced calculation. At sensitivity 1, 2048 relative counts make a complete turn. The configured/default sensitivity is 1, with executable descriptor bounds [0.01,1.5].

This establishes radians per **SDL relative count**, not physical centimetres per turn. SDL's upstream event accumulation has configurable scaling paths, and Windows/Proton versus native Wayland may deliver different counts for the same physical device motion. Raw-input backend selection, OS pointer acceleration, device DPI, and compositor logical-coordinate treatment were not established by this trace. Do not compensate by multiplying the game coefficient by drawable/logical window ratio without separate measurement. The SDL accumulator getter itself performs no such conversion.

Default packed bindings use the low 10 bits for SDL scancode, decoded at `0x140086490`. `0xe1 = 225` is `SDL_SCANCODE_LSHIFT`; `0x2c = 44` is `SDL_SCANCODE_SPACE`, confirmed by the installed SDL 2.0 headers (`SDL_scancode.h`). Higher packed bits also encode controller bindings and must not be mistaken for part of a keyboard code.

Keyboard turning is separate: camera update `0x1400237fc`–`0x140023820` divides elapsed microseconds by 1,000,000 and multiplies by **3 radians/s**. Pitch is clamped to **±π/2 (±90°)**: `0x14002376a`–`0x140023799` compare against constants at `0x140761f98`/`0x1407623d0`, storing exact binary32 bits `0x3fc90fdb` or `0xbfc90fdb`. The independent reconstruction’s narrower 58.5° limit is not the remaster limit. Yaw wrapping in `0x14002374d`–`0x140023765` adds or subtracts one full revolution when outside the range. The user-config fields named `Move Acceleration` and stick dead zones are not evidence of keyboard acceleration; the normal keyboard target and physics formula above do not read those settings.

## Scope and implementation checks

A faithful baseline should use the verified autorun inversion, mouse coefficient, normalized direction, target speed, capped vector approach, gravity/jump timing, and velocity clipping. A Quake 1 option is a separate user-requested movement model and should remain explicitly selectable. Its constants and unit mapping must not replace or be attributed to the original formulas above.

Useful focused checks are: axial versus diagonal equal speed; run/walk ratio; both autorun states with Shift; stop/reverse acceleration; equal approach while airborne; preservation of wall tangent velocity; fixed-step residual carry; gravity-before-displacement; held jump after landing/cooldown; and yaw response to a known relative-count total. These verify the implemented mathematical behavior, not complete replay equivalence with the original. Full 1:1 collision, moving platforms, stance/head-bob, input-backend physical sensitivity, and rendered input latency still require further tracing or controlled comparison.
