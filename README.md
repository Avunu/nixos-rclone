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
    nixpkgs.url = "github:NixOS/nixpkgs?ref=nixos-unstable";
    rclone-remotes.url = "github:Avunu/rclone-nixos-module";
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
        extraOpts = [
          "vfs-cache-mode=full"           # full read-write caching for Drive
        ];
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
          "--resync-on-new-path"
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
    };

    # ── Suspend/resume ──────────────────────────────────────────────────
    enableMountReset = true;   # default
    mountResetDelay = 15;      # seconds after resume before resetting
  };
}
```

## Pandoc integration (Obsidian ↔ Google Drive)

Bisync pairs can optionally convert between markdown and docx before/after each sync. This is useful for editing Obsidian vault files as Google Docs:

```nix
services.rclone-remotes.bisyncs.obsidian = {
  remote = "gdrive:ObsidianVault";
  localPath = "/home/user/.obsidian-docx";
  configFile = "/etc/rclone.conf";
  user = "user";
  interval = "5min";

  pandoc = {
    enable = true;
    markdownPath = "/home/user/ObsidianVault";
    syncDeletions = true;
    mdToDocxArgs = [ "--reference-doc=/home/user/template.docx" ];
    docxToMdArgs = [ "--wrap=none" "--extract-media=./media" ];
  };

  extraArgs = [
    "--verbose"
    "--resilient"
    "--recover"
    "--create-empty-src-dirs"
    "--max-lock" "5m"
    "--compare" "size,modtime,checksum"
    "--drive-export-formats" "docx"
    "--drive-import-formats" "docx"
  ];
};
```

When pandoc is enabled:

1. **Pre-sync**: Newer markdown files in `markdownPath` are converted to docx (applying the `strip-heading-ids` filter) and placed in `localPath`
2. **Rclone bisync** runs between `localPath` and the remote
3. **Post-sync**: Newer docx files from the remote are converted back to markdown (applying the `compact-lists` filter) in `markdownPath`

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
| `pandoc.enable` | bool | `false` | Enable md↔docx conversion |
| `pandoc.markdownPath` | string | — | Markdown/vault directory |
| `pandoc.syncDeletions` | bool | `false` | Propagate deletions |
| `pandoc.mdToDocxArgs` | list of strings | `[]` | Extra pandoc args (md→docx) |
| `pandoc.docxToMdArgs` | list of strings | `["--wrap=none"]` | Extra pandoc args (docx→md) |

Default `extraArgs`:
```nix
[ "--verbose" "--resilient" "--recover" "--resync-on-new-path" "--create-empty-src-dirs" "--max-lock" "5m" ]
```

## How FUSE mounts work

Mounts use `fileSystems` with `fsType = "rclone"` and systemd automount. They are:

- **Lazy**: not mounted until first access (`noauto` + `x-systemd.automount`)
- **Network-aware**: depend on `network-online.target`
- **Auto-unmounting**: idle timeout of 600s
- **Resilient**: aggressive retry and timeout settings
- **Cached**: VFS write-through cache with chunked reads

The module also creates a `rclone-mount-reset` service that clears failed mount/automount units after suspend/resume, allowing transparent reconnection on next access.

## License

MIT
