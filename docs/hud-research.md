# Remaster HUD evidence

This records local, read-only examination of the owned 2022 executable, SHA-256
`72c189ca808e026d59844c835fbc21a780038989fd22ff150479c0490ac5c949`.
Addresses are original executable virtual addresses. No executable code, shader
blobs or proprietary artwork is included here. The independent reconstruction
is not evidence for these findings.

## Draw path and scale

The call at `0x1400075a8` dispatches HUD vtable slot `+0x38`. The GPU HUD
constructor installs vtable `0x1407456c0` at `0x140033013`; that slot points to
`0x140036400`. The world/weapon tone pass has already completed at this point
(see `remaster-rendering.md`).

`0x140030770` updates the integer HUD scale `s` at object offset `+0x24`:

1. Read the `HUD Scale` setting. Its default is zero, also present in the local
   user's configuration; the setting registration is at `0x140154ab2`.
2. For zero, `0x140030700` computes `H / 240 * 0.75`, compares its distances to
   floor and ceil, and chooses the nearest (ceil on a tie). This is nearest
   integer to `H / 320` for normal dimensions.
3. For a nonzero setting, use `setting + 1`.
4. Decrement until `320*s <= W`, then enforce a minimum of **2**.

The minimum is applied after the fit loop. An implementation supporting widths
below 640 needs an explicit adaptation; a one-pixel scale at such widths is not
the original algorithm.

The bar origin is `x0 = floor(W/2) - floor(320*s/2)`; see
`0x140036431–0x140036456`. The original draws it in a bottom-origin coordinate
system, with bottom `0`, top `32*s`, and width `320*s`
(`0x140036486–0x140036510`). The existing 1920×1080 original screenshot confirms
the resulting bar at x=480..1440, y=984..1080 in top-origin image coordinates.
It is centered and does not stretch to the modern display's full width.

The HUD vertex shader registration at `0x1400524ba` selects a shader whose
position formula is `2 * position / viewport_size - 1`, and whose UV formula is
`texcoord / texture_size`. Thus CPU-authored integer pixel coordinates control
the layout; there is no independent 4:3 horizontal stretch in that shader.

## Resources and rectangles

Dimensions below are from the owned local CEL headers / PNG, not guesses from
screen captures. Rectangles use edge coordinates, with exclusive right/bottom
edges; atlas rows are top-origin. Screen placement in this table is relative to
the bar's left edge and the screen's bottom edge, before multiplication by `s`.

| Resource | Dimensions | Verified use |
| --- | --- | --- |
| `status2.cel` | 320×76 | Bar samples x=0..320, y=12..44 normally; an alternate branch samples y=44..76. Both cover a 320×32 screen rectangle. |
| `bfont2.cel` | 230×39 | Ten horizontal digit cells, 23-pixel advance, 22×19 visible glyph rectangles. |
| `wicons.cel` | 224×25 | Eight 28-pixel cells. Current weapon cell samples x=`28*k+1`..`28*k+27`, y=1..24; screen x=106..132, bottom-origin y=5..28. |
| `hud-player-extra.png` | 182×52 | Seven 26×26 face cells and two rows. Overlay screen x=4..30, bottom-origin y=3..29. |
| `font256.cel` | 256×160 | Loaded separately at `0x14005e077`; not the large status numerals in this draw path. |
| `lfont2.cel` | 234×5 | Present in resources; no use established in the traced GPU status draw. |

HUD resource loads are at `0x14003328a` (weapon icons), `0x1400333fa`
(big font), `0x140033575` (status), and `0x1400336ed` (extra faces).
Stored texture pointers are HUD offsets `+0x88`, `+0x90`, `+0x98`, `+0xa0`.
Bar rectangle construction is `0x14003647e–0x140036510`; weapon icon construction
is `0x1400366b0–0x1400367f1`.

## Numerals

`0x140036ab0` formats a signed 16-bit value with `%d`, then maps each decimal
character to `digit = character - '0'`. The local atlas gives cell width
`230/10 = 23` and glyph height `(39-1)/2 = 19`. The visible source rectangle for
digit `d` is x=`23*d`..`23*d+22` and y=`row`..`row+19`.

