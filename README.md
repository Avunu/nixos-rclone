# rclone-nixos-module

A NixOS module, backed by a small Rust daemon (`rclone-remotes`), providing:

- **Live FUSE mounts** via `fileSystems` with systemd automount (lazy, on-demand)
- **Bidirectional sync** (`rclone bisync`) where **local changes are pushed the
  moment they happen** — created, edited, renamed and deleted files reach the
  remote within seconds — and remote changes are pulled on a timer
- **Markdown ↔ docx conversion** built in (no pandoc), for editing an Obsidian
  vault as Google Docs, with moves and renames followed as renames
- **Suspend/resume recovery** that resets failed mounts after waking from sleep
- **Automatic directory creation** via systemd tmpfiles

## How it works

Each bisync pair is one long-running service, `rclone-bisync-<name>.service`,
that talks to a private `rclone rcd` over rclone's remote-control API:

```
 local files ──inotify──▶  rclone-remotes  ──rc API──▶  rclone rcd ──▶ remote
       ▲                    │  ▲                             
       └──── pull timer ────┘  └── markdown ⇄ docx (carta)
```

- **Pushing.** A watcher notices changes under `localPath`, waits for the tree
  to be quiet (`push.debounce`), then uploads new and modified files, applies
  deletions, and turns a rename into a **server-side move** — so a Google Drive
  document keeps its file ID, sharing and history. bisync's listings are
  patched to match, so the next pull reads the rename as the same file at a new
  path instead of replaying it as delete + create.
- **Pulling.** rclone offers no way to subscribe to remote changes, so the
  remote is polled with `rclone bisync` every `pull.interval`. This is also the
  safety net for anything the watcher missed.
- **First run.** A pair that has never synced is initialised by the daemon
  itself (a `--resync`, keeping the newer copy). Nothing is pushed until then.
- **Safety.** A burst of deletions (a vault unmounted, a folder emptied) is
  withheld from pushing and left to the pull, whose `maxDelete` check judges
  it. A bisync that locks itself out after a critical error is *not* resynced
  automatically; the daemon reports it and waits for you.

