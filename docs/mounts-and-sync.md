---
title: Mount and sync cloud storage
description: "Configure live rclone FUSE automounts and event-driven two-way sync on NixOS, including Markdown to Google Docs conversion for an Obsidian vault."
nav_title: Mounts and sync
order: 1
tags: [mounts, bisync, google-drive]
updated: 2026-11-24
---

`nixos-rclone` is a NixOS module plus a small Rust daemon (`rclone-remotes`) that turns rclone remotes into declarative NixOS configuration. Use it when you want cloud or NAS storage to appear as ordinary folders: either as live, on-demand mounts, or as local copies that stay in two-way sync.

It gives you two kinds of resource, both configured under `services.rclone-remotes`:

- **Mounts** are FUSE filesystems that systemd mounts lazily on first access.
- **Bisync pairs** are local folders kept in sync with a remote. Local changes are pushed within seconds, and remote changes are pulled on a timer.

## Install the module

Add the flake as an input, follow your own nixpkgs, and import the module:

```nix
{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    rclone-remotes.url = "github:Avunu/nixos-rclone";
    rclone-remotes.inputs.nixpkgs.follows = "nixpkgs";
  };

  outputs = { nixpkgs, rclone-remotes, ... }: {
    nixosConfigurations.myhost = nixpkgs.lib.nixosSystem {
      modules = [
        rclone-remotes.nixosModules.default
        ./my-remotes.nix
      ];
    };
  };
}
```

> [!TIP]
> The daemon is built from the repository's `Cargo.lock` and published to a public Cachix cache for `x86_64-linux` and `aarch64-linux`. Cache hits only happen when your nixpkgs matches the flake's locked one, so use `inputs.nixpkgs.follows = "rclone-remotes/nixpkgs"` instead of the line above if you want to avoid compiling it. The module enables the cache for you (`services.rclone-remotes.binaryCache.enable`, on by default).

You also need an rclone config that defines the remotes you reference. The module does not create remotes; create them with `rclone config` first and point `configFile` at the result.

## Configure mounts and sync pairs

This example defines one mount and two bisync pairs, with global defaults for the user:

```nix
# my-remotes.nix
{ ... }:

let
  home = "/home/jane";
in
{
  services.rclone-remotes = {
    enable = true;

    defaultConfigFile = "${home}/rclone.conf";
    defaultUser = "jane";
    defaultGroup = "users";
    defaultUid = 1000;
    defaultGid = 100;

    mounts.documents = {
      remote = "webdav:documents";
      localPath = "${home}/Documents";
    };

    bisyncs = {
      fonts = {
        remote = "webdav:fonts";
        localPath = "${home}/.local/share/fonts";
        pull.interval = "1h";
        push.enable = false;   # fonts only change on the server
      };
      notes = {
        remote = "webdav:notes";
        localPath = "${home}/Notes";
        pull.interval = "5min";
      };
    };
  };
}
```

Apply it with `nixos-rebuild switch`. The module validates every generated daemon config at build time (`rclone-remotes validate` runs as a system check), so a typo in an option fails the rebuild instead of surfacing at runtime.

### Global defaults

| Option | Default | Meaning |
| --- | --- | --- |
| `enable` | `false` | Turns the module on. |
| `defaultConfigFile` | `null` | rclone config used when a mount or pair does not set `configFile`. `null` means rclone's own `~/.config/rclone/rclone.conf`. |
| `defaultUser` | `"root"` | Owner for mounts and the user a sync service runs as. |
| `defaultGroup` | `"users"` | Default group. |
| `defaultUid` / `defaultGid` | `1000` / `100` | Ownership of files shown by FUSE mounts. |
| `enableMountReset` | `true` | Recover mounts after suspend (see below). |
| `mountResetDelay` | `15` | Seconds to wait after resume before resetting. |
| `package` / `rclonePackage` | module build / `pkgs.rclone` | The `rclone-remotes` binary, and the rclone used for mounts and the daemon's private `rclone rcd`. |

The daemon speaks the rc API of rclone 1.75. A different minor version works but is logged.

