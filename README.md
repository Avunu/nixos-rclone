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
        # appended to baseArgs, not a replacement for it
        extraArgs = [
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
    # syncDeletions and trackMoves are on by default
    mdToDocxArgs = [ "--reference-doc=/home/user/template.docx" ];
    docxToMdArgs = [ "--wrap=none" "--extract-media=./media" ];
  };
};
```

When markdown sync is enabled:

1. **Pre-sync**: Moves/renames made in `path` are mirrored onto `localPath`, then newer markdown files are converted to docx and placed there
2. **Rclone bisync** runs between `localPath` and the remote
3. **Post-sync**: Moves/renames that arrived from the remote are mirrored onto `path`, then newer docx files are converted back to markdown

The optional args are passed through to the Pandoc CLI, which facilitates the conversion process.

### Moves and renames

The two trees are matched by path, so relocating or renaming a file on one side
reads as "deleted here, created there" on the other. Left alone, the stale
counterpart regenerates the document at its old path on the next run, so the
move never sticks and the file ends up at **both** paths, in **both** trees,
permanently. rclone bisync cannot help: it has no rename tracking and models
every move as delete + create.

`markdownSync.trackMoves` (on by default) closes this. An orphaned file is
paired with a newly-appeared one and moved to match, using two passes:

- **basename** — survives a relocation, even if the file was edited in transit
- **mtime** — survives a rename, which changes the basename but not the
  timestamp (a Drive move rewrites `parents`, not `modifiedTime`)

Only unambiguous 1:1 matches are acted on; anything else is logged and left
alone, as are genuine creates and deletes. Whole-directory moves work, since
their members pair individually. A file both renamed *and* edited before the
next run cannot be paired by either key — it is reported as an unpaired
orphan/new pair for you to resolve.

Deletions are handled by `syncDeletions`, which runs *after* this pass so that
only genuine deletions reach it.

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

> **Bisyncing native Google Docs needs `settlePass`.** With `importFormats`
> set (the default), every uploaded `.docx` is converted into a *native Google
> Doc*. Native Docs report `Size: -1` and no checksum, so modtime is the only
> change signal bisync has for the remote — and Drive rewrites it itself when
> the conversion finishes, seconds after rclone recorded the modtime it asked
> for. Every upload therefore makes the *next* run see the remote as "changed"
> even though nobody touched it. Alone that is harmless. But if the local side
> also changed in that window, bisync sees both sides as changed, declares a
> conflict, and drops a `.conflictN` file — on every run, for as long as you
> keep editing locally. `settlePass` closes the window and is on by default.

```nix
services.rclone-remotes.bisyncs.gdocs = {
  remote = "gdrive:";
  localPath = "/home/user/GoogleDrive";
  configFile = "/etc/rclone.conf";

  # settlePass, conflictResolve = "newer" and conflictLoser = "delete" are
  # all defaults, so nothing extra is needed here. Prefer keeping losers?
  #   conflictLoser = "num";

  googleDrive = {
    enable = true;
    rootFolderId = "AMa5T4yt9apUd24z_671iTMA5a_I4Hra6";  # omit to sync entire Drive
    exportFormats = "docx";  # default
    importFormats = "docx";  # default
  };
};
```

If you don't need documents to be *native* Google Docs, setting
`importFormats = null` is the stronger fix: uploads stay plain `.docx`, keeping
a real size and md5, and bisync becomes fully deterministic. Note that rclone
cannot update an *existing* native Doc without `--drive-import-formats`, so a
folder that already contains Google Docs must be migrated first.

## SFTP remotes

The SFTP backend has no hash primitive of its own. When rclone wants a checksum
it opens a *second* SSH channel and runs `md5sum <path>` on the server, then
parses the output. That works only if the shell sees the same files, under the
same names, as the SFTP session — and a server that jails SFTP to a virtual
root does not give you that. A Synology NAS is the usual case: it serves a
share as `/document` over SFTP while the shell knows it as
`/volume1/document`. Files that transfer perfectly well then fail their hash
check on every run:

```
ERROR : Home & Family/.../Keystone Scholars Fund.pdf: Failed to calculate src hash:
  failed to calculate md5 hash: failed to run "md5sum /document/Home\ \&\ Family/...":
  md5sum: '/document/Home & Family/...': No such file or directory
