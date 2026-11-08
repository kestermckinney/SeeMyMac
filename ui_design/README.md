# Remote Desktop Client — Design Handoff

## 1. Purpose

This package contains the UI design for a minimal-chrome remote desktop client. The receiving agent will use it as the reference for building the client. The design gives the remote screen as much of the window as possible. All connection management sits in one menu, one switcher overlay and one dialog.

## 2. Package Contents

| File | Description |
|---|---|
| `README.md` | This specification. Describes layout, states, behavior, data model and visual tokens. |
| `project/canvas.json` | Canvas index. Lists each artboard, its position and its title. |
| `project/Main.dc.html` | The complete interactive prototype. Contains all markup, state and behavior. |
| `project/Focus.dc.html` | State frame: windowed, toolbar unpinned. |
| `project/Menu.dc.html` | State frame: Connections menu open. |
| `project/Switcher.dc.html` | State frame: session switcher overlay. |
| `project/Dialog.dc.html` | State frame: Edit Connection dialog. |
| `project/Monitors.dc.html` | State frame: two remote monitors, zoomed out to show both. |
| `project/FullScreen.dc.html` | State frame: full screen, no window frame. |

**Note on file format:** The `.dc.html` files are Design Component files. They depend on a `support.js` runtime that is not included, so they will not render when opened directly in a browser. The receiving agent must treat them as readable source: HTML markup with inline styles, `{{ }}` template holes, `<sc-if>` and `<sc-for>` blocks, and a JavaScript `Component` class that holds state and handlers. Every state frame mounts `Main.dc.html` with a different starting prop, so `Main.dc.html` is the single source of truth.

## 3. Design Principles

- The remote screen must occupy all space not used by the toolbar. The toolbar is 40px high when pinned and collapses to a 24px pull-tab when unpinned.
- One menu must handle quick connect, active sessions, favorites, recent connections and access to create or edit.
- Switching between active sessions must take one click or one shortcut.
- Chrome must stay dark and low in contrast against the remote screen so it does not compete with remote content.
- Status must never rely on color alone. Dots differ in lightness and carry a text label.

## 4. Window Layout

| Region | Height | Visible when | Contents |
|---|---|---|---|
| Window title bar | 32px | Windowed mode only | App icon, "Session name — host — Remote Desktop", Minimize, Maximize, Close |
| Toolbar | 40px | Toolbar pinned | Connections menu button, session tabs, New connection (+), monitor selector, resolution label, action buttons |
| Pull-tab | 24px, overlaid | Toolbar unpinned | Status dot, session name, monitor indicator, "+N more" count. A click restores the toolbar. |
| Remote screen | Remaining height | Always | The remote desktop, scaled to fit |

### 4.1 Toolbar Actions (left to right, right-hand group)

| Control | Behavior | Shortcut |
|---|---|---|
| Monitor selector (1 / 2 / Both) | Shown only when the active session has more than one remote monitor | Ctrl+Alt+PgUp / PgDn / Home |
| Resolution label | Read-only, e.g. "1920 × 1080 · Fit" | — |
| Switch session | Opens the switcher overlay | Ctrl+Alt+Tab |
| Send Ctrl+Alt+Del | Sends the key sequence to the remote | — |
| Scale to fit | Scaling mode | — |
| Pin / Unpin toolbar | Toggles auto-hide | — |
| Full screen / Exit full screen | Toggles window mode. Shows a text label in full screen. | Ctrl+Alt+Enter |

## 5. States

| State | Trigger | Description |
|---|---|---|
| Session view | Default | Toolbar pinned, active session shown |
| Focus | Unpin toolbar | Toolbar replaced by the pull-tab |
| Connections menu | Connections button | 380px drop-down anchored under the button. A click outside closes it. |
| Switcher | Switch button or Ctrl+Alt+Tab | Full-window scrim with one 244px thumbnail card per active session, plus a "New connection" card |
| Connection dialog | + button, "New connection…", or the pencil on any favorite or recent item | 640px modal with tabs General, Display, Local resources |
| Both monitors | Monitor selector "Both" | Remote monitors shown side by side at 16:9 on a dark letterbox, each labelled. A click on a monitor zooms back into it. |
| Full screen | Full screen button | Title bar removed. The toolbar unpins automatically on entry. |
| No sessions | Last session closed | Centered message with an "Open connections" button |