## Live mounts

A mount is a `fileSystems` entry with `fsType = "rclone"`, using the `mount.rclone` helper the module installs through `system.fsPackages`. The module sets these behaviors for you:

- **Lazy mounting.** The mount is `noauto` with `x-systemd.automount`, so nothing connects until something touches the folder. It also waits for `network-online.target`.
- **Idle unmount.** Mounts detach after 600 seconds of inactivity (`x-systemd.idle-timeout=600`).
- **Full file cache.** `vfs-cache-mode=full` caches reads and writes on local disk, with the cache in `/var/cache/rclone/<name>`. Writes are uploaded after the program closes the file. systemd runs mount helpers without `$HOME`, so the module sets an explicit cache directory.
- **Logging.** The `syslog` option is set so errors from the background writeback reach the journal. With a full VFS cache, uploads happen after the writing program has already closed the file, so without logging a failed upload is invisible.

Per-mount options:

| Option | Default | Meaning |
| --- | --- | --- |
| `remote` | required | rclone remote path, such as `myremote:path`. |
| `localPath` | required | Mount point. The module creates it with systemd tmpfiles. |
| `configFile` | global default | rclone config for this mount. |
| `uid` / `gid` | global default | Ownership shown by the mount. |
| `user` / `group` | global default | Owner of the mount point directory. |
| `dirPerms` | `"0755"` | Permissions of the mount point directory. |
| `extraOpts` | `[]` | Extra mount options in `flag=value` form, translated to `--flag=value`. Values must not contain commas. |
| `googleDrive.*`, `sftp.*`, `excludes` | | Described below. |

### Credential files

If a mount sets `configFile`, a one-shot `rclone-config` service copies that file to `/run/rclone/<name>.conf` (mode 0600, root only) before the mount starts, and the mount reads the copy. This matters because rclone writes refreshed OAuth tokens back to its config, and a read-only secret, such as one managed with agenix, would reject every refresh.

Bisync services do the equivalent themselves: they receive the config through `LoadCredential` and keep a writable copy in their own runtime directory.

## Two-way sync

Each bisync pair runs as one long-lived service, `rclone-bisync-<name>.service`. Inside it, the daemon starts a private `rclone rcd` and drives it over rclone's remote-control API.

- **Pushing.** A watcher uses inotify on `localPath`, waits until the tree has been quiet for `push.debounce` (default `2s`), then uploads new and changed files and applies deletions. A rename becomes a server-side move, so a Google Drive document keeps its file ID, sharing and history. The daemon patches bisync's listings so the next pull sees the same file at a new path, not a delete plus a create.
- **Pulling.** rclone cannot subscribe to remote changes, so the daemon runs `rclone bisync` every `pull.interval`. This is also the safety net for anything the watcher missed.
- **First run.** A pair that has never synced is initialized by the daemon with a `--resync`, keeping the newer copy. Nothing is pushed until that finishes.
- **Safety.** A sudden burst of deletions, such as an unmounted vault or an emptied folder, is withheld from pushing and left to the next pull, where the `maxDelete` check judges it. If bisync locks itself out after a critical error, the daemon reports it and waits for you instead of resyncing on its own.

> [!WARNING]
> With the defaults, `conflict.resolve = "newer"` and `conflict.loser = "delete"`. When a file really was edited on both sides, the older edit is discarded. Set `conflict.loser = "num"` to keep the loser as `file.docx.conflict1`, `file.docx.conflict2` and so on.

### Pair options