```

Note that `md5sum` itself ran fine, and that the path it printed is correctly
unescaped — it just does not exist outside the jail. Nothing installed locally
changes that. Give rclone the translation instead:

```nix
services.rclone-remotes.bisyncs.documents = {
  remote = "nas:/document";
  localPath = "/home/kevin/Documents";
  sftp.pathOverride = "@/volume1";
};
```

The leading `@` means "this is only the root" — rclone appends the remote's own
path itself, so `nas:/document` is looked up as `/volume1/document` and the
setting survives a change of remote path. Without the `@` the value has to
spell out the full shell path corresponding to the remote's root. The mechanism
is rclone's generic `--sftp-path-override`; nothing about it is Synology-
specific, and it applies equally to a chrooted OpenSSH account or a
containerised SFTP service. Leave it unset for an ordinary account over a real
home directory.

Where the shell cannot be made to reach the files at all — no shell access, no
`md5sum` on it, or a mapping that is not a fixed prefix — fall back to
`sftp.disableHashcheck = true`. Both sides are then left with no hash in
common and rclone compares size and modtime instead; bisync notes the fallback
once per run and continues, so `--compare size,modtime,checksum` in `baseArgs`
can stay as it is. Prefer `pathOverride` where it applies: real checksums are
what let bisync tell a genuine change from a file that merely has the same size
and a rewritten modtime.

## Excluding paths

`excludes` is a list of rclone `--exclude` patterns, applied to both mounts and
bisyncs. It defaults to `[ "#recycle/**" ]` — the per-share recycle bin a
Synology keeps at the root of every shared folder, which holds exactly the
files somebody already decided to throw away. Add `"@eaDir/**"` if the same NAS
is indexing media into thumbnail directories.

Changing this on an established bisync pair needs a moment's care. rclone only
forces a `--resync` when a `--filters-file` changes, and these are plain
`--exclude` flags, so nothing forces one here. Newly excluded files drop out of
both listings at once, which bisync reads as "deleted on both sides" and
accepts without touching either disk — but if they come to more than half the
pair, `--max-delete` aborts the run instead:

```
ERROR : Safety abort: too many deletes (>50%, 3 of 4) on Path1 "...". Run with --force if desired.
```

Nothing is deleted when that happens; the run simply stops. Recover by
resyncing the pair — remove the listings under `~/.cache/rclone/bisync/` and
start `rclone-bisync-<name>-init.service`. Note also that excluding a directory
stops it syncing but does not remove a copy an earlier run already made.

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
| `sftp.pathOverride` | string or null | `null` | Path the SSH shell sees for the SFTP root, so checksums work through an SFTP jail (see above) |
| `sftp.disableHashcheck` | bool | `false` | Give up on SFTP checksums entirely; fallback for when `pathOverride` cannot help |
| `excludes` | list of strings | `["#recycle/**"]` | `--exclude` patterns; defaults to the Synology recycle bin (see above) |

### Upgrading

Three defaults changed once native-Google-Docs support was fixed. If you are
coming from an earlier revision:

- **`extraArgs` is now additive**, appended to `baseArgs` rather than replacing
  it. If you had copied the old default list into `extraArgs` just to add a flag,
  delete the copy and keep only your additions — otherwise you will pass some
  flags twice. To *drop* one of the defaults, set `baseArgs` instead.
- **Conflict handling moved out of `extraArgs`** into `conflictResolve` and
  `conflictLoser`. Note `conflictLoser` defaults to `delete`, which discards the
  losing copy; set it to `"num"` for rclone's keep-everything behaviour.
- **`markdownSync.syncDeletions` and `settlePass.enable` now default to `true`.**
  The first means deletions actually propagate — including ones you had been
  relying on *not* propagating. The second costs a second listing pass plus
  `settlePass.delay` seconds per run, which is wasted on any backend that stores
  modtimes faithfully (SFTP, WebDAV, local); set `settlePass.enable = false`
  there.

Two more since:

- **Mounts no longer force `--sftp-disable-hashcheck`.** It used to be
  hardcoded, which silently gave up checksums on every SFTP mount; it is now
  `sftp.disableHashcheck`, off by default. If your SFTP server jails the SFTP
  session away from the shell, set `sftp.pathOverride` (the real fix) or turn
  the flag back on — otherwise the hash failures it was hiding will surface.
- **`excludes` defaults to `[ "#recycle/**" ]`.** On an established bisync pair
  those files leave both listings at once, which is harmless, but see
  [Excluding paths](#excluding-paths) for the `--max-delete` case.

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
| `baseArgs` | list of strings | see below | Base `rclone bisync` arguments; replace to drop a default |
| `extraArgs` | list of strings | `[]` | Additional arguments, appended to `baseArgs` |
| `conflictResolve` | enum | `"newer"` | Which side wins a conflict (`--conflict-resolve`) |
| `conflictLoser` | enum | `"delete"` | What happens to the losing copy: `num`, `pathname` or `delete` |
| `settlePass.enable` | bool | `true` | Run a second bisync pass to reconcile remotes that rewrite modtimes after upload (see Google Drive above); turn off for SFTP/WebDAV/local |
| `settlePass.delay` | int | `30` | Seconds between the two passes |
| `googleDrive.enable` | bool | `false` | Apply Google Drive-specific flags |
| `googleDrive.rootFolderId` | string or null | `null` | Restrict sync to a specific Drive folder ID |
| `googleDrive.exportFormats` | string | `"docx"` | Formats to export Google Docs as |
| `googleDrive.importFormats` | string | `"docx"` | Formats to import into Google Docs |
| `sftp.pathOverride` | string or null | `null` | Path the SSH shell sees for the SFTP root, so checksums work through an SFTP jail (see above) |
| `sftp.disableHashcheck` | bool | `false` | Give up on SFTP checksums entirely; fallback for when `pathOverride` cannot help |
| `excludes` | list of strings | `["#recycle/**"]` | `--exclude` patterns; defaults to the Synology recycle bin (see above) |
| `markdownSync.enable` | bool | `false` | Enable md↔docx conversion |
| `markdownSync.path` | string | — | Markdown/vault directory |
| `markdownSync.syncDeletions` | bool | `true` | Propagate deletions (without it, a deletion is undone on the next run) |
| `markdownSync.trackMoves` | bool | `true` | Follow moves/renames instead of duplicating them (see above) |
| `markdownSync.mdToDocxArgs` | list of strings | `[]` | Extra args (md→docx) |
| `markdownSync.docxToMdArgs` | list of strings | `["--wrap=none"]` | Extra args (docx→md) |

Default `baseArgs`:
```nix
[ "--verbose" "--resilient" "--recover" "--create-empty-src-dirs" "--max-lock" "5m" "--compare" "size,modtime,checksum" ]
```

The conflict flags are not in that list — they come from `conflictResolve` and
`conflictLoser`, so changing conflict behaviour does not mean restating
everything else. Final argument order is:

```
baseArgs ++ conflict flags ++ Google Drive flags ++ extraArgs
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
