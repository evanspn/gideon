# Installing gideon on a Kobo

## What you need

- A Kobo e-reader (armv7 — Clara, Libra, Sage, Forma, Aura, etc.)
- The `gideon-kobo-bundle` zip from the latest build (post-merge CI artifact,
  or a GitHub release once those exist)
- [NickelMenu](https://pgaskin.net/NickelMenu/) — required to *launch*
  gideon from the Kobo home screen. Installing gideon works without it, but
  you'd have no way to start the app except over SSH/telnet. Install
  NickelMenu first (drop its `KoboRoot.tgz` into the device's `.kobo`
  folder and eject), then run gideon's installer — it detects NickelMenu
  and adds the menu entry automatically. If you install NickelMenu later,
  just re-run gideon's installer.

## Install / upgrade

1. Plug the Kobo into your computer over USB and let it mount.
2. Unzip the bundle and run:

   ```sh
   ./install.sh
   ```

   The installer auto-detects the mounted Kobo. If detection fails, point it
   at the mount: `./install.sh --root /media/$USER/KOBOeReader`.

3. Eject safely and unplug.

Running the installer again later **upgrades in place**. Your data is safe:

- `.adds/gideon/data/` (settings, app state) is **never** written, modified
  or deleted by the installer.
- Before each upgrade your data directory is archived to
  `.adds/gideon/backups/` (the 3 most recent backups are kept).
- Reading progress stored next to your manga library (`.gideon/` folders) is
  never touched.

## On-device install (SSH/telnet)

Copy the bundle to the device, then:

```sh
sh install.sh --root /mnt/onboard
```

## Updating over the air

There is a single **gideon** entry in the NickelMenu — it opens the app.
Everything, including updates, lives inside it:

**Home → Check for updates** — if a release is newer, tap once more to
install it (wifi on). The binary swap is atomic and the previous version
is kept as `gideon.old` for manual rollback. Close and reopen gideon to
run the new version. If anything goes wrong, the app log is at
`.adds/gideon/browse.log`.

## Leaving gideon

Closing gideon (power menu → **Close gideon**) restarts the stock Kobo
home screen **in place** — Nickel is back within a few seconds, no device
reboot. If Nickel fails to reappear within ~10 seconds, the launcher falls
back to a full reboot, which always recovers the device.

## Uninstall

```sh
./install.sh --uninstall           # removes the app, KEEPS your data
./install.sh --uninstall --purge   # removes everything including data
```

## Layout on the device

```
.adds/gideon/
  bin/gideon     # the app — replaced on every upgrade
  VERSION
  data/          # settings + state — never touched by the installer
  backups/       # automatic pre-upgrade archives of data/
.adds/nm/gideon  # NickelMenu launcher entry (only if NickelMenu is present)
```

## Known issues

### The gideon menu entry disappears from Home after a Kobo firmware update

Kobo firmware updates reset the device's system partition, which is where
NickelMenu's own hook into Nickel lives. Your `.adds/` folder (everything
under it, including `.adds/gideon/` and `.adds/nm/gideon`) is on the FAT32
user partition and is untouched by a firmware update — so your reading
progress and settings are never at risk here. What's gone is just
NickelMenu's ability to read `.adds/nm/gideon` and add the entry to Home.

**Fix: reinstall NickelMenu, not gideon.**

1. Plug the Kobo into your computer and let it mount as `KOBOeReader`.
2. Download the latest `KoboRoot.tgz` from
   <https://github.com/pgaskin/NickelMenu/releases/latest/download/KoboRoot.tgz>.
3. Copy it into the device's `.kobo` folder (enable hidden files in Finder/
   Explorer if you don't see `.kobo`), overwriting any existing
   `KoboRoot.tgz` there.
4. Eject safely and unplug. The Kobo applies the update and reboots on its
   own — this only patches the system side, it never touches `.adds/`.
5. Check Home for the **gideon** entry. It should reappear immediately,
   since `.adds/nm/gideon` was never removed.

If it's still missing after that, re-run gideon's own `install.sh` — per
the data-safety rules above it's safe to run any time and will not touch
`.adds/gideon/data/`.