| Option | Default | Meaning |
| --- | --- | --- |
| `remote`, `localPath` | required | Remote path and local directory. |
| `configFile`, `user`, `group`, `dirPerms` | global defaults, `"0755"` | As for mounts. |
| `workdir` | `~/.cache/rclone/bisync` of the user | Where bisync keeps its listings. This is rclone's own default, so existing pairs are not forced to resync. |
| `push.enable` | `true` | Watch and push local changes. Set `false` for data that only changes on the server. |
| `push.debounce` | `"2s"` | Quiet time before a burst is pushed. |
| `pull.interval` | `"15min"` | How often to pull. |
| `pull.onBoot` | `"5min"` | Delay before the first pull after the service starts. |
| `pull.jitter` | `"5min"` | Random delay added to each pull. |
| `conflict.resolve` | `"newer"` | One of `none`, `path1`, `path2`, `newer`, `older`, `larger`, `smaller`. |
| `conflict.loser` | `"delete"` | One of `num`, `pathname`, `delete`. |
| `compare` | `"size,modtime,checksum"` | How two files are judged equal. |
| `resilient`, `recover`, `createEmptySrcDirs` | `true` | The matching bisync options. |
| `maxLock` | `"5m"` | How long a crashed run's lock is honored. |
| `maxDelete` | `null` | Abort above this percentage of deletions (0 to 100). `null` uses rclone's 50. |
| `extraParams` | `{}` | Extra parameters for rclone's `sync/bisync` rc call, merged over the typed options. |
| `settle.enable` | `true` | Run a second pass after a pull and after pushed changes. |
| `settle.delay` | `30` | Seconds between the two passes. |

Time values use systemd time-span syntax. `push.debounce` must be at least 100ms.

For very large trees, the watcher needs an inotify watch per directory. If you hit the limit, raise `boot.kernel.sysctl."fs.inotify.max_user_watches"`. NixOS defaults to 524288.

### Inspect and control a running pair

The module installs the `rclone-remotes` command. Run it as the pair's user or as root. It talks to the pair's control socket at `/run/rclone-remotes/<name>/ctl.sock`.

```bash
rclone-remotes ctl --name notes status   # state, last success, push counters
rclone-remotes ctl --name notes sync     # pull now and wait for it
rclone-remotes ctl --name notes resync   # rebuild the listings (--resync)
```

`status` prints JSON. It includes the pair's state (`starting`, `idle`, `syncing`, `failed` or `needs-resync`), the last success time and error, completed passes, resync count, push counters for uploaded, moved and deleted files, and the number of pushes waiting to retry. A `needs-resync` state is not retried until you run `resync`.

To run or restart the service itself, use `systemctl` on `rclone-bisync-<name>.service`. There is no timer unit and no separate init unit.

## Google Drive

Set `googleDrive.enable = true` on a mount or pair to export Google Docs as real `.docx` files and to upload `.docx` files back as native Google Docs:

```nix
services.rclone-remotes.mounts.gdrive = {
  remote = "gdrive:";
  localPath = "/run/media/jane/GDrive";
  configFile = "/etc/rclone.conf";

  googleDrive = {
    enable = true;
    rootFolderId = "<DRIVE_FOLDER_ID>";   # omit to use the whole Drive
    exportFormats = "docx";               # default
    importFormats = "docx";               # default
  };
};
```

On a mount, this passes `drive-export-formats`, `drive-import-formats` and (if set) `drive-root-folder-id` to rclone. On a bisync pair it also applies `fix_case` and `slowHashSyncOnly`, which handle Drive's case-insensitive naming and avoid hashing every file on every run.

The module's default `exportFormats` is `docx`, which exports Google Docs only. rclone's own default is `docx,xlsx,pptx,svg`, so set `exportFormats = "docx,xlsx,pptx";` if you also want Sheets and Slides as Office files.

### Why the settle pass matters

With `importFormats` set, every uploaded `.docx` becomes a native Google Doc. Native Docs report no size or checksum, so bisync only has the modification time to go on, and Drive rewrites that time itself a few seconds after the conversion finishes. The next run then sees the remote as changed even though nobody touched it. If you also edited the file locally in that window, bisync declares a conflict.

The `settle` pass closes that window with a second bisync run after `settle.delay` seconds, so the listings converge within the run. It is on by default. Turn it off only for remotes that store modification times faithfully, such as SFTP, WebDAV or local paths, where it costs an extra listing and the delay on every run.

