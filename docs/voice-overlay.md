# Voice overlay

On KDE Plasma Wayland, open **Settings → Voice & Video → Voice overlay** and choose a monitor. The overlay appears at the top left and shows up to eight current speakers in your active call, with their avatars and server nicknames. It disappears when nobody is speaking. It does not take focus or intercept clicks. Choose **Off** to disable it. The monitor choice is device-local and is not redirected to another display if the selected one is unplugged.

This is rendered by Serein. There is no voice RPC endpoint, external overlay client, new sign-in, or direct audio access. The first target is KDE Wayland with layer-shell support; X11, Windows, macOS, and Wayland compositors without layer-shell support are outside this implementation.
