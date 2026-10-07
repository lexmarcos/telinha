# Telinha native design system

The native app is the same brand as the website in a smaller format: a **pocket screen**.
A round bubble floats over whatever the person is doing (a game, a
spreadsheet), shows the channel and who is watching, and hides in the tray when it
is not needed. Everything other than the bubble only appears when someone asks for it.

This document is the source of the decisions. The interface is built in Slint and mirrors
each item in `ui/tokens.slint` (values) and in the components in `ui/` (pieces); the
springs, which the app animates frame by frame, live in `src/ui/motion.rs`. No
screen defines its own color, size, radius or animation timing: if a value is
missing, it is added here first.

## Principles

1. **The bubble is the product.** It is the only thing always visible, so it is where the
   personality lives: a round TV tube with the channel number in a dot matrix
   and the red "on air" light. The rest (menu, panel) is quiet and functional.
2. **Do not get in the way of what is underneath.** The window is only as big as its content,
   with no borders and no background. Nothing blinks or moves on its own, except the "on air" light
   when the broadcast starts (a single moment).
3. **Everything is born from the bubble and returns to it.** The menu and quality panel open
   anchored to the bubble, growing out of it, and close the same way.
4. **Respond on press.** Buttons and the bubble react on press, not on release.
   Animations use interruptible springs: a click in the middle of an animation starts
   from where the thing currently is, never jumps.
5. **Words say what happens.** "Transmitir a tela", "Parar de
   transmitir", "Copiar convite". Short phrases, verb first, no all caps.

## Colors