Inspect or nudge a running pair with the `rclone-remotes` command the module
installs (run as the pair's user, or root):

```
rclone-remotes ctl --name documents status   # state, last success, push counters
rclone-remotes ctl --name documents sync     # pull now, and wait for it
rclone-remotes ctl --name documents resync   # rebuild the listings (--resync)
```

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
        pull.interval = "15min";
        pull.onBoot = "2min";
        # extra rclone `sync/bisync` parameters, merged over the typed options
        extraParams.ignoreListingChecksum = true;
      };
      fonts = {
        remote = "webdav:font";
        localPath = "${home}/.local/share/fonts";
        pull.interval = "1h";
        push.enable = false;     # fonts only ever change on the server
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

A bisync pair can convert between a directory of markdown notes and the docx
files it syncs. This is useful for editing an Obsidian vault as Google Docs:

```nix
services.rclone-remotes.bisyncs.obsidian = {
  remote = "gdrive:ObsidianVault";
  localPath = "/home/user/.obsidian-docx";
  configFile = "/etc/rclone.conf";
  user = "user";
  pull.interval = "5min";

  googleDrive = {
    enable = true;
    rootFolderId = "AMa5T4yt9apUd24z_671iTMA5a_I4Hra6";  # optional
  };

  markdownSync = {
    enable = true;
    path = "/home/user/ObsidianVault";
    # syncDeletions and trackMoves are on by default
    referenceDoc = /home/user/template.docx;   # optional: styles for new documents
  };
};
```

The conversion is done in the daemon with the [carta](https://github.com/mfkrause/carta)
library; pandoc is not used. Both directions keep the notes round-tripping
exactly: markdown → docx strips heading ids and makes tight lists loose (so
Google Docs renders each item as a paragraph); docx → markdown makes them tight
again and writes unwrapped lines.

With markdown sync enabled:

- **A note is edited, created, renamed or deleted in the vault** → the daemon
  converts it at once, and the docx change is pushed like any other local
  change. A rename of a note renames its docx, and from there the remote file.
- **Before each pull**, the vault is reconciled as a whole, which also catches
  anything that happened while the daemon was not running.
- **After each pull**, documents that changed on the remote are converted back
  to notes, and moves made on the remote move the notes.

Conversion decides by modification time, and copies the source's time onto its
output, so a converted pair compares equal and nothing is converted twice.
Hidden files and folders (`.obsidian`, `.trash`) are never touched. Each note's
own previous docx is the reference for its styling, so formatting applied in
Google Docs survives later edits; `referenceDoc` styles a note's first
conversion.

Not carried across, as before: Obsidian wikilinks and callouts come back
escaped, and images are not embedded.

### Moves and renames

The vault and the docx tree are matched by path, so relocating or renaming a
note reads as "deleted here, created there". Left alone, the stale counterpart
regenerates the document at its old path and the file ends up at **both**
paths, permanently. rclone bisync cannot help: it has no rename tracking.

`markdownSync.trackMoves` (on by default) pairs the orphaned file with the
newly appeared one and moves it to match, in three passes:

- **identity** — the note's inode and birth time, which a rename keeps however
  much the note was edited. This catches a note that was renamed *and* edited.
- **basename** — survives a relocation, even if the note was edited in transit
- **mtime** — survives a rename, which changes the basename but not the
  timestamp

Only unambiguous 1:1 matches are acted on, and never onto an existing path.
Whole-folder moves work. The same identity check recovers a rename the watcher
only saw half of (`mkdir new && mv old new/`).

A move made in the vault is carried to the remote *before* the next pull as a
server-side move; if the remote cannot be reached the pull waits and the rename
is retried (and survives a restart), rather than being replayed as delete +
create. `syncDeletions` runs after move tracking, so only genuine deletions
reach it, and an empty or unmounted side is never propagated.

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

- `fix_case` — handle Drive's case-insensitive filesystem
- `slowHashSyncOnly` — limit checksum computation to files where size+modtime already match, avoiding expensive full-file hashes on every sync

> **Bisyncing native Google Docs needs the settle pass.** With `importFormats`
> set (the default), every uploaded `.docx` is converted into a *native Google
> Doc*. Native Docs report `Size: -1` and no checksum, so modtime is the only
> change signal bisync has for the remote — and Drive rewrites it itself when
> the conversion finishes, seconds after rclone recorded the modtime it asked
> for. Every upload therefore makes the *next* run see the remote as "changed"
> even though nobody touched it. Alone that is harmless. But if the local side
> also changed in that window, bisync sees both sides as changed, declares a
> conflict, and drops a `.conflictN` file — on every run, for as long as you
> keep editing locally. `settle` closes the window and is on by default; it
> also runs after a burst of pushed changes, once `settle.delay` has passed.

```nix
services.rclone-remotes.bisyncs.gdocs = {
  remote = "gdrive:";
  localPath = "/home/user/GoogleDrive";
  configFile = "/etc/rclone.conf";

  # settle, conflict.resolve = "newer" and conflict.loser = "delete" are
  # all defaults, so nothing extra is needed here. Prefer keeping losers?
  #   conflict.loser = "num";

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
once per run and continues, so `compare = "size,modtime,checksum"`
can stay as it is. Prefer `pathOverride` where it applies: real checksums are
what let bisync tell a genuine change from a file that merely has the same size
and a rewritten modtime.

## Excluding paths

`excludes` is a list of rclone `--exclude` patterns, applied to both mounts and
bisyncs (and honoured by the watcher exactly as rclone itself would, so an
excluded file is never pushed). It defaults to `[ "#recycle/**" ]` — the per-share recycle bin a
Synology keeps at the root of every shared folder, which holds exactly the
files somebody already decided to throw away. Add `"@eaDir/**"` if the same NAS
is indexing media into thumbnail directories.

Changing this on an established bisync pair needs a moment's care. rclone only
forces a `--resync` when a `--filters-file` changes, and these are plain
exclude patterns, so nothing forces one here. Newly excluded files drop out of
both listings at once, which bisync reads as "deleted on both sides" and
accepts without touching either disk — but if they come to more than half the
pair, `--max-delete` aborts the run instead:

```
ERROR : Safety abort: too many deletes (>50%, 3 of 4) on Path1 "...". Run with --force if desired.
```

Nothing is deleted when that happens; the run simply stops. Recover by
resyncing the pair: `rclone-remotes ctl --name <name> resync`. Note also that excluding a directory
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

### `bisyncs.<name>`

| Option | Type | Default | Description |
|--------|------|---------|-------------|
| `remote` | string | — | Rclone remote path |
| `localPath` | string | — | Local directory to sync |
| `configFile` | string or null | global default | Rclone config file path (`null` = rclone's default) |
| `user` | string | global default | User to run the service as |
| `group` | string | global default | Group for the service |
| `dirPerms` | string | `"0755"` | Directory permissions |
| `workdir` | string | `~/.cache/rclone/bisync` | Where bisync keeps its listings (the default is rclone's own, so existing pairs are not resynced) |
| `push.enable` | bool | `true` | Watch `localPath` and push changes as they happen |
| `push.debounce` | string | `"2s"` | How long the tree must be quiet before a burst of changes is pushed |
| `pull.interval` | string | `"15min"` | How often to pull remote changes |
| `pull.onBoot` | string | `"5min"` | Delay before the first pull after start |
| `pull.jitter` | string | `"5min"` | Random delay added to each pull |
| `conflict.resolve` | enum | `"newer"` | Which side wins a conflict |
| `conflict.loser` | enum | `"delete"` | What happens to the losing copy: `num`, `pathname` or `delete` |
| `compare` | string | `"size,modtime,checksum"` | How two files are judged equal |
| `resilient` / `recover` / `createEmptySrcDirs` | bool | `true` | The matching bisync options |
| `maxLock` | string | `"5m"` | How long a crashed run's lock is honoured |
| `maxDelete` | null or 0–100 | `null` | Abort if more than this percent would be deleted (null = rclone's 50) |
| `extraParams` | attrs | `{}` | Extra parameters for rclone's `sync/bisync`, merged over the above |
| `settle.enable` | bool | `true` | Run a second pass after a pull and after pushed changes, to reconcile remotes that rewrite modtimes after upload (see Google Drive above); turn off for SFTP/WebDAV/local |
| `settle.delay` | int | `30` | Seconds to wait before the second pass |
| `googleDrive.*`, `sftp.*`, `excludes` | | | As for mounts |
| `markdownSync.enable` | bool | `false` | Enable md↔docx conversion |
| `markdownSync.path` | path | — | Markdown/vault directory (must be separate from `localPath`) |
| `markdownSync.syncDeletions` | bool | `true` | Propagate deletions (without it, a deletion is undone on the next run) |
| `markdownSync.trackMoves` | bool | `true` | Follow moves/renames instead of duplicating them |
| `markdownSync.referenceDoc` | path or null | `null` | A docx whose styles a note's first conversion starts from |

Top level also has `package` (the `rclone-remotes` binary) and `rclonePackage`
(the rclone used for mounts and the daemon's private `rcd`).

### Upgrading from the script-based module

Bisync no longer runs `rclone bisync` from a timer; the options changed shape
to match. Old names keep working as shims and print a deprecation warning that
names the replacement:

| Old | New |
|-----|-----|
| `interval`, `onBootSec` | `pull.interval`, `pull.onBoot` |
| `conflictResolve`, `conflictLoser` | `conflict.resolve`, `conflict.loser` |
| `settlePass.enable`, `settlePass.delay` | `settle.enable`, `settle.delay` |

These no longer exist, and setting them is an error that says what to use
instead:

| Removed | Replacement |
|---------|-------------|
| `baseArgs` | the typed `compare`, `resilient`, `recover`, `createEmptySrcDirs`, `maxLock` |
| `extraArgs` | `extraParams` (rclone rc parameters, not CLI flags) |
| `markdownSync.mdToDocxArgs` | `markdownSync.referenceDoc` |
| `markdownSync.docxToMdArgs` | none: markdown is always written unwrapped |

Other things to know when upgrading:

- **No timer and no `-init` unit.** `rclone-bisync-<name>.service` is a single
  long-running service; `systemctl start` it to run it, `ctl sync` to pull now.
  A pair with existing listings is not resynced.
- **Local changes now arrive immediately.** If a pair should not do that,
  set `push.enable = false`.
- **pandoc is no longer needed** (or installed for this module).
- Earlier changes still apply: `conflict.loser` defaults to `delete` (set
  `"num"` to keep both copies), `markdownSync.syncDeletions` and
  `settle.enable` default to `true`, mounts no longer force
  `sftp.disableHashcheck`, and `excludes` defaults to `[ "#recycle/**" ]`.

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

## Binary cache

The `rclone-remotes` daemon is a Rust program, built from the `Cargo.lock` in
this repository. CI builds it for `x86_64-linux` and `aarch64-linux` and
publishes the result to a public Cachix cache, so consumers need not compile
it. The module is built with *your* `pkgs`, so cache hits need your nixpkgs to
be this flake's locked one:

```nix
inputs.rclone-remotes.url = "github:Avunu/nixos-rclone";
inputs.nixpkgs.follows = "rclone-remotes/nixpkgs";
```

The module enables the cache for you (`services.rclone-remotes.binaryCache.enable`,
on by default); the flake's `nixConfig` does the same for building the flake
itself. See `.github/workflows/checks.yml` for what CI pushes and why only from
`main`.

## Development

```
nix develop            # cargo, clippy, rustfmt, cargo-deny, rclone; installs git hooks
cargo test             # unit tests, plus integration tests against a real rclone
nix flake check        # package + tests, clippy, rustfmt, and the NixOS VM test
```

The integration tests (`tests/`) run the real daemon against a real
`rclone rcd` over local directories, so `rclone` must be on `PATH`.

## License

MIT
