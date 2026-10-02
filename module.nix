{
  config,
  options,
  pkgs,
  lib,
  utils,
  ...
}:

let
  inherit (lib)
    attrValues
    concatMap
    concatStringsSep
    filterAttrs
    getExe
    listToAttrs
    literalExpression
    mapAttrs
    mapAttrsToList
    mkEnableOption
    mkIf
    mkMerge
    mkRemovedOptionModule
    mkRenamedOptionModule
    mkOption
    nameValuePair
    optional
    optionalAttrs
    optionals
    types
    ;

  cfg = config.services.rclone-remotes;

  rcloneRemotes = getExe cfg.package;

  serviceEnvPackages = [
    pkgs.coreutils
    cfg.rclonePackage
  ];

  userHomeOf = user: config.users.users.${user}.home or "/home/${user}";

  # ── Shared option fragment: Google Drive ─────────────────────────────
  googleDriveOptions = {
    enable = mkEnableOption "Google Drive-specific options (export/import formats for Workspace files)";

    rootFolderId = mkOption {
      type = types.nullOr types.str;
      default = null;
      description = "Restrict to a specific Google Drive folder ID.";
      example = "14zaHa9I5dpMa4AaUTt_Mi7r2_AyT6654";
    };

    exportFormats = mkOption {
      type = types.str;
      default = "docx";
      description = "Comma-separated export formats for Google Workspace files (Docs→docx, etc.).";
    };

    importFormats = mkOption {
      type = types.str;
      default = "docx";
      description = "Comma-separated import formats when writing back to Google Drive.";
    };
  };

  # ── Shared option fragment: SFTP ─────────────────────────────────────
  sftpOptions = {
    pathOverride = mkOption {
      type = types.nullOr types.str;
      default = null;
      example = "@/volume1";
      description = ''
        Where the SSH shell sees the files that the SFTP session serves
        (`--sftp-path-override`).

        The SFTP backend has no hash primitive of its own: to checksum a file
        it opens a *second* SSH channel and runs `md5sum <path>` there. Any
        server that jails SFTP to a virtual root — a Synology or QNAP NAS, a
        chrooted OpenSSH account, a containerised SFTP service — hands the
        shell a different filesystem than the SFTP session, so that command
        fails on paths that transfer perfectly well:

        ```
        ERROR : Legal/Scholars Fund.pdf: Failed to calculate src hash:
          failed to calculate md5 hash: failed to run "md5sum /document/Legal/...":
          md5sum: '/document/Legal/...': No such file or directory
        ```

        Note that `md5sum` itself ran: the path simply does not exist outside
        the jail. This option supplies the translation.

        Prefix the value with `@` to give only the *root* and let rclone
        append the remote's own path — the setting then stays correct when
        that path changes. A share served as `remote:/document` that really
        lives at `/volume1/document` needs nothing more than `@/volume1`.
        Without the `@`, the value must spell out the full shell path
        corresponding to the remote's root, and has to be restated per remote
        path.

        Leave null when shell and SFTP agree on paths, as they do for an
        ordinary OpenSSH account over a real home directory.
      '';
    };

    disableHashcheck = mkOption {
      type = types.bool;
      default = false;
      description = ''
        Stop asking the server for checksums at all
        (`--sftp-disable-hashcheck`).

        The fallback for a server where `pathOverride` cannot help: no shell
        access, no `md5sum` on it, or a path mapping that is not a fixed
        prefix. Both sides are then left with no hash in common and rclone
        compares size and modtime instead — bisync says so once per run
        ("falling back to --compare modtime,size") and carries on, so
        `--compare size,modtime,checksum` in `baseArgs` can stay as it is.

        Prefer `pathOverride` where it applies: it keeps the checksums, and
        with them bisync's ability to tell a real change from a file that
        merely has the same size and a rewritten modtime.
      '';
    };
  };

  # ── Shared option: filters ────────────────────────────────────────────
  excludesOption = mkOption {
    type = types.listOf types.str;
    default = [
      ".AppleDouble"
      ".DS_Store"
      ".Spotlight-V100"
      ".Trashes"
      "@eaDir/**"
      "#recycle/**"
      "$RECYCLE.BIN/**"
      "Thumbs.db"
    ];
    example = [
      "#recycle/**"
      "@eaDir/**"
    ];
    description = ''
      Patterns to keep out of the transfer, passed as rclone `--exclude`.

      The default covers `#recycle`, the per-share recycle bin a Synology NAS
      keeps at the root of every shared folder: it holds exactly the files
      somebody already decided to throw away, and syncing it doubles their
      cost forever. A Synology also scatters `@eaDir` thumbnail directories
      through every folder — add `"@eaDir/**"` if you are indexing media.

      Note when changing this on an existing bisync pair: rclone only forces a
      `--resync` when a `--filters-file` changes, and these are plain
      `--exclude` flags, so nothing forces one here. Newly excluded files drop
      out of both listings at once, which bisync reads as "deleted on both
      sides" and accepts without touching either disk — but if they are more
      than half of the pair, `--max-delete` aborts the run instead ("Safety
      abort: too many deletes"). Recover by resyncing: remove the listings
      under `<home>/.cache/rclone/bisync/` and start
      `rclone-bisync-<name>-init.service`.

      Excluding a directory stops it syncing; it does not remove a copy an
      earlier run already made.
    '';
  };

  # ── Submodule: live FUSE mount ────────────────────────────────────────
  mountSubmodule = types.submodule {
    options = {
      remote = mkOption {
        type = types.str;
        description = "Rclone remote path, e.g. `myremote:path`.";
        example = "webdav:documents";
      };
      localPath = mkOption {
        type = types.path;
        description = "Absolute local path to mount into.";
      };
      configFile = mkOption {
        type = types.nullOr types.str;
        default = cfg.defaultConfigFile;
        description = "Path to the rclone config file (e.g. an agenix secret). Defaults to the owning user's ~/.config/rclone/rclone.conf when null.";
      };
      uid = mkOption {
        type = types.int;
        default = cfg.defaultUid;
        description = "UID for the FUSE mount.";
      };
      gid = mkOption {
        type = types.int;
        default = cfg.defaultGid;
        description = "GID for the FUSE mount.";
      };
      user = mkOption {
        type = types.str;
        default = cfg.defaultUser;
        description = "Owner for the tmpfiles directory rule and source of the default rclone config path.";
      };
      group = mkOption {
        type = types.str;
        default = cfg.defaultGroup;
        description = "Group for the tmpfiles directory rule.";
      };
      dirPerms = mkOption {
        type = types.str;
        default = "0755";
        description = "Permission mode for the local directory (tmpfiles).";
      };
      extraOpts = mkOption {
        type = types.listOf types.str;
        default = [ ];
        description = ''
          Extra mount options appended to the rclone mount, in `flag=value`
          form (translated to `--flag=value` by the rclone mount helper).
          Values must not contain commas.
        '';
      };

      googleDrive = googleDriveOptions;

      sftp = sftpOptions;

      excludes = excludesOption;
    };
  };

  # ── Submodule: bisync ─────────────────────────────────────────────────
  bisyncSubmodule = types.submodule (
    { config, ... }:
    {
      # Options that changed shape in the move to the daemon keep working, with
      # a deprecation warning pointing at the new name.
      imports = [
        (mkRenamedOptionModule [ "interval" ] [ "pull" "interval" ])
        (mkRenamedOptionModule [ "onBootSec" ] [ "pull" "onBoot" ])
        (mkRenamedOptionModule [ "conflictResolve" ] [ "conflict" "resolve" ])
        (mkRenamedOptionModule [ "conflictLoser" ] [ "conflict" "loser" ])
        (mkRenamedOptionModule [ "settlePass" "enable" ] [ "settle" "enable" ])
        (mkRenamedOptionModule [ "settlePass" "delay" ] [ "settle" "delay" ])
        (mkRemovedOptionModule [ "baseArgs" ] ''
          bisync no longer shells out to `rclone bisync`, so there is no flag list.
          Its defaults are now the typed options `compare`, `resilient`, `recover`,
          `createEmptySrcDirs` and `maxLock`.
        '')
        (mkRemovedOptionModule [ "markdownSync" "mdToDocxArgs" ] ''
          Conversion no longer shells out to pandoc, so there are no pandoc flags.
          To style new documents after a template use `markdownSync.referenceDoc`.
        '')
        (mkRemovedOptionModule [ "markdownSync" "docxToMdArgs" ] ''
          Conversion no longer shells out to pandoc, so there are no pandoc flags.
          Markdown is written unwrapped, as `--wrap=none` did.
        '')
        (mkRemovedOptionModule [ "extraArgs" ] ''
          bisync no longer shells out to `rclone bisync`. Pass extra rc parameters
          through `extraParams` instead, e.g. `extraParams.ignoreListingChecksum = true;`.
          Backend flags such as --drive-acknowledge-abuse have no per-pair
          equivalent yet.
        '')
      ];

      options = {
        # The shims above report through these; a submodule has no top-level
        # `warnings`/`assertions`, so they are collected in `config` below.
        warnings = mkOption {
          type = types.listOf types.str;
          default = [ ];
          internal = true;
          visible = false;
        };
        assertions = mkOption {
          type = types.listOf types.unspecified;
          default = [ ];
          internal = true;
          visible = false;
        };

        remote = mkOption {
          type = types.str;
          description = "Rclone remote path, e.g. `webdav:ssh`.";
        };
        localPath = mkOption {
          type = types.path;
          description = "Absolute local directory to sync.";
        };
        configFile = mkOption {
          type = types.nullOr types.str;
          default = cfg.defaultConfigFile;
          description = "Path to the rclone config file. Defaults to rclone's default (~/.config/rclone/rclone.conf) when null.";
        };
        user = mkOption {
          type = types.str;
          default = cfg.defaultUser;
          description = "User to run the sync as.";
        };
        group = mkOption {
          type = types.str;
          default = cfg.defaultGroup;
          description = "Group for the sync service.";
        };
        dirPerms = mkOption {
          type = types.str;
          default = "0755";
          description = "Permission mode for the local directory (tmpfiles).";
        };
        workdir = mkOption {
          type = types.str;
          default = "${userHomeOf config.user}/.cache/rclone/bisync";
          defaultText = literalExpression ''"''${home of user}/.cache/rclone/bisync"'';
          description = ''
            Where bisync keeps its listings. The default is rclone's own, so
            pairs created by earlier versions of this module keep their history
            and are not forced to resync.
          '';
        };

        push = {
          enable = mkOption {
            type = types.bool;
            default = true;
            description = ''
              Watch `localPath` and send local changes to the remote as they
              happen: new and modified files are uploaded, deletions are
              applied, and a rename becomes a server-side move there, so a
              Google Drive document keeps its file ID, sharing and history.
              The periodic `pull` remains the way remote changes arrive, and
              catches anything the watcher missed.

              Nothing is pushed until the pair has completed its first sync.
              Files whose names contain control characters are left to the
              pull. The watcher needs an inotify watch per directory, so a very
              large tree may need `boot.kernel.sysctl."fs.inotify.max_user_watches"`
              raised (NixOS defaults to 524288).
            '';
          };
          debounce = mkOption {
            type = types.str;
            default = "2s";
            description = ''
              How long the tree must be quiet before a burst of changes is
              pushed (systemd time-span syntax, at least 100ms). Longer values
              keep a file still being written from being uploaded half done.
            '';
          };
        };

        pull = {
          interval = mkOption {
            type = types.str;
            default = "15min";
            description = ''
              How often to pull remote changes, counted from the end of the last
              pull (systemd time-span syntax). rclone has no change notification
              to subscribe to, so remote changes are found by polling; local
              changes do not wait for this.
            '';
          };
          onBoot = mkOption {
            type = types.str;
            default = "5min";
            description = "Delay after the service starts before the first pull.";
          };
          jitter = mkOption {
            type = types.str;
            default = "5min";
            description = "Up to this much random delay is added to each pull, so pairs do not all hit the network at once.";
          };
        };

        conflict = {
          resolve = mkOption {
            type = types.enum [
              "none"
              "path1"
              "path2"
              "newer"
              "older"
              "larger"
              "smaller"
            ];
            default = "newer";
            description = ''
              How to pick the winner when a file changed on both sides
              (`--conflict-resolve`). rclone's own default is `none`, which keeps
              both; `newer` is a better fit for a periodic timer, where the side
              you touched most recently is almost always the one you meant.
            '';
          };

          loser = mkOption {
            type = types.enum [
              "num"
              "pathname"
              "delete"
            ];
            default = "delete";
            description = ''
              What to do with the losing copy of a conflict (`--conflict-loser`).

              - `num` — keep it as `file.docx.conflict1`, `.conflict2`, … This is
                rclone's own default and the conservative choice: nothing is ever
                discarded, at the cost of debris if conflicts are frequent.
              - `pathname` — keep it as `file.docx.path1` / `.path2`.
              - `delete` — discard it, keeping the winner only.

              Note that `delete` loses a version of a *genuine* simultaneous edit.
              It pairs well with `settle`, which removes the spurious conflicts
              that would otherwise dominate; but if conflicts are rare in your
              setup, they are more likely to be real, and `num` is the safer pick.
            '';
          };
        };

        compare = mkOption {
          type = types.str;
          default = "size,modtime,checksum";
          description = "How bisync decides two files are the same (`compare`).";
        };

        resilient = mkOption {
          type = types.bool;
          default = true;
          description = "Retry the next run after a recoverable error instead of demanding a resync (`resilient`).";
        };

        recover = mkOption {
          type = types.bool;
          default = true;
          description = "Recover from an interrupted run using the backup listings (`recover`).";
        };

        createEmptySrcDirs = mkOption {
          type = types.bool;
          default = true;
          description = "Keep empty directories in sync (`createEmptySrcDirs`).";
        };

        maxLock = mkOption {
          type = types.str;
          default = "5m";
          description = "How long a crashed run's lock is honoured before it is considered stale (`maxLock`).";
        };

        maxDelete = mkOption {
          type = types.nullOr (types.ints.between 0 100);
          default = null;
          description = ''
            Abort if more than this percentage of files would be deleted
            (`maxDelete`). Null uses rclone's own default of 50.
          '';
        };

        extraParams = mkOption {
          type = types.attrsOf types.anything;
          default = { };
          example = {
            ignoreListingChecksum = true;
          };
          description = ''
            Additional parameters for rclone's `sync/bisync` call, merged over the
            options above. See the rclone rc documentation for the full list.
          '';
        };

        settle = {
          enable = mkOption {
            type = types.bool;
            default = true;
            description = ''
              Run a second bisync pass, to reconcile remotes that rewrite modtimes
              after an upload.

              Google Drive does this whenever `importFormats` converts an
              uploaded file into a native Google Doc: the conversion finishes
              asynchronously and stamps the Doc's `modifiedTime` with the
              conversion time, seconds after rclone has already recorded the
              modtime it asked for. The next run therefore sees Path2 as "changed"
              even though nobody touched the remote. On its own that is harmless —
              bisync just pulls the file back down — but if the local side changed
              in the same window, bisync sees both sides as changed and declares a
              conflict, spraying `.conflictN` files on every run.

              The second pass closes that window: it runs after the first has
              uploaded, so it pulls the restamped remote copy back down and the
              listings converge *within* the run instead of colliding at the next
              one. The markdown conversion runs once, around both passes, so the
              local side cannot change between them and the second pass cannot
              itself conflict.

              On by default, because getting this wrong corrupts a pair quietly.
              Turn it off for remotes that store modtimes faithfully — SFTP,
              WebDAV, plain local paths — where the extra pass buys nothing and
              costs a full second listing plus `delay` seconds on every run.
            '';
          };

          delay = mkOption {
            type = types.int;
            default = 30;
            description = ''
              Seconds to wait between the two passes, to give the remote time to
              finish rewriting modtimes. Observed Google Drive conversion lag is
              5-10s; the default leaves generous headroom.
            '';
          };
        };

        googleDrive = googleDriveOptions;

        sftp = sftpOptions;

        excludes = excludesOption;

        markdownSync = {
          enable = mkEnableOption "bidirectional markdown/docx sync";

          path = mkOption {
            type = types.nullOr types.path;
            default = null;
            description = ''
              Path to the markdown directory (e.g. Obsidian vault). Markdown files
              here are converted to docx in localPath before sync, and docx files
              synced from the remote are converted back after sync.
            '';
            example = "/home/user/ObsidianVault";
          };

          syncDeletions = mkOption {
            type = types.bool;
            default = true;
            description = ''
              Propagate deletions between the markdown and docx directories.

              On by default: without it a deletion never sticks. The surviving
              counterpart simply regenerates the document at its old path on the
              next run, the same resurrection that `trackMoves` fixes for moves.

              `trackMoves` runs first and consumes relocations, so only genuine
              deletions reach this pass, and a side that is empty or unmounted is
              skipped rather than propagated. It does still delete files, though —
              set it to `false` if you would rather let orphans accumulate.
            '';
          };

          trackMoves = mkOption {
            type = types.bool;
            default = true;
            description = ''
              Follow moves and renames instead of duplicating them.

              The two trees are matched by path, so relocating a file on one side
              reads as "deleted here, created there" on the other, and the stale
              counterpart regenerates the document at its old path on the next
              run — leaving it at both paths, in both trees, permanently. rclone
              bisync cannot help here: it has no rename tracking and models every
              move as delete + create.

              With this on, an orphaned file is paired with a newly-appeared one
              (by inode, basename or mtime) and moved to match. Only unambiguous
              1:1 pairings are followed; anything else is logged and left alone.

              A move made in the markdown directory is also performed on the
              remote, as a server-side move, so a Google Drive document keeps its
              file ID, sharing and history instead of being replaced by a new
              upload.
            '';
          };

          referenceDoc = mkOption {
            type = types.nullOr types.path;
            default = null;
            example = "/home/user/template.docx";
            description = ''
              A docx whose styles (fonts, headings, spacing) a note's *first*
              conversion starts from. After that, each note's own docx is the
              reference, so formatting applied to it in Google Docs or Word
              survives later edits of the note.
            '';
          };
        };
      };
    }
  );

  # ── Builders ──────────────────────────────────────────────────────────

  mkSftpMountOpts =
    m:
    optional m.sftp.disableHashcheck "sftp-disable-hashcheck"
    ++ optional (m.sftp.pathOverride != null) "sftp-path-override=${m.sftp.pathOverride}";

  mkExcludeMountOpts = m: map (pat: "exclude=${pat}") m.excludes;

  mkGDriveMountOpts =
    m:
    optionals m.googleDrive.enable (
      [
        "drive-export-formats=${m.googleDrive.exportFormats}"
        "drive-import-formats=${m.googleDrive.importFormats}"
      ]
      ++ optional (
        m.googleDrive.rootFolderId != null
      ) "drive-root-folder-id=${m.googleDrive.rootFolderId}"
    );

  # ── Mounts ────────────────────────────────────────────────────────────

  credMounts = filterAttrs (_name: m: m.configFile != null) cfg.mounts;

  # Writable staging copy so rclone can persist config changes (token
  # refreshes, etc.) that it cannot write to a read-only secret.
  stagingDir = "/run/rclone";
  stagedConfigPath = name: "${stagingDir}/${name}.conf";

  # A bisync unit receives its config as a credential and the daemon copies it
  # to its own RuntimeDirectory, a writable place rclone can persist OAuth token
  # refreshes into (LoadCredential's directory is read-only, which makes every
  # refresh fail with "Failed to save config after 10 tries").

  # What the rclone-remotes binary reads. Its schema (src/config.rs) rejects
  # unknown fields, and `validate` runs at build time (see system.checks), so a
  # field added on one side only fails nixos-rebuild rather than being ignored.
  daemonConfig = pkgs.writeText "rclone-remotes.json" (
    builtins.toJSON {
      kind = "global";
      version = 1;
      inherit stagingDir;
      mounts = mapAttrs (_name: m: {
        localPath = toString m.localPath;
        unit = utils.escapeSystemdPath m.localPath;
        configFile = m.configFile;
      }) cfg.mounts;
      mountReset.delay = cfg.mountResetDelay;
    }
  );

  pairConfig =
    name: s:
    pkgs.writeText "rclone-remotes-${name}.json" (
      builtins.toJSON {
        kind = "pair";
        version = 1;
        inherit name;
        rclone = getExe cfg.rclonePackage;
        inherit (s) remote workdir excludes;
        localPath = toString s.localPath;
        configCredential = s.configFile != null;
        googleDrive =
          if s.googleDrive.enable then
            {
              inherit (s.googleDrive) exportFormats importFormats rootFolderId;
            }
          else
            null;
        sftp = {
          inherit (s.sftp) pathOverride disableHashcheck;
        };
        pull = {
          inherit (s.pull) interval onBoot jitter;
        };
        conflict = {
          inherit (s.conflict) resolve loser;
        };
        bisync = {
          inherit (s)
            compare
            resilient
            recover
            createEmptySrcDirs
            maxLock
            maxDelete
            extraParams
            ;
        };
        settle = {
          inherit (s.settle) enable delay;
        };
        push = {
          inherit (s.push) enable debounce;
        };
        markdownSync =
          if s.markdownSync.enable then
            {
              path = toString s.markdownSync.path;
              inherit (s.markdownSync) syncDeletions trackMoves referenceDoc;
            }
          else
            null;
      }
    );

  validatedDaemonConfig =
    pkgs.runCommand "rclone-remotes-config-check" { nativeBuildInputs = [ cfg.package ]; }
      ''
        rclone-remotes validate --config ${daemonConfig}
        ${concatStringsSep "\n" (
          mapAttrsToList (name: s: "rclone-remotes validate --config ${pairConfig name s}") cfg.bisyncs
        )}
        touch $out
      '';

  # The rclone mount helper (mount.rclone, via system.fsPackages) translates
  # `opt=value` mount options into `--opt=value` flags. systemd runs mount
  # helpers with an empty environment (no HOME/PATH), so config= and
  # cache-dir= must be explicit absolute paths.
  mkFilesystem =
    name: m:
    let
      effectiveConfig =
        if m.configFile != null then
          stagedConfigPath name
        else
          "${userHomeOf m.user}/.config/rclone/rclone.conf";
    in
    {
      device = m.remote;
      mountPoint = m.localPath;
      fsType = "rclone";
      noCheck = true;
      options = [
        # Systemd mount architecture
        "noauto"
        "x-systemd.automount"
        "_netdev"
        "x-systemd.idle-timeout=600"
        "x-systemd.mount-timeout=120s"
        "x-systemd.requires=network-online.target"
        "x-systemd.after=network-online.target"

        # Rclone core operations
        "rw"
        "allow_other"
        "uid=${toString m.uid}"
        "gid=${toString m.gid}"
        "umask=022"
        "config=${effectiveConfig}"
        "cache-dir=/var/cache/rclone/${name}"
        "vfs-cache-mode=full"
        "dir-cache-time=5m"
        "vfs-cache-max-age=24h"

        # A mount runs --daemon, so rclone's stdout/stderr go nowhere: without
        # this, every error it raises after the mount is up is lost. That is
        # exactly the wrong thing to lose, because with vfs-cache-mode=full the
        # upload happens *after* the writing program has already closed the file
        # and walked away — nothing is left to return the error to. A failing
        # writeback is therefore completely silent, and the mount keeps serving
        # the local cache copy, so the file still looks present and correct on
        # the mount while the remote never receives it.
        #
        # One such failure (a wrong --sftp-path-override, which makes md5sum
        # return an empty hash that rclone reads as "corrupted on transfer")
        # deleted every upload to a share and retried on the writeback cycle for
        # twenty months undetected, leaving 33k discarded .partial files in the
        # NAS recycle bin as the only evidence. Keep the default NOTICE level:
        # ERROR-level events still reach the journal, without logging traffic.
        "syslog"

        # Network & performance safeguards
        "transfers=4"
        "multi-thread-streams=4"
        "timeout=1m"

        # Chunked streaming optimization
        "vfs-read-chunk-size=64M"
        "vfs-read-chunk-size-limit=512M"
        "buffer-size=64M"
      ]
      ++ mkSftpMountOpts m
      ++ mkExcludeMountOpts m
      ++ optionals (m.configFile != null) [
        "x-systemd.requires=rclone-config.service"
        "x-systemd.after=rclone-config.service"
      ]
      ++ mkGDriveMountOpts m
      ++ m.extraOpts;
    };

  # ── Bisync services ───────────────────────────────────────────────────

  # One long-running daemon per pair (replacing the oneshot + timer + init
  # trio): it owns a private rclone rcd, pulls on its own schedule, and answers
  # `rclone-remotes ctl --name <name>`.
  mkPairService =
    name: s:
    nameValuePair "rclone-bisync-${name}" {
      description = "Rclone bisync for ${name}";
      wantedBy = [ "multi-user.target" ];
      after = [ "network-online.target" ];
      wants = [ "network-online.target" ];
      restartTriggers = [ (pairConfig name s) ];
      path = serviceEnvPackages;
      serviceConfig = {
        Type = "notify";
        NotifyAccess = "main";
        User = s.user;
        Group = s.group;
        ExecStart = "${rcloneRemotes} run --config ${pairConfig name s}";
        LoadCredential = optional (s.configFile != null) "rclone.conf:${s.configFile}";
        # Holds the rc and control sockets and the writable config copy; only
        # the unit's own user may enter.
        RuntimeDirectory = "rclone-remotes/${name}";
        RuntimeDirectoryMode = "0700";
        # What must survive a restart: renames not yet pushed, and which files
        # were pushed since the last pull.
        StateDirectory = "rclone-remotes/${name}";
        StateDirectoryMode = "0700";
        # rcd is a child of the daemon; a hung rcd stops the watchdog pings.
        WatchdogSec = "5min";
        # A failure the daemon cannot handle itself (rcd died) is retried; a
        # sync that fails is the daemon's own business and does not exit.
        Restart = "on-failure";
        RestartSec = "30s";
        TimeoutStopSec = "30s";
      };
    };

  mkTmpfile = _name: r: "d '${r.localPath}' ${r.dirPerms} ${r.user} ${r.group} -";

in
{
  options.services.rclone-remotes = {

    enable = mkEnableOption "rclone remote mounts and bisync services";

    package = mkOption {
      type = types.package;
      default = pkgs.callPackage ./nix/package.nix { };
      defaultText = literalExpression "pkgs.callPackage ./nix/package.nix { }";
      description = ''
        The `rclone-remotes` binary that supervises bisync pairs, stages mount
        configs and recovers stale mounts. Built with the consumer's own `pkgs`,
        so binary-cache hits need the consumer's nixpkgs to match this flake's
        locked one.
      '';
    };

    binaryCache.enable = mkOption {
      type = types.bool;
      default = true;
      description = ''
        Fetch the `rclone-remotes` daemon from this project's public Cachix
        cache (`nixos-rclone.cachix.org`) instead of compiling it. Lower
        priority than cache.nixos.org. Hits need your nixpkgs to be this
        flake's locked one (`nixpkgs.follows = "rclone-remotes/nixpkgs"`).
      '';
    };

    rclonePackage = mkOption {
      type = types.package;
      default = pkgs.rclone;
      defaultText = literalExpression "pkgs.rclone";
      description = ''
        The rclone used for FUSE mounts and, as a private `rclone rcd`, by every
        bisync pair. The rc API the daemon speaks is that of rclone 1.75; a
        different minor version works but is logged.
      '';
    };

    # ── Global defaults ─────────────────────────────────────────────────
    defaultConfigFile = mkOption {
      type = types.nullOr types.str;
      default = null;
      description = "Default rclone config file when a remote doesn't specify one. Null means use rclone's default (~/.config/rclone/rclone.conf).";
    };

    defaultUser = mkOption {
      type = types.str;
      default = "root";
      description = "Default user for mounts / syncs.";
    };

    defaultGroup = mkOption {
      type = types.str;
      default = "users";
      description = "Default group for mounts / syncs.";
    };

    defaultUid = mkOption {
      type = types.int;
      default = 1000;
      description = "Default UID passed to the FUSE mount.";
    };

    defaultGid = mkOption {
      type = types.int;
      default = 100;
      description = "Default GID passed to the FUSE mount.";
    };

    # ── Per-remote definitions ──────────────────────────────────────────
    mounts = mkOption {
      type = types.attrsOf mountSubmodule;
      default = { };
      description = "Attribute set of live rclone FUSE mounts (systemd automount).";
    };

    bisyncs = mkOption {
      type = types.attrsOf bisyncSubmodule;
      default = { };
      description = "Attribute set of rclone bisync pairs: remote changes are pulled periodically.";
    };

    # ── Suspend / resume reset ──────────────────────────────────────────
    enableMountReset = mkOption {
      type = types.bool;
      default = true;
      description = "Reset failed/stale rclone mounts after suspend/hibernate resume.";
    };

    mountResetDelay = mkOption {
      type = types.int;
      default = 15;
      description = "Seconds to wait after resume before resetting mounts (network stabilisation).";
    };
  };

  config = mkIf cfg.enable (mkMerge [
    # The qemu-vm module (NixOS tests, nixos-rebuild build-vm) overrides
    # fileSystems wholesale with mkVMOverride (priority 10), which would
    # silently discard the module's mounts. Re-state them through
    # virtualisation.fileSystems when that option exists. optionalAttrs (not
    # mkIf) because referencing a nonexistent option errors even under mkIf.
    (optionalAttrs (options ? virtualisation.fileSystems) {
      virtualisation.fileSystems = mapAttrs mkFilesystem cfg.mounts;
    })
    {

      assertions =
        mapAttrsToList (name: s: {
          assertion = s.markdownSync.enable -> s.markdownSync.path != null;
          message = "services.rclone-remotes.bisyncs.${name}.markdownSync.path must be set when markdownSync is enabled";
        }) cfg.bisyncs
        ++ concatMap (s: s.assertions) (attrValues cfg.bisyncs);

      warnings = concatMap (s: s.warnings) (attrValues cfg.bisyncs);

      system.checks = [ validatedDaemonConfig ];

      nix.settings = mkIf cfg.binaryCache.enable {
        substituters = [ "https://nixos-rclone.cachix.org?priority=41" ];
        trusted-public-keys = [ "nixos-rclone.cachix.org-1:y67XDcu9PSJL5n6GnU3Ju+PPCZh3JETrfwpxm9AndzE=" ];
      };

      environment.systemPackages = [
        cfg.rclonePackage
        cfg.package
      ];

      # Provides the mount.rclone helper used by mount(8) for fsType "rclone".
      system.fsPackages = [ cfg.rclonePackage ];

      fileSystems = mapAttrs mkFilesystem cfg.mounts;

      systemd.services =
        listToAttrs (mapAttrsToList mkPairService cfg.bisyncs)
        // optionalAttrs (credMounts != { }) {
          # Stage credential-backed configs into a writable location: .mount
          # units cannot use LoadCredential, and rclone wants to persist token
          # refreshes, which a read-only secret would reject on every refresh.
          rclone-config = {
            description = "Stage rclone configs for credential-backed mounts";
            restartTriggers = [
              daemonConfig
            ]
            ++ mapAttrsToList (_name: m: m.configFile) credMounts;
            serviceConfig = {
              Type = "oneshot";
              # Keep the unit active so RuntimeDirectory survives while mounts
              # are using the staged configs.
              RemainAfterExit = true;
              RuntimeDirectory = "rclone";
              RuntimeDirectoryMode = "0700";
              ExecStart = "${rcloneRemotes} stage-mount-configs --config ${daemonConfig}";
            };
          };
        }
        // optionalAttrs (cfg.enableMountReset && cfg.mounts != { }) {
          rclone-mount-reset = {
            description = "Reset failed rclone mounts after resume";
            after = [
              "suspend.target"
              "hibernate.target"
              "hybrid-sleep.target"
              "network-online.target"
            ];
            wants = [ "network-online.target" ];
            # mount-reset runs `systemctl reset-failed`.
            path = [ pkgs.systemd ];
            wantedBy = [
              "suspend.target"
              "hibernate.target"
              "hybrid-sleep.target"
            ];
            serviceConfig = {
              Type = "oneshot";
              ExecStart = "${rcloneRemotes} mount-reset --config ${daemonConfig}";
            };
          };
        };

      systemd.tmpfiles.rules =
        (mapAttrsToList mkTmpfile cfg.mounts)
        ++ (mapAttrsToList mkTmpfile cfg.bisyncs)
        # VFS cache: systemd runs mount helpers without HOME, so each mount
        # gets an explicit cache dir.
        ++ (mapAttrsToList (name: _m: "d /var/cache/rclone/${name} 0700 root root -") cfg.mounts);
    }
  ]);
}