| Token | Value | Use |
|---|---|---|
| `room` | `#0D1430` | Panel background (the website's dark room) |
| `room_raised` | `#16204A` | Focused/hovered item, segmented control track |
| `screen` | `#070B1D` | Bubble tube glass |
| `phosphor` | `#CFE0FF` | Channel digits, selected thumb, focus |
| `text` | `#EDF0FF` | Main text |
| `muted` | `#8C95BD` | Supporting text, captions |
| `line` | `#C8D4FF` at 14% | Thin outlines |
| `tally` | `#FF4438` | Only "on air": bubble ring, stop broadcasting |
| `warn` | `#FFC56B` | Warning (bad connection, software encoder) |

Viewer avatars use the same palette as the website, picked by name:
`#CFE0FF #FFD9A8 #BFF0D4 #F5C6FF #FFE48A #A8ECFF #FFC2C2`.

Red is reserved: it appears only while the screen is being broadcast
(ring, recording dot, stop button). If everything is red, nothing is.

## Typography

- **Interface:** the system font (no embedded font), to feel at home on
  Windows and Linux.
- **Channel digits:** **Doto** (dot matrix, the same as the website), embedded.
  Only for channel digits; never for text.

| Token | Size | Weight | Use |
|---|---|---|---|
| `title` | 15 | semibold | Panel title |
| `body` | 14 | regular / semibold in items | Menu items, buttons |
| `label` | 12 | semibold | Group name ("Resolução") |
| `note` | 12 | regular | Short explanations, in `muted` |
| `digits` | 17 in the bubble, 30 in the channel field | Doto black | Channel number |

Labels in sentence case, never all caps.

## Space, size and shape

- 4-point grid: `xs 4`, `s 8`, `m 12`, `l 16`, `xl 24`.
- **Bubble:** 64 diameter; inner tube 52; "on air" ring of 3.
- **Viewer dots:** 18 diameter, overlapping by 5, at most 5 and
  then `+n`. They sit in a centered row below the bubble.
- **Panel:** width 264, radius 20, inner padding 14. Sits 10 away from the
  bubble, aligned to its top.
- **Menu item:** height 36, radius 10, 16 icon on the left.
- **Segmented control:** height 32, radius 11 on the track and 9 on the thumb.
- **Button:** height 36, pill shape.

Radii follow the hierarchy: large surface (panel 20) > control (11) >
thumb (9). Never a single radius for everything.

## Material and depth

The window does not blur what is behind it, so the website's "glass" becomes a
solid dark material with three layers:

1. `room` background at 96% opacity, 1-wide `line` outline.
2. Light hitting the top edge: a 1-wide line that is bright in the middle and fades at the
   ends (white up to 16%).
3. A wide outer shadow, made of seven increasingly larger and fainter rings
   (`ui/shadow.slint`), because the software renderer has no `drop-shadow`.
   Panel: 16 reach, 8 down, 60% combined. Bubble: 10, 4 and 50%.

Larger surfaces have larger shadows (panel > menu > bubble). Never stack
translucent material on top of translucent material.

## Motion

Springs, not durations. Apple-style parameters (damping, response):

| Token | Damping | Response | Use |
|---|---|---|---|
| `spring_ui` | 1.0 | 0.32 s | Panel opening and closing, segmented thumb |
| `spring_press` | 1.0 | 0.12 s | Shrinking on press (0.94) and back |
| `spring_tally` | 0.7 | 0.45 s | The red ring arriving when the broadcast starts |
| `spring_fade` | 1.0 | 0.22 s | New content appearing when the panel changes |

The springs run in the app (`src/ui/spring.rs`), not in Slint's curves: a spring
that changes target midway continues from its current position **and velocity**,
so opening and closing quickly never jumps or "hits a wall". Slint's curves
are only for micro-responses (`press` 90 ms, `hover` 120 ms).

- The panel **is born from the bubble**: scales from 0.92 to 1 with the origin on the bubble's side,
  and opacity from 0 to 1. It closes the same way.
- Every animation starts from the current value: opening and closing quickly does not jump.
- **Panel switch** (menu → quality): the surface height follows
  `spring_ui` to the new panel's height, and the new content appears with `spring_fade`.
  The window reserves the larger of the two heights during the switch and only shrinks at the
  end, so it resizes twice instead of every frame.
- The panel measures itself (Slint's layout gives the natural height); nothing is
  measured "by eye" or cached.
- `Esc` closes the panel.
- **Reduced motion** (system setting): no scaling or bounce, only a short
  opacity change.

## Components

Each component lives in `ui/*.slint` and only reads values from `ui/tokens.slint`. State
comes from the app through `AppState` (`ui/state.slint`).

### Bubble (`bubble`)
Round tube with the channel number in Doto. States:
- **No channel:** a subtle "+" on the glass, inviting a click.
- **Connecting:** a `phosphor` arc spinning slowly around it (the app's only spinner).
- **In channel:** channel number lit in `phosphor`.
- **On air:** 3-wide `tally` ring + recording dot at the top right.
- **Hover:** lighter outline. **Press:** shrinks to 0.94.
Click opens or closes the menu. Dragging moves the window through the window manager
(follows the mouse 1:1); the drag only starts after 4 of movement, so it is not
confused with a click.

### Viewer dots (`dots`)
18-wide circles with initials, avatar color picked by name. Someone on a browser
that does not receive native mode appears dimmed (40%). The names appear in the menu's
supporting line when there are up to three ("Canal 4821. Ana e Bia assistindo"); with
more people, the count.

### Panel (`panel`)
Container with the material. Optional header with a title and, in subpanels, the
back arrow on the left (always in the same place).

### Menu item (`menu_item`)
Icon + text, full width. Variants: `normal`, `primary` (text in
`phosphor`) and `danger` (text in `tally`, only for stopping a broadcast). Hover paints
`room_raised`.

### Segmented control (`segmented`)
`room_raised` track with a thumb that slides with `spring_ui` to the chosen
option. The thumb has the same top light as the material, in miniature.

### Buttons (`button`)
- `primary`: `text` background, `room` text. One primary action per panel.
- `quiet`: `line` background, `text` text.
- `danger`: `tally` background, white text.
On press it shrinks to 0.97 immediately; on hover it brightens slightly.

### Channel field (`channel_field`)
The four digits in Doto 30 on `screen`, like the small screen on the website's home
page. Also accepts a pasted invite link.

### Note (`note`)
`note` text in `muted`; as a warning, icon and text in `warn`.

## Copy

UI copy stays in Brazilian Portuguese; the examples below are the actual strings.

- Verb first, and the same word as the button until confirmation: "Transmitir a
  tela" → "Transmitindo"; "Copiar convite" → "Convite copiado".
- Errors say what happened and what to do: "O canal 4821 não está no ar. Confira o
  número com quem te convidou."
- No decorative ellipses, no exclamation marks, no emoji.