> [!TIP]
> If you do not need native Google Docs, set `importFormats = "";` on the pair. Uploads then stay plain `.docx` with a real size and checksum, and bisync becomes fully deterministic. The option is a string, so `null` is rejected. This comes from reading the module and rclone sources, not from a test against a live Drive. An existing native Doc cannot be updated without `--drive-import-formats`, so migrate a folder that already contains Google Docs before you change this.

## Edit an Obsidian vault as Google Docs

A bisync pair can convert between a folder of Markdown notes and the `.docx` files it syncs. Point `markdownSync.path` at the vault and `localPath` at a separate staging folder that holds the converted files:

```nix
services.rclone-remotes.bisyncs.obsidian = {
  remote = "gdrive:ObsidianVault";
  localPath = "/home/jane/.obsidian-docx";
  configFile = "/etc/rclone.conf";
  user = "jane";
  pull.interval = "5min";

  googleDrive.enable = true;

  markdownSync = {
    enable = true;
    path = "/home/jane/ObsidianVault";
    referenceDoc = /home/jane/template.docx;   # optional
  };
};
```

The module asserts that `markdownSync.path` is set whenever `markdownSync.enable` is true, and the two directories must be separate. Conversion happens inside the daemon using the [carta](https://github.com/mfkrause/carta) library; pandoc is not used or installed.

### What happens when you edit

- **A note is created, edited, renamed or deleted.** The daemon converts it immediately, and the resulting `.docx` change is pushed like any other local change.
- **Before each pull**, the daemon reconciles the whole vault, which also catches changes made while the daemon was not running.
- **After each pull**, documents that changed on the remote are converted back to notes, and moves made on the remote move the notes.

Conversion compares modification times and copies the source's time onto its output, so a converted pair compares equal and nothing is converted twice. Hidden files and folders such as `.obsidian` and `.trash` are never touched.

For styling, each note's own previous `.docx` is the reference for later conversions, so formatting you apply in Google Docs survives later edits. `referenceDoc` only styles a note's first conversion.

> [!NOTE]
> Some Obsidian features do not round-trip. Wikilinks and callouts come back escaped, and images are not embedded.

### Moves and renames

The vault and the `.docx` tree are matched by path. Left alone, moving or renaming a note looks like a deletion in one place and a creation in another, and the stale counterpart regenerates the document at its old path, so the file ends up at both paths. bisync has no rename tracking, so it cannot prevent this.

`markdownSync.trackMoves` (on by default) pairs the orphaned file with the new one and moves it to match. It tries three matches in turn:

1. **Identity.** The note's inode and birth time, which survive a rename even if the note was edited at the same time.
2. **Basename.** Survives a relocation to another folder.
3. **Modification time.** Survives a rename, which changes the name but not the timestamp.

Only unambiguous one-to-one matches are acted on, and never onto an existing path. Whole-folder moves work. A move made in the vault reaches the remote as a server-side move before the next pull. If the remote is unreachable, the pull waits and the rename is retried, including after a restart.

`markdownSync.syncDeletions` (also on by default) propagates genuine deletions after move tracking has run. An empty or unmounted side is never propagated. Without it, a deleted note comes back on the next run.

## SFTP remotes

The SFTP backend has no built-in hashing. To get a checksum, rclone opens a second SSH channel and runs `md5sum` on the server. If the server jails SFTP into a virtual root, the shell sees different paths than the SFTP session, and files that transfer fine fail their hash check with a "No such file or directory" error from `md5sum`. NAS devices that serve a share as `/document` over SFTP while the shell knows it as `/volume1/document` are the common case.

Tell rclone about the translation with `sftp.pathOverride`:

```nix
services.rclone-remotes.bisyncs.documents = {
  remote = "nas:/document";
  localPath = "/home/jane/Documents";
  sftp.pathOverride = "@/volume1";
};
```

The leading `@` means the value is only the root, and rclone appends the remote's own path. Here `nas:/document` is looked up as `/volume1/document`. Without the `@`, write out the full shell path for the remote's root. Leave the option unset for an ordinary account over a real home directory.

If the shell cannot reach the files at all, set `sftp.disableHashcheck = true`. Both sides then compare size and modification time instead. Prefer `pathOverride` where it applies, because real checksums are what let bisync tell a genuine change from a rewritten timestamp.

> [!WARNING]
> A wrong `pathOverride` on a mount can fail silently. With the full VFS cache, a failing writeback leaves the file looking present on the mount while the remote never receives it. Check the journal after you set it.

## Exclude paths

`excludes` is a list of rclone `--exclude` patterns for mounts and pairs. The watcher honors it exactly as rclone does, so an excluded file is never pushed. The module's default list is:

```nix
[
  ".AppleDouble"
  ".DS_Store"
  ".Spotlight-V100"
  ".Trashes"
  "@eaDir/**"
  "#recycle/**"
  "$RECYCLE.BIN/**"
  "Thumbs.db"
]
```

Setting `excludes` replaces the whole list. Changing it on an established pair needs care, because rclone only forces a `--resync` when a filters file changes, and these are plain exclude flags. Newly excluded files drop out of both listings, which bisync reads as deleted on both sides. If they are more than half of the pair, `maxDelete` aborts the run with a "Safety abort: too many deletes" error. Nothing is deleted when that happens. Recover with:

```bash
rclone-remotes ctl --name <PAIR_NAME> resync
```

Excluding a directory stops it syncing but does not remove a copy an earlier run already made.

## Recovery after suspend

When a laptop sleeps, rclone can die uncleanly and leave a stale FUSE mount that fails with "transport endpoint is not connected". With `enableMountReset = true` (the default) and at least one mount configured, a `rclone-mount-reset` service runs after `suspend`, `hibernate` and `hybrid-sleep`. It waits `mountResetDelay` seconds for the network, then for each configured mount it lazily unmounts the stale filesystem and clears the failed state of that mount's `.mount` and `.automount` units. The next access remounts it. Healthy mounts are left alone.

## Upgrade from the script-based module

Older versions ran `rclone bisync` from a timer. Several options moved and keep working with a deprecation warning that names the replacement:

| Old | New |
| --- | --- |
| `interval`, `onBootSec` | `pull.interval`, `pull.onBoot` |
| `conflictResolve`, `conflictLoser` | `conflict.resolve`, `conflict.loser` |
| `settlePass.enable`, `settlePass.delay` | `settle.enable`, `settle.delay` |

These options were removed, and setting them is an error:

| Removed | Use instead |
| --- | --- |
| `baseArgs` | `compare`, `resilient`, `recover`, `createEmptySrcDirs`, `maxLock` |
| `extraArgs` | `extraParams` (rc parameters, not CLI flags) |
| `markdownSync.mdToDocxArgs` | `markdownSync.referenceDoc` |
| `markdownSync.docxToMdArgs` | nothing; Markdown is always written unwrapped |

Existing listings are kept, so an upgraded pair is not resynced. Local changes now arrive immediately, so set `push.enable = false` on any pair that should not behave that way.

## Troubleshooting

- **A mount fails or a write never reaches the remote.** Mount errors are logged to the journal through `syslog`, so read the journal for the mount unit first.
- **A pair shows `needs-resync`.** Read `lastError` in `rclone-remotes ctl --name <PAIR_NAME> status`, fix the cause, then run `resync`.
- **`.conflictN` files keep appearing on Google Drive.** Confirm `settle.enable` is still `true`, or switch the pair to `importFormats = "";`.
- **A rebuild fails with a config validation error.** The module and daemon schema must match. Both ship in the same flake, so update the flake input as a whole.

## Related documents

- [A NixOS Micro Desktop for Family and Friends](https://docs.avunu.net/nixos-micro-desktop/docs/getting-started/), for a NixOS desktop you could pair this module with.
- [Mount a Drive with a systemd Mount Unit](https://avunu.net/kb/linux/systemd-mount-units/), for plain `.mount` and `.automount` units on any Linux host.
- [Why Nix for Business Systems](https://avunu.net/kb/nixos/why-nix-for-business-systems/), for the wider case for declarative configuration.