The usual row is zero; if the value is less than the supplied threshold, the
row is 20. This low-value row selection is at `0x140036b52–0x140036b62`.
Glyphs advance by 23, leaving the authored one-pixel gap. Each digit starts at
`right_anchor - remaining_digit_count * 23*s` and occupies `22*s` pixels.
This means the final painted glyph edge is one scaled pixel before its anchor.
Bottom-origin glyph y is `floor((32-19)/2)*s = 6*s`, extending to `25*s`.

The normal status branch supplies:

| Value | Player-state source | Right anchor | Low-value threshold |
| --- | --- | --- | --- |
| Health | signed word `+0x28` | `x0 + 104*s` | 25 |
| Current weapon ammo | byte `+0x2e + weapon_index` | `x0 + 205*s` | 10 |
| Armor | byte `+0x27` | `x0 + 315*s` | 25 |

Health call: `0x1400367fc–0x140036810`. Ammo call:
`0x140036815–0x14003683f`, skipped for weapon index zero. Armor setup/call:
`0x140036844–0x1400368b1`. Health/armor semantic names agree with the observed
100-health/0-armor original capture; the memory accesses and anchors are direct
executable evidence. The helper does not contain a separate minus-sign atlas
mapping; clients should avoid assuming it safely renders arbitrary negatives.

## Face selection

With no special flags active, health greater than 80 uses the face already in
the status artwork. Otherwise extra-face columns are:

| Health | Extra-face column |
| --- | --- |
| 61..80 | 0 |
| 41..60 | 1 |
| 21..40 | 2 |
| 1..20 | 3 |
| <=0 | 4 |

Selection is at `0x140036515–0x14003657e`, and geometry at
`0x140036585–0x14003669d`. Positive state word `+0x50` or a global flag selects
column 5; otherwise positive state word `+0x4e` selects column 6. Their gameplay
meanings are not established here. The alternate bar flag selects the second
26-pixel face row. The meaning of this alternate flag is also not established;
do not label it as small-HUD/death/multiplayer solely from this draw routine.

## Transparency and color

The original `hud_frag` registration is at `0x14005231e`; local shader inspection
shows exactly: sample texture, discard if sampled alpha is below 0.5, output
sampled RGB and **half the sampled alpha**. No exposure, gamma or ACES operation
is present in this shader. The HUD constructor zeroes the pipeline descriptor at `0x1400355ec` and does
not change blend mode at descriptor offset `+0x94`. The shared builder reads
that mode at `0x14004eeea`; index zero in table `0x14004f20c` selects
`0x14004ef23`, with source ONE, destination ZERO and ADD for both RGB and
alpha. Surviving RGB therefore replaces the destination. Half-alpha in the
shader alone does not establish translucent source-over blending.

For palette fallback, status and big font use loader mode 1: palette indices
0 and 255 have alpha zero, other indices alpha 255. Weapon icons use mode 3:
index 0 has alpha 180, others 255. These modes are passed at `0x140033282`,
`0x1400333f2`, `0x14003356d`, and decoded by the original loader's switch at
`0x140163afc`. The loader tries an image override first, preserving its RGBA,
before palette fallback; see `0x1401638c5`. Do not apply palette transparency
rules to an already authored RGBA override. Shader sampling uses `texture()`;
the sampler's precise filter mode remains unverified in this note.

## Client proposal and limits

Independently implement the verified normal bar layout using the player's own
resources at runtime, after the world/weapon tone pass. Keep diagnostics behind
a developer toggle. At 640×400 the original scale algorithm produces a
640×64 bar; at 1920×1080 it produces 960×96. Preserve the integer glyph advance,
the three anchors, and source rows rather than replacing the digits with the
general debug font.

Use the ordinary >80-health built-in face initially; add verified damage columns
when that separate PNG is available. Missing optional extra faces can fall back
to the built-in face. Keep special-state/alternate layout behavior explicit and
provisional until its state semantics are traced. The client implements the normal bar, health-dependent damage face, current
weapon icon and numeric anchors, with alpha-tested RGB replacement. Nearest
sampling remains provisional pending sampler confirmation. No matched runtime pixel comparison was performed in
this research, and no original game or configuration was changed.
