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
    concatMap
    concatStringsSep
    escapeShellArg
    escapeShellArgs
    filterAttrs
    flatten
    getExe
    listToAttrs
    mapAttrs
    mapAttrsToList
    mkEnableOption
    mkIf
    mkMerge
    mkOption
    nameValuePair
    optional
    optionalAttrs
    optionals
    optionalString
    removePrefix
    types
    ;

  cfg = config.services.rclone-remotes;

  filters = import ./filters pkgs;

  serviceEnvPackages = [
    pkgs.coreutils
    pkgs.rclone
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
  bisyncSubmodule = types.submodule {
    options = {
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
      interval = mkOption {
        type = types.str;
        default = "15min";
        description = "How often to re-sync after the last run completes (OnUnitActiveSec).";
      };
      onBootSec = mkOption {
        type = types.str;
        default = "5min";
        description = "Delay after boot before the first sync.";
      };
      baseArgs = mkOption {
        type = types.listOf types.str;
        default = [
          "--verbose"
          "--resilient"
          "--recover"
          "--create-empty-src-dirs"
          "--max-lock"
          "5m"
          "--compare"
          "size,modtime,checksum"
        ];
        description = ''
          Base arguments passed to `rclone bisync`. Replace this only to drop or
          change one of the defaults; to *add* arguments use `extraArgs`, which
          is appended on top.

          Conflict handling lives in `conflictResolve`/`conflictLoser` rather
          than here, so that changing it does not mean restating this list.
        '';
      };

      extraArgs = mkOption {
        type = types.listOf types.str;
        default = [ ];
        description = ''
          Additional arguments appended to `rclone bisync`, on top of
          `baseArgs` and the conflict and Google Drive flags.
        '';
        example = [ "--drive-acknowledge-abuse" ];
      };

      conflictResolve = mkOption {
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

      conflictLoser = mkOption {
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
          It pairs well with `settlePass`, which removes the spurious conflicts
          that would otherwise dominate; but if conflicts are rare in your
          setup, they are more likely to be real, and `num` is the safer pick.
        '';
      };

      settlePass = {
        enable = mkOption {
          type = types.bool;
          default = true;
          description = ''
            Run a second bisync pass, to reconcile remotes that rewrite modtimes
            after an upload.

            Google Drive does this whenever `--drive-import-formats` converts an
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
            one. Pre/post-sync hooks run once each, around both passes, so the
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
            of the same basename and moved to match. Only unambiguous 1:1
            pairings are followed; anything else is logged and left alone.
          '';
        };

        mdToDocxArgs = mkOption {
          type = types.listOf types.str;
          default = [ ];
          description = "Extra arguments for markdown to docx conversion.";
          example = [ "--reference-doc=/path/to/template.docx" ];
        };

        docxToMdArgs = mkOption {
          type = types.listOf types.str;
          default = [ "--wrap=none" ];
          description = "Extra arguments for docx to markdown conversion.";
        };
      };
    };
  };

  # ── Markdown sync helpers ─────────────────────────────────────────────

  # Shared by the pre- and post-sync hooks; see mirror-moves.sh for the why.
  # Kept in a plain .sh file rather than inline so it can be sourced directly by
  # checks.move-tracking, and so it is not written through Nix string escaping.
  mirrorMovesFn = builtins.readFile ./mirror-moves.sh;

  mkMarkdownPreSync =
    name: syncConfig:
    let
      mdDir = syncConfig.markdownSync.path;
      docxDir = syncConfig.localPath;
      pandocBin = getExe pkgs.pandoc;
      mdToDocxArgs = escapeShellArgs syncConfig.markdownSync.mdToDocxArgs;
    in
    pkgs.writeShellScript "markdown-pre-sync-${name}" ''
      set -euo pipefail
      shopt -s globstar nullglob

      md_dir=${escapeShellArg mdDir}
      docx_dir=${escapeShellArg docxDir}

      ${optionalString syncConfig.markdownSync.trackMoves ''
        ${mirrorMovesFn}
        # Vault is authoritative for paths here: follow md moves with the docx,
        # so bisync sees a move as delete+create rather than create-only.
        mirror_moves "$md_dir" .md "$docx_dir" .docx
      ''}

      md_files=("$md_dir"/**/*.md)

      for mdfile in "''${md_files[@]}"; do
        relpath="''${mdfile#"$md_dir"/}"
        docxfile="$docx_dir/''${relpath%.md}.docx"

        if [ ! -f "$docxfile" ] || [ "$mdfile" -nt "$docxfile" ]; then
          mkdir -p "$(dirname "$docxfile")"
          ref_args=()
          if [ -f "$docxfile" ]; then
            ref_args=("--reference-doc=$docxfile")
          fi
          ${pandocBin} "$mdfile" --from=markdown+lists_without_preceding_blankline --wrap=preserve --filter ${filters.md2docx}/bin/md2docx "''${ref_args[@]}" -o "$docxfile" ${mdToDocxArgs}
          touch -r "$mdfile" "$docxfile"
        fi
      done

      ${optionalString syncConfig.markdownSync.syncDeletions ''
        docx_files=("$docx_dir"/**/*.docx)
        # Safety guard: an unmounted/empty markdown dir must not wipe every
        # docx (and then propagate mass deletion to the remote).
        if [ ''${#md_files[@]} -eq 0 ] && [ ''${#docx_files[@]} -gt 0 ]; then
          echo "markdown dir '$md_dir' is missing or empty; skipping deletion pass" >&2
        else
          for docxfile in "''${docx_files[@]}"; do
            relpath="''${docxfile#"$docx_dir"/}"
            mdfile="$md_dir/''${relpath%.docx}.md"
            if [ ! -f "$mdfile" ]; then
              rm "$docxfile"
            fi
          done
        fi
      ''}
    '';

  mkMarkdownPostSync =
    name: syncConfig:
    let
      mdDir = syncConfig.markdownSync.path;
      docxDir = syncConfig.localPath;
      pandocBin = getExe pkgs.pandoc;
      docxToMdArgs = escapeShellArgs syncConfig.markdownSync.docxToMdArgs;
    in
    pkgs.writeShellScript "markdown-post-sync-${name}" ''
      set -euo pipefail
      shopt -s globstar nullglob

      md_dir=${escapeShellArg mdDir}
      docx_dir=${escapeShellArg docxDir}

      ${optionalString syncConfig.markdownSync.trackMoves ''
        ${mirrorMovesFn}
        # bisync has just applied the remote's moves to the docx tree, so the
        # docx side is authoritative for paths here: follow them with the md.
        mirror_moves "$docx_dir" .docx "$md_dir" .md
      ''}

      docx_files=("$docx_dir"/**/*.docx)

      for docxfile in "''${docx_files[@]}"; do
        relpath="''${docxfile#"$docx_dir"/}"
        mdfile="$md_dir/''${relpath%.docx}.md"

        if [ ! -f "$mdfile" ] || [ "$docxfile" -nt "$mdfile" ]; then
          mkdir -p "$(dirname "$mdfile")"
          ${pandocBin} "$docxfile" --filter ${filters.docx2md}/bin/docx2md -o "$mdfile" ${docxToMdArgs}
          touch -r "$docxfile" "$mdfile"
        fi
      done

      ${optionalString syncConfig.markdownSync.syncDeletions ''
        md_files=("$md_dir"/**/*.md)
        # Safety guard: an empty docx dir must not wipe the markdown vault.
        if [ ''${#docx_files[@]} -eq 0 ] && [ ''${#md_files[@]} -gt 0 ]; then
          echo "docx dir '$docx_dir' is missing or empty; skipping deletion pass" >&2
        else
          for mdfile in "''${md_files[@]}"; do
            relpath="''${mdfile#"$md_dir"/}"
            docxfile="$docx_dir/''${relpath%.md}.docx"
            if [ ! -f "$docxfile" ]; then
              rm "$mdfile"
            fi
          done
        fi
      ''}
    '';

  # ── Builders ──────────────────────────────────────────────────────────

  mkSftpMountOpts =
    m:
    optional m.sftp.disableHashcheck "sftp-disable-hashcheck"
    ++ optional (m.sftp.pathOverride != null) "sftp-path-override=${m.sftp.pathOverride}";

  mkSftpArgs =
    s:
    optional s.sftp.disableHashcheck "--sftp-disable-hashcheck"
    ++ optionals (s.sftp.pathOverride != null) [
      "--sftp-path-override"
      s.sftp.pathOverride
    ];

  mkExcludeMountOpts = m: map (pat: "exclude=${pat}") m.excludes;

  mkExcludeArgs =
    s:
    concatMap (pat: [
      "--exclude"
      pat
    ]) s.excludes;

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

  mkGDriveArgs =
    s:
    optionals s.googleDrive.enable (
      [
        "--drive-export-formats"
        s.googleDrive.exportFormats
        "--drive-import-formats"
        s.googleDrive.importFormats
        "--fix-case"
        "--slow-hash-sync-only"
      ]
      ++ optional (
        s.googleDrive.rootFolderId != null
      ) "--drive-root-folder-id=${s.googleDrive.rootFolderId}"
    );

  # Derive the listing filename rclone bisync uses under <home>/.cache/rclone/bisync/.
  # Must be a literal path: %h in system units resolves to the service
  # *manager's* home (/root), not the User= of the unit.
  # Caveat: rclone canonicalizes the remote before naming the listings, so
  # for `alias` remotes (which resolve to their target) this derivation won't
  # match and the initial resync would re-run on every sync.
  bisyncListingPath =
    s:
    let
      sanitize = p: builtins.replaceStrings [ ":" "/" " " ] [ "_" "_" "_" ] (removePrefix "/" p);
    in
    "${userHomeOf s.user}/.cache/rclone/bisync/${sanitize s.localPath}..${sanitize s.remote}.path1.lst";

  # ── Mounts ────────────────────────────────────────────────────────────

  credMounts = filterAttrs (_name: m: m.configFile != null) cfg.mounts;
  credBisyncs = filterAttrs (_name: s: s.configFile != null) cfg.bisyncs;

  # Writable staging copy so rclone can persist config changes (token
  # refreshes, etc.) that it cannot write to a read-only secret.
  stagedConfigPath = name: "/run/rclone/${name}.conf";

  # Bisyncs get a private directory rather than a bare file: rclone rewrites a
  # config by creating a temp file *alongside* it and renaming, so the parent
  # directory has to be writable by the unit's User= too. LoadCredential can't
  # serve this — $CREDENTIALS_DIRECTORY is read-only, which makes every OAuth
  # token refresh fail with "Failed to save config after 10 tries".
  stagedBisyncDir = name: "/run/rclone/bisync-${name}";
  stagedBisyncConfigPath = name: "${stagedBisyncDir name}/rclone.conf";

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

  mkBisyncExec =
    name: scriptName: s: initArgs:
    let
      argv = [
        (getExe pkgs.rclone)
        "bisync"
        s.localPath
        s.remote
      ]
      ++ initArgs
      ++ s.baseArgs
      ++ [
        "--conflict-resolve"
        s.conflictResolve
        "--conflict-loser"
        s.conflictLoser
      ]
      ++ mkSftpArgs s
      ++ mkExcludeArgs s
      ++ mkGDriveArgs s
      ++ s.extraArgs
      ++ optionals (s.configFile != null) [
        "--config"
        (stagedBisyncConfigPath name)
      ];
    in
    pkgs.writeShellScript scriptName ''
      exec ${escapeShellArgs argv}
    '';

  mkBisyncInitService =
    name: s:
    nameValuePair "rclone-bisync-${name}-init" {
      description = "Initial resync for rclone bisync ${name}";
      after = [ "network-online.target" ] ++ optional (s.configFile != null) "rclone-config.service";
      wants = [ "network-online.target" ];
      requires = optional (s.configFile != null) "rclone-config.service";
      # requiredBy (not wantedBy): a failed initial resync must block the
      # main sync instead of letting it fail confusingly on missing listings.
      requiredBy = [ "rclone-bisync-${name}.service" ];
      before = [ "rclone-bisync-${name}.service" ];
      unitConfig.ConditionPathExists = "!${bisyncListingPath s}";
      path = serviceEnvPackages;
      serviceConfig = {
        Type = "oneshot";
        User = s.user;
        Group = s.group;
        ExecStartPre = "${pkgs.coreutils}/bin/mkdir -p ${escapeShellArg s.localPath}";
        ExecStart = mkBisyncExec name "rclone-bisync-${name}-init" s [
          "--resync"
          "--resync-mode"
          "newer"
        ];
      };
    };

  mkBisyncService =
    name: s:
    let
      bisyncExec = mkBisyncExec name "rclone-bisync-${name}" s [ ];
    in
    nameValuePair "rclone-bisync-${name}" {
      description = "Rclone bisync for ${name}";
      after = [ "network-online.target" ] ++ optional (s.configFile != null) "rclone-config.service";
      wants = [ "network-online.target" ];
      requires = optional (s.configFile != null) "rclone-config.service";
      path = flatten [
        serviceEnvPackages
        (optionals s.markdownSync.enable [ pkgs.pandoc ])
      ];
      serviceConfig = {
        Type = "oneshot";
        User = s.user;
        Group = s.group;
        ExecStartPre = [
          "${pkgs.coreutils}/bin/mkdir -p ${escapeShellArg s.localPath}"
        ]
        ++ optional s.markdownSync.enable "${mkMarkdownPreSync name s}";
        # Type=oneshot runs multiple ExecStart= lines in sequence, and the
        # Pre/Post hooks bracket all of them — so the markdown conversion still
        # happens exactly once per run, with both bisync passes inside it.
        # The settle pass is prefixed "-" (non-fatal): it is an optimisation,
        # and a transient failure there must not fail a run whose first pass
        # succeeded, nor skip the post-sync conversion.
        ExecStart = [ "${bisyncExec}" ]
        ++ optionals s.settlePass.enable [
          "${pkgs.coreutils}/bin/sleep ${toString s.settlePass.delay}"
          "-${bisyncExec}"
        ];
        ExecStartPost = optional s.markdownSync.enable "${mkMarkdownPostSync name s}";
        # No Restart=: the timer is the retry mechanism. A bisync failure
        # that needs --resync would otherwise loop uselessly.
      };
    };

  mkBisyncTimer =
    name: s:
    nameValuePair "rclone-bisync-${name}" {
      description = "Timer for rclone bisync ${name}";
      wantedBy = [ "timers.target" ];
      timerConfig = {
        OnBootSec = s.onBootSec;
        OnUnitActiveSec = s.interval;
        RandomizedDelaySec = "5m";
      };
    };

  mkTmpfile = _name: r: "d '${r.localPath}' ${r.dirPerms} ${r.user} ${r.group} -";

in
{
  options.services.rclone-remotes = {

    enable = mkEnableOption "rclone remote mounts and bisync services";

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
      description = "Attribute set of rclone bisync pairs (periodic two-way sync).";
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

      assertions = mapAttrsToList (name: s: {
        assertion = s.markdownSync.enable -> s.markdownSync.path != null;
        message = "services.rclone-remotes.bisyncs.${name}.markdownSync.path must be set when markdownSync is enabled";
      }) cfg.bisyncs;

      environment.systemPackages = [ pkgs.rclone ];

      # Provides the mount.rclone helper used by mount(8) for fsType "rclone".
      system.fsPackages = [ pkgs.rclone ];

      fileSystems = mapAttrs mkFilesystem cfg.mounts;

      systemd.services =
        listToAttrs (mapAttrsToList mkBisyncService cfg.bisyncs)
        // listToAttrs (mapAttrsToList mkBisyncInitService cfg.bisyncs)
        // optionalAttrs (credMounts != { } || credBisyncs != { }) {
          # Stage credential-backed configs into a writable location: .mount
          # units cannot use LoadCredential, and rclone wants to persist token
          # refreshes, which a read-only secret would reject on every refresh.
          rclone-config = {
            description = "Stage rclone configs for credential-backed mounts and bisyncs";
            restartTriggers =
              mapAttrsToList (_name: m: m.configFile) credMounts
              ++ mapAttrsToList (_name: s: s.configFile) credBisyncs;
            serviceConfig = {
              Type = "oneshot";
              # Keep the unit active so RuntimeDirectory survives while mounts
              # and bisyncs are using the staged configs.
              RemainAfterExit = true;
              RuntimeDirectory = "rclone";
              # 0711, not 0700: bisync units run as their own User= and must
              # traverse /run/rclone to reach their private staging directory.
              # Traverse-only keeps the mount configs unlistable.
              RuntimeDirectoryMode = "0711";
              ExecStart = pkgs.writeShellScript "rclone-config-stage" (
                ''
                  set -euo pipefail
                ''
                + concatStringsSep "\n" (
                  mapAttrsToList (
                    name: m:
                    "${pkgs.coreutils}/bin/install -m 600 ${escapeShellArg m.configFile} ${escapeShellArg (stagedConfigPath name)}"
                  ) credMounts
                  ++ flatten (
                    mapAttrsToList (name: s: [
                      "${pkgs.coreutils}/bin/install -d -m 700 -o ${escapeShellArg s.user} -g ${escapeShellArg s.group} ${escapeShellArg (stagedBisyncDir name)}"
                      "${pkgs.coreutils}/bin/install -m 600 -o ${escapeShellArg s.user} -g ${escapeShellArg s.group} ${escapeShellArg s.configFile} ${escapeShellArg (stagedBisyncConfigPath name)}"
                    ]) credBisyncs
                  )
                )
              );
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
            wantedBy = [
              "suspend.target"
              "hibernate.target"
              "hybrid-sleep.target"
            ];
            serviceConfig = {
              Type = "oneshot";
              ExecStartPre = "${pkgs.coreutils}/bin/sleep ${toString cfg.mountResetDelay}";
              ExecStart = pkgs.writeShellScript "reset-rclone-mounts" (
                ''
                  set -u
                ''
                + concatStringsSep "\n" (
                  mapAttrsToList (
                    name: m:
                    let
                      unit = utils.escapeSystemdPath m.localPath;
                      path = escapeShellArg m.localPath;
                    in
                    ''
                      # ${name}: lazily unmount a stale FUSE mount (rclone died
                      # uncleanly; the mount entry lingers and blocks remounting).
                      # findmnt reads /proc/self/mountinfo and, unlike stat-based
                      # checks, does not trigger an armed automount.
                      fstype="$(${pkgs.util-linux}/bin/findmnt -n -o FSTYPE -M ${path} 2>/dev/null | ${pkgs.coreutils}/bin/tail -n1 || true)"
                      if [ "$fstype" = "fuse.rclone" ] || [ "$fstype" = "rclone" ]; then
                        if ! ${pkgs.coreutils}/bin/stat -t ${path} >/dev/null 2>&1; then
                          ${pkgs.fuse3}/bin/fusermount3 -uz ${path} \
                            || ${pkgs.util-linux}/bin/umount -l ${path} \
                            || true
                        fi
                      fi
                      ${pkgs.systemd}/bin/systemctl reset-failed ${unit}.mount ${unit}.automount 2>/dev/null || true
                    ''
                  ) cfg.mounts
                )
              );
            };
          };
        };

      systemd.timers = listToAttrs (mapAttrsToList mkBisyncTimer cfg.bisyncs);

      systemd.tmpfiles.rules =
        (mapAttrsToList mkTmpfile cfg.mounts)
        ++ (mapAttrsToList mkTmpfile cfg.bisyncs)
        # VFS cache: systemd runs mount helpers without HOME, so each mount
        # gets an explicit cache dir.
        ++ (mapAttrsToList (name: _m: "d /var/cache/rclone/${name} 0700 root root -") cfg.mounts);
    }
  ]);
}
