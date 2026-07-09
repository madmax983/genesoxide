# VDP Shadow / Highlight Mode

Genesoxide implements the Mega Drive VDP shadow/highlight (S/H) feature in
`crates/genesoxide-core/src/vdp.rs`. This note documents the model as
implemented.

## Enable gate

S/H is controlled by **register 0x0C, bit 3** (`0x08`). At the top of
`render_scanline` we compute:

```rust
let sh = self.registers[0x0C] & 0x08 != 0;
```

When `sh` is false the renderer is byte-for-byte identical to a build without
S/H support: operator sprites are treated as ordinary palette-3 colors 14/15,
and the framebuffer writeback is a plain copy. All S/H work is gated behind this
flag, so the disabled path carries no per-pixel overhead beyond the single
branch.

## Per-pixel intensity model

Each output pixel resolves to one of three intensities:

| Value | Intensity |
|-------|-----------|
| 0     | Shadow    |
| 1     | Normal    |
| 2     | Highlight |

### Base intensity from priority

The base intensity is derived from the *winning* background/plane/sprite pixel's
priority level (`pixel_priority[x]`) **after** all plane and sprite compositing:

- `pixel_priority[x] == 2` (a high-priority plane or sprite pixel) -> **Normal**
- otherwise (priority 0 = backdrop/background-fill, or 1 = low-priority
  plane/sprite) -> **Shadow**

Consequently the **backdrop is shadowed**, and any low-priority plane or sprite
pixel is shadowed, matching hardware where the low-priority layer is dimmed
unless a high-priority element or a highlight operator lifts it.

## Operator sprites

When `sh` is true, a sprite pixel is an **operator** when its palette is 3 and
its color index is 14 or 15:

- color index **15** -> **shadow operator**
- color index **14** -> **highlight operator**

Operator sprites do **not** draw color and do **not** set `pixel_priority` or
`pixel_has_sprite`. Instead they record a modifier into a per-scanline
`sh_op: [u8; 320]` buffer (0 = none, 1 = shadow op, 2 = highlight op).

### Front-of-color rule

Sprites are processed front-to-back (link-list order). A drawn color sprite sets
`pixel_has_sprite[xi] = true`. An operator is recorded only if

```rust
!pixel_has_sprite[xi] && sh_op[xi] == 0
```

i.e. no color sprite has been drawn in front of it yet, and no nearer operator
was already recorded. The operator therefore applies to a color sprite (or
plane) drawn *behind* it and is ignored when a color sprite already covers that
pixel in front. After recording, the operator pixel is skipped (`continue`).

When `sh` is false these same pixels draw as ordinary palette-3 colors 14/15,
with no special-casing.

## Applying the operator

After the sprite pass, for each pixel the recorded operator is folded into the
base intensity:

| base       | operator  | result    |
|------------|-----------|-----------|
| any        | none      | base      |
| Shadow     | Highlight | Normal    |
| Normal     | Highlight | Highlight |
| Highlight  | Highlight | Highlight |
| Highlight  | Shadow    | Normal    |
| Normal     | Shadow    | Shadow    |
| Shadow     | Shadow    | Shadow    |

(In practice the base is only ever Shadow or Normal, since Highlight is produced
only by a highlight operator; the Highlight-base rows are included for
completeness.)

## Intensity -> RGBA math

The stored `pixel_color` is already Normal 8-bit RGBA. Because
`Shadow = Normal / 2` and `Highlight = 128 + Normal / 2` are both **linear** in
the Normal 8-bit value, the transform is applied directly to the stored RGBA at
writeback rather than re-derived from CRAM (`apply_intensity`):

```
Shadow    (0): channel -> channel >> 1
Normal    (1): channel unchanged
Highlight (2): channel -> 128 + (channel >> 1)
```

Only the RGB channels are transformed; alpha is preserved. `128 + (x >> 1)` with
`x <= 255` peaks at 255, so there is no overflow.

## Known deviations

- **Frontmost-operator approximation.** Operators use a single frontmost-sprite
  model: exactly one operator modifier is retained per pixel (the nearest one),
  recorded via the `!pixel_has_sprite && sh_op == 0` condition. Stacked
  multi-sprite interactions where more than one operator overlaps the same pixel
  are approximated to the frontmost operator rather than combined.

## References

- Charles MacDonald, *Sega Genesis VDP documentation* (`vdp.txt`) — shadow/
  highlight operation and operator-sprite (palette 3, colors 14/15) semantics.
- Plutiedev, *Shadow & Highlight* article
  (https://plutiedev.com/shadow-highlight) — base-intensity-from-priority rule
  and operator color/brightness behavior.
- *Sega Genesis Software Manual* — VDP register 0x0C bit 3 (S/H enable) and
  color-intensity levels.
