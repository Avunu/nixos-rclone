# rclone-nixos-module

A NixOS module providing:

- **Live FUSE mounts** via `fileSystems` with systemd automount (lazy, on-demand)
- **Bidirectional sync** (`rclone bisync`) on a timer with optional pandoc markdown↔docx conversion
- **Suspend/resume recovery** that resets failed mounts after waking from sleep
- **Automatic directory creation** via systemd tmpfiles

## Installation

Add the flake input and import the module:

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

## Complete configuration example

```nix
# my-remotes.nix
{ config, ... }:

let
  home = "/home/user";
  webdavConf = "${home}/rclone-webdav.conf";
in
{
  services.rclone-remotes = {
    enable = true;

    # ── Global defaults ─────────────────────────────────────────────────
    defaultConfigFile = webdavConf;
    defaultUser = "user";
    defaultGroup = "users";
    defaultUid = 1000;
    defaultGid = 100;

    # ── Live FUSE mounts (systemd automount) ────────────────────────────
    mounts = {
      documents = {
        remote = "webdav:document";
        localPath = "${home}/Documents";
      };
      photos = {
        remote = "webdav:photo";
        localPath = "${home}/Pictures";
      };
      gdrive = {
        remote = "gdrive:";
        localPath = "/run/media/user/GDrive";
        configFile = "/etc/rclone.conf";  # override global default
        googleDrive.enable = true;        # export Google Docs/Sheets/Slides as real files
      };
    };

    # ── Bisync pairs (periodic two-way sync) ────────────────────────────
    bisyncs = {
      ssh = {
        remote = "webdav:ssh";
        localPath = "${home}/.ssh";
        dirPerms = "0700";
        interval = "15min";
        onBootSec = "2min";
        extraArgs = [
          "--verbose"
          "--resilient"
          "--recover"
          "--create-empty-src-dirs"
          "--max-lock" "5m"
          "--checksum"
          "--links"
        ];
      };
      fonts = {
        remote = "webdav:font";
        localPath = "${home}/.local/share/fonts";
        interval = "1h";
      };
      gdocs = {
        remote = "gdrive:Documents";
        localPath = "${home}/GoogleDocs";
        configFile = "/etc/rclone.conf";
        googleDrive.enable = true;
      };
    };

    # ── Suspend/resume ──────────────────────────────────────────────────
    enableMountReset = true;   # default
    mountResetDelay = 15;      # seconds after resume before resetting
  };
}
```

## Markdown sync (Obsidian ↔ Google Drive)

Bisync pairs can optionally convert between markdown and docx before/after each sync. This is useful for editing Obsidian vault files as Google Docs:

```nix
services.rclone-remotes.bisyncs.obsidian = {
  remote = "gdrive:ObsidianVault";
  localPath = "/home/user/.obsidian-docx";
  configFile = "/etc/rclone.conf";
  user = "user";
  interval = "5min";

  googleDrive = {
    enable = true;
    rootFolderId = "AMa5T4yt9apUd24z_671iTMA5a_I4Hra6";  # optional
  };

  markdownSync = {
    enable = true;
    path = "/home/user/ObsidianVault";
    syncDeletions = true;
    mdToDocxArgs = [ "--reference-doc=/home/user/template.docx" ];
    docxToMdArgs = [ "--wrap=none" "--extract-media=./media" ];
  };
};
```

When markdown sync is enabled:

1. **Pre-sync**: Newer markdown files in `path` are converted to docx and placed in `localPath`
2. **Rclone bisync** runs between `localPath` and the remote
3. **Post-sync**: Newer docx files from the remote are converted back to markdown in `path`

The optional args are passed through to the Pandoc CLI, which facilitates the conversion process.

## Google Drive integration

Set `googleDrive.enable = true` on any **mount** or **bisync** pair to export Google Workspace files (Docs, Sheets, Slides) as real Office files instead of 0-byte stubs.

### Mounts

```nix
services.rclone-remotes.mounts.gdrive = {
  remote = "gdrive:";
  localPath = "/run/media/user/GDrive";
  configFile = "/etc/rclone.conf";

  googleDrive = {
    enable = true;
    rootFolderId = "AMa5T4yt9apUd24z_671iTMA5a_I4Hra6";  # omit to mount entire Drive
    exportFormats = "docx";  # default — Google Docs appear as .docx
    importFormats = "docx";  # default — .docx uploads convert to Google Docs
  };
};
```

This passes `drive-export-formats` and `drive-import-formats` as FUSE mount options so Google Workspace files have real content.

### Bisync

For bisync pairs, `googleDrive.enable = true` additionally applies:

- `--fix-case` — handle Drive's case-insensitive filesystem
- `--slow-hash-sync-only` — limit checksum computation to files where size+modtime already match, avoiding expensive full-file hashes on every sync

```nix
services.rclone-remotes.bisyncs.gdocs = {
  remote = "gdrive:";
  localPath = "/home/user/GoogleDrive";
  configFile = "/etc/rclone.conf";

  googleDrive = {
    enable = true;
    rootFolderId = "AMa5T4yt9apUd24z_671iTMA5a_I4Hra6";  # omit to sync entire Drive
    exportFormats = "docx";  # default
    importFormats = "docx";  # default
  };
};
```

