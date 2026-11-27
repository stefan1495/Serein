# Overlay

On KDE Plasma Wayland, open **Settings → Overlay**. The voice roster and message notifications
have independent switches and share one display. The picker includes connector names to distinguish
identical monitors. **Identify** flashes a red border and label on the selected display for three
seconds without changing preferences.

## Placement

**Edit Overlay** puts a fictional voice roster and notification on the selected monitor, even if
both live features are disabled. The editor uses one red border, does not take keyboard focus and
lets clicks outside the two handles pass through. Drag either handle, or focus the corresponding
move buttons in Settings. Each panel has its own reset. Use the picker to explicitly change
monitors. The voice and notification footprints reserve room for the full roster and toast, so
panels cannot be saved on top of each other even if the preview text does not touch.

**Save** applies the shared display and both positions together, then writes the change to this
device in the background. A failed write leaves the change active for this session and offers
**Retry save** when storage is available. Serein never overwrites preferences it could not load.
**Cancel**, leaving the page or closing Settings discards the draft. Output loss or a geometry change
invalidates an active editor rather than sending the overlays to another monitor. A display may
support live surfaces but not the combined editor or full footprints; it can still be selected for
live use without changing saved positions. Identify and the editor require Wayland subsurfaces
and viewporter in addition to layer-shell.

## Voice roster

**Speaking only** is the default. It shows up to eight current speakers with server nicknames and
optional avatars, then hides during silence. **Everyone in my active call** keeps the roster on
screen even without an activity sample. Your row comes first, followed by stable participant order;
when the roster will not fit, the last whole row reports how many participants were omitted.
The same microphone-slash, headphones-slash and speaker icons as in the call UI mark mute, deafen
and **Muted for you**. Deafen takes precedence over mic mute; local mute is independent. A missing
speaking indicator is not proof of silence.

**Appearance** controls avatars, installed font, text size and weight, text and background colors,
opacity and speaking accent. Resetting appearance does not move either panel. Missing fonts use a
system fallback. The live roster is click-through, has no handle and hides when the call ends or
its display disconnects. It pauses if Discord moves you without a fresh voice session or if voice
is securing or loses access. Leave and rejoin if necessary to start a fresh voice session.

## Notifications

Live notifications and text excerpts are off by default. The live overlay shows eligible alerts
only when KDE reports a foreground window that Serein can attribute to a recognized game. Detection
uses local registered games or an existing local game catalog. It does not start **Share Game
Activity**, publish presence, start RPC, or download a game list. Running a game in the background,
full-screen status or an unfocused Serein window is not proof. If focus is unknown, stale, or the
program cannot be identified, no custom toast appears. KDE may withhold its foreground protocol
or the executable may be inaccessible, for example across sandbox or PID namespace boundaries.
Serein does not request extra compositor permissions. Changing away from a game clears the toast
without replay. New alerts use system notifications only if enabled when the overlay cannot admit
them. Notification sounds remain independent.

A toast replaces the previous one and lasts at most four seconds. Both live panels use their saved
positions. Legacy settings retain the selected toast corner and inset until the paired editor is
saved. If a live roster and toast would overlap, the toast is omitted instead of being moved.
Neither Identify nor the editor displays real message content.

Live toasts show sender and conversation labels. Opt-in excerpts use only eligible plain source
text and omit protected or uncertain text such as spoilers, markup, and attachment or system
summaries. This does not change the existing system notification preview. **Screen sharing,
screenshots and recordings may show the overlays**, including any text they display. There is no
capture exclusion claim.

This implementation targets native KDE Plasma Wayland, not other compositors, X11, Windows or macOS.
Synthetic KWin checks covered two outputs, independent placement commands, overlap rejection, a tiny
output, idle toast expiry, and foreground window acquisition and exit. They do not establish
packaged foreground reporting, pointer pass-through and drag, fractional scaling, or hotplug.
