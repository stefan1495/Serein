# Voice overlay

On KDE Plasma Wayland, open **Settings → Voice Overlay**. Turn on **Enable voice overlay**,
select a **Display**, then **Save**. The picker includes connector names to distinguish identical
monitors. **Identify** flashes a red border and label on the selected display for three seconds;
it works even when the live overlay is disabled and does not change your settings.

**Edit placement and appearance** shows a red border and two sample speakers on the selected
monitor. Drag by **Drag to move**; clicks outside the handle pass through. The preview does not
take keyboard focus and needs neither a call nor a microphone. Use the display picker to move it
to another monitor without changing its relative position.

Adjust avatar visibility and size, installed font family, text size and weight, text color,
background color and opacity, and the speaking accent color. With avatars hidden, the accent is
shown as a small bar. Missing fonts use a system fallback. These controls update the sample
speakers immediately; they do not apply Discord profile name styles. **Reset appearance** restores
the default appearance without moving the overlay, and **Reset position** restores the top-left
inset without changing the appearance.

**Save** keeps the display, position and appearance together. **Cancel**, leaving the page or
closing Settings discards the draft. If the display disconnects while editing, the preview closes;
select a connected display before saving.

In a call, the overlay shows up to eight current speakers with server nicknames and optional avatars. It
does not take focus or intercept clicks, and it has no border or handle. It disappears during
silence, when the call ends, or while the placement preview is open. Disabling it keeps the saved
display, position and appearance. If the saved display is disconnected, the overlay is hidden rather than
redirected, and Settings reports the disconnected display.

Serein renders the overlay locally using Wayland layer-shell. Placement editing also uses
subsurfaces and viewporter. There is no voice RPC endpoint, external overlay client, new sign-in,
or direct audio access. Native KDE Plasma Wayland is the target; other compositors, X11,
Windows and macOS are outside this implementation.