## Options reference

### Top-level

| Option | Type | Default | Description |
|--------|------|---------|-------------|
| `enable` | bool | `false` | Enable the module |
| `defaultConfigFile` | string or null | `null` | Default rclone config path (`null` = use rclone's default `~/.config/rclone/rclone.conf`) |
| `defaultUser` | string | `"root"` | Default user for mounts/syncs |
| `defaultGroup` | string | `"users"` | Default group |
| `defaultUid` | int | `1000` | Default UID for FUSE mounts |
| `defaultGid` | int | `100` | Default GID for FUSE mounts |
| `enableMountReset` | bool | `true` | Reset failed mounts after resume |
| `mountResetDelay` | int | `15` | Seconds to wait after resume |

### `mounts.<name>`

| Option | Type | Default | Description |
|--------|------|---------|-------------|
| `remote` | string | — | Rclone remote path (e.g. `myremote:path`) |
| `localPath` | string | — | Local mount point |
| `configFile` | string or null | global default | Rclone config file path (`null` = rclone's default) |
| `uid` | int | global default | UID for the FUSE mount |
| `gid` | int | global default | GID for the FUSE mount |
| `user` | string | global default | Owner for tmpfiles rule |
| `group` | string | global default | Group for tmpfiles rule |
| `dirPerms` | string | `"0755"` | Directory permissions |
| `extraOpts` | list of strings | `[]` | Extra mount options |
| `googleDrive.enable` | bool | `false` | Export Google Workspace files as real Office files |
| `googleDrive.rootFolderId` | string or null | `null` | Restrict mount to a specific Drive folder ID |
| `googleDrive.exportFormats` | string | `"docx"` | Formats to export Google Docs/Sheets/Slides as |
| `googleDrive.importFormats` | string | `"docx"` | Formats to import when writing back to Drive |

### `bisyncs.<name>`

| Option | Type | Default | Description |
|--------|------|---------|-------------|
| `remote` | string | — | Rclone remote path |
| `localPath` | string | — | Local directory to sync |
| `configFile` | string or null | global default | Rclone config file path (`null` = rclone's default) |
| `user` | string | global default | User to run sync as |
| `group` | string | global default | Group for service |
| `dirPerms` | string | `"0755"` | Directory permissions |
| `interval` | string | `"15min"` | Re-sync interval |
| `onBootSec` | string | `"5min"` | Delay before first sync |
| `extraArgs` | list of strings | see below | Extra `rclone bisync` arguments |
| `googleDrive.enable` | bool | `false` | Apply Google Drive-specific flags |
| `googleDrive.rootFolderId` | string or null | `null` | Restrict sync to a specific Drive folder ID |
| `googleDrive.exportFormats` | string | `"docx"` | Formats to export Google Docs as |
| `googleDrive.importFormats` | string | `"docx"` | Formats to import into Google Docs |
| `markdownSync.enable` | bool | `false` | Enable md↔docx conversion |
| `markdownSync.path` | string | — | Markdown/vault directory |
| `markdownSync.syncDeletions` | bool | `false` | Propagate deletions |
| `markdownSync.mdToDocxArgs` | list of strings | `[]` | Extra args (md→docx) |
| `markdownSync.docxToMdArgs` | list of strings | `["--wrap=none"]` | Extra args (docx→md) |

Default `extraArgs`:
```nix
[ "--verbose" "--resilient" "--recover" "--create-empty-src-dirs" "--max-lock" "5m" "--conflict-resolve" "newer" "--compare" "size,modtime,checksum" ]
```

## How FUSE mounts work

Mounts use `fileSystems` with `fsType = "rclone"`, relying on the `mount.rclone`
helper that the module installs via `system.fsPackages`. The helper translates
mount options (`vfs-cache-mode=full`, `config=...`, ...) into rclone flags.
Mounts are:

- **Lazy**: not mounted until first access (`noauto` + `x-systemd.automount`)
- **Network-aware**: depend on `network-online.target`
- **Auto-unmounting**: idle timeout of 600s
- **Cached**: VFS write-through cache with chunked reads. Because systemd runs
  mount helpers with an empty environment (no `$HOME`), each mount gets an
  explicit cache directory at `/var/cache/rclone/<name>`.

### Credential configs

`.mount` units cannot use systemd's `LoadCredential`, and rclone wants to write
token refreshes back to its config file, which a read-only secret (e.g. agenix)
would reject. For every mount with a `configFile`, a single `rclone-config`
oneshot service stages a writable copy at `/run/rclone/<name>.conf` (mode 0600,
root-only) before the mount starts. The staged copy is re-created from the
secret on reboot and on config changes.

### Suspend/resume recovery

The `rclone-mount-reset` service runs after resume. For each configured mount
it lazily unmounts stale FUSE mounts (left behind when rclone dies uncleanly —
"transport endpoint is not connected") and clears the failed state of exactly
that mount's `.mount`/`.automount` units, so the next access transparently
remounts. Healthy mounts are left untouched.

## License

MIT