## 6. Connections Menu Structure

1. **Quick connect field:** searches favorites or accepts a host name. Enter connects.
2. **Active (N):** one row per open session, with status dot, status text and a Ctrl+Alt+1…9 shortcut. The current session is highlighted.
3. **Favorites:** star icon, name, "host · protocol", an "Open" badge when already connected, and an Edit (pencil) button.
4. **Recent:** clock icon, name, host, and an Edit button.
5. **Actions:** New connection… (Ctrl+N), Manage all connections… (Ctrl+M), Settings.

Selecting a favorite that is already open must switch to it, not open a duplicate.

## 7. Connection Dialog Fields

| Tab | Field | Control | Default |
|---|---|---|---|
| General | Display name | Text | — |
| General | Protocol | Segmented (RDP / VNC) | RDP |
| General | Host name or IP address | Text | — |
| General | Port | Numeric text | 3389 (RDP), 5900 (VNC) |
| General | User name | Text | — |
| General | Credentials | Select: Ask when connecting / Save in credential vault / Use my Windows sign-in | Ask when connecting |
| General | Group | Select: Servers / Plant floor / Lab / No group | Servers |
| General | Gateway (optional) | Text | [GATEWAY HOST] |
| General | Show in Favorites | Checkbox | On |
| General | Reconnect automatically if dropped | Checkbox | On |
| Display | Remote resolution | Select: Match client window / Match local display / 1920 × 1080 / 1280 × 720 | Match client window |
| Display | Scaling | Select: Fit to window / 100% / Fill | Fit to window |
| Display | Color depth | Select: 32-bit / 16-bit | 32-bit |
| Display | Start in | Select: Window, toolbar unpinned / Window, toolbar pinned / Full screen | Window, toolbar unpinned |
| Display | When the remote PC has more monitors than this PC | Select: One at a time / All side by side / Primary only | One at a time |
| Display | Span all local monitors | Checkbox | Off |
| Local resources | Clipboard, Local drives, Printers, Audio playback, Windows key combinations, Smart cards | Checkboxes | Clipboard, Audio and Key combinations on |

Footer buttons: Delete (edit mode only, left), Cancel, Save, **Save & connect** (primary).

## 8. Data Model

Each connection holds the attributes below. Session state, the per-session monitor selection, and the window/full-screen flag are kept separately from the connection record.

| Attribute | Type | Notes |
|---|---|---|
| `id` | string | Unique |
| `name` | string | Display name |
| `host` | string | Host name or IP |
| `proto` | `RDP` \| `VNC` | |
| `port` | string | |
| `user` | string | Optional for VNC |
| `group` | string | |
| `fav` | boolean | Favorites vs. Recent |
| `status` | `ok` \| `retry` | Connected / Reconnecting |
| `monitors` | number | Count of remote monitors (default 1) |

Runtime state: `open` (ordered list of session ids), `active` (id), `mon` (map of session id to `1` / `2` / `all`), `pinned`, `full`, `view` (`idle` / `menu` / `switcher` / `dialog`).

The sample connection names and hosts in the prototype are placeholders.

## 9. Visual Tokens

| Token | Value | Use |
|---|---|---|
| Font | Inter 400/500/600, 13px base | All UI |
| Background, chrome | `#0D1117` | Toolbar, inputs |
| Background, title bar | `#010409` | Window title bar |
| Surface | `#161B22` | Menu, dialog, switcher |
| Surface, selected | `#1F2630` | Active tab, selected rows |
| Border | `#30363D` / `#262C36` | Panels / dividers |
| Text | `#E6EDF3` | Primary |
| Text, muted | `#9198A1` | Secondary, labels |
| Accent | `#2F6FEB` | Primary buttons, selected segment |
| Accent, on dark | `#6CA4FF` | Active tab underline, focus ring |
| Status, connected | `#3FB950` | Dot plus "Connected" |
| Status, reconnecting | `#D29922` | Dot plus "Reconnecting…" |
| Danger | `#F47067` | Delete |
| Radii | 6px controls, 10px menu, 12px dialog | |
| Hit size | 32px toolbar buttons, 36–44px menu rows | |

## 10. Open Questions

- Should the unpinned toolbar also reveal itself when the pointer touches the top edge, or only when the tab is clicked?
- Is SSH or any other protocol in scope?
- Should favorites be grouped into folders in the menu?
