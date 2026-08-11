# aceitw/herdr fork

This repository tracks [upstream Herdr](https://github.com/ogulcancelik/herdr)
and adds a small set of navigation and sidebar features. Upstream documentation
still applies unless this file says otherwise.

## Install this fork

The installer in this repository uses the fork release manifest:

```bash
curl -fsSL https://raw.githubusercontent.com/aceitw/herdr/master/website/install.sh | sh
```

Fork binaries and release notes are available from the
[`aceitw/herdr` releases](https://github.com/aceitw/herdr/releases). The
installer and package-manager links on [herdr.dev](https://herdr.dev) install
upstream Herdr instead.

## Swap workspaces and tabs

The fork adds four optional keybindings:

| Setting | Action |
| --- | --- |
| `keys.swap_previous_workspace` | Move the active workspace one position earlier in the sidebar. |
| `keys.swap_next_workspace` | Move the active workspace one position later in the sidebar. |
| `keys.swap_previous_tab` | Move the active tab one position left in the tab bar. |
| `keys.swap_next_tab` | Move the active tab one position right in the tab bar. |

Swapping moves the active workspace or tab without changing its identity or
focused pane. Agent entries may move in the sidebar because their workspace or
tab order changed, but focus remains on the same agent.

All four bindings are unset by default. One example configuration is:

```toml
[keys]
swap_previous_workspace = "prefix+ctrl+,"
swap_next_workspace = "prefix+ctrl+."
swap_previous_tab = "prefix+,"
swap_next_tab = "prefix+."
swap_wrap = true
```

`keys.swap_wrap` defaults to `true`. Moving backward from the first position
wraps to the last position, and moving forward from the last position wraps to
the first. Set it to `false` to stop at either boundary.

## Resize and customize the sidebar

The fork adds optional actions for changing the expanded sidebar width:

```toml
[keys]
widen_sidebar = "prefix+ctrl+b"
narrow_sidebar = "prefix+ctrl+v"

[ui]
sidebar_min_width = 18
sidebar_max_width = 36
sidebar_resize_step = 2
```

Each action changes the width by `ui.sidebar_resize_step` columns and clamps it
to `ui.sidebar_min_width` and `ui.sidebar_max_width`. The actions do nothing
while the sidebar is collapsed.

The sidebar header can also be customized:

```toml
[ui]
sidebar_header_bg = "#fab387"
sidebar_header_title = " spaces"
```

`ui.sidebar_header_bg` accepts `#rrggbb`, named colors, and `rgb(r,g,b)`.
`ui.sidebar_header_title` replaces the default ` spaces` label; a leading space
can be used as padding.

Run `herdr config check` after changing the configuration. Press `prefix+?` in
Herdr to see the effective bindings.

## MOSH and Ctrl-punctuation bindings

MOSH does not transparently carry enhanced keyboard-protocol negotiation. In
the legacy terminal encoding used through MOSH, `Ctrl+,` and `Ctrl+.` are
indistinguishable from plain `,` and `.`. With the example above, MOSH can
therefore dispatch `prefix+ctrl+,` as `prefix+,` and
`prefix+ctrl+.` as `prefix+.`. Normal SSH does not have this problem when the
outer terminal and Herdr negotiate enhanced keyboard reporting.

There is no MOSH option that restores the lost modifier. Either choose bindings
that remain distinct in legacy terminal input, or configure the outer terminal
to send explicit CSI-u sequences.

### iTerm2 workaround

In **Settings -> Profiles -> Keys -> Key Mappings**, add these mappings using
the **Send Escape Sequence** action:

| Shortcut | Value |
| --- | --- |
| `Ctrl+,` | `[44;5u` |
| `Ctrl+.` | `[46;5u` |

iTerm2 adds the leading Escape byte. Prefer putting the mappings in a profile
used for MOSH so they do not change those chords for unrelated terminal
applications. Herdr recognizes the resulting sequences as `Ctrl+,` and
`Ctrl+.` respectively.

An alternative is to avoid Ctrl-punctuation entirely, for example:

```toml
[keys]
swap_previous_workspace = "prefix+alt+up"
swap_next_workspace = "prefix+alt+down"
swap_previous_tab = "prefix+alt+left"
swap_next_tab = "prefix+alt+right"
```

Outer-terminal and operating-system mappings can still claim modified arrows,
so verify the chosen chords in the terminal used to start MOSH.
