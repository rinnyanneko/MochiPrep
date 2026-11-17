# The desktop launcher

MochiPrep runs as a tray app. It starts the backend in the background and opens the interface in its own window.

## Where things live

Your data is kept in these folders. The bundle identifier is `io.github.rinnyanneko.mochiprep`.

| What | Windows | macOS | Linux |
|---|---|---|---|
| Launcher settings (`launcher.json`) | `%APPDATA%\io.github.rinnyanneko.mochiprep` | `~/Library/Application Support/io.github.rinnyanneko.mochiprep` | `~/.config/io.github.rinnyanneko.mochiprep` |
| Database (`mochiprep.db`) | `%APPDATA%\io.github.rinnyanneko.mochiprep` | `~/Library/Application Support/io.github.rinnyanneko.mochiprep` | `~/.local/share/io.github.rinnyanneko.mochiprep` |

The interface's own saved preferences (such as its theme) are kept separately. On Windows, they are in `%LOCALAPPDATA%\io.github.rinnyanneko.mochiprep\EBWebView`.

Changing the port starts those saved preferences fresh, because they belong to the old address.

## How it behaves

- The backend does not start until you accept the current risk warning. If you decline, or close the warning, MochiPrep quits.
- On first start with an empty data folder, MochiPrep offers once to import an existing data folder, such as one from an earlier version. The original folder is not changed.
- The backend uses port 8787 by default. You can change it in **Settings** (1024 to 65535). If the port is busy, the tray shows `Error: port <p> is in use`. MochiPrep does not pick another port by itself.
- Closing the interface window does not stop the backend. Agents keep running in the tray.
- **Quit** and **Restart** ask `Stop MochiPrep's backend?` first if an agent is running, a real account is armed, or the backend's state cannot be read.
- **Disarm all** is always safe and is available while a real account is armed.
- On macOS there is no Dock icon. Quit is only in the tray menu, and it asks first.
- On Linux without a tray (for example GNOME without the AppIndicator extension), a small panel window with the same items replaces the tray icon.
- Quitting the launcher stops the backend cleanly.

## Updates

Release builds check for a new version when they start and then every 24 hours. Turn this off with **Check for updates automatically** in Settings.

A check only looks for a new version. Nothing is downloaded or installed until you click the update item, which appears only when a newer release exists.

- **Windows, the macOS app and the Linux AppImage** update themselves. The item reads `Update to v<version>`. Clicking it downloads the update while the backend keeps running, then asks before stopping the backend, installs the update and relaunches. A failed download stops nothing. A failed install starts the backend again.
- **A `.deb` or a distribution package** updates through your package manager. The item reads `v<version> available` and opens the release page.

Updates are signed, and an update without the right signature is refused.
