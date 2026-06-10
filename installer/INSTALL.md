# Installing gideon on a Kobo

## What you need

- A Kobo e-reader (armv7 — Clara, Libra, Sage, Forma, Aura, etc.)
- The `gideon-kobo-bundle` zip from the latest build (post-merge CI artifact,
  or a GitHub release once those exist)
- Optional but recommended: [NickelMenu](https://pgaskin.net/NickelMenu/) to
  launch gideon from the Kobo home menu

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
