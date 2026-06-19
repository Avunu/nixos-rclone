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
      type = types.nullOr types.str;
      default = "docx";
      description = "Comma-separated export formats for Google Workspace files (Docs→docx, etc.). Null omits --drive-export-formats entirely.";
    };

    importFormats = mkOption {
      type = types.nullOr types.str;
      default = "docx";
      description = ''
        Comma-separated import formats when writing back to Google Drive.
        When set (e.g. "docx"), uploaded Office files are converted into native
        Google Workspace files (a Google Doc, etc.). Set to null to upload them
        as-is and keep them as plain Office files.

        For bisync pairs, prefer null: native Google Docs cannot be reliably
        bisynced. They have no stable checksum and Google rewrites their modtime
        on every save, so the remote looks "changed" on every run and collides
        with local edits, producing an endless stream of .conflictN files.
      '';
    };
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
      extraArgs = mkOption {
        type = types.listOf types.str;
        default = [
          "--verbose"
          "--resilient"
          "--recover"
          "--create-empty-src-dirs"
          "--max-lock"
          "5m"
          "--conflict-resolve"
          "newer"
          "--compare"
          "size,modtime,checksum"
        ];
        description = "Extra arguments passed to `rclone bisync`.";
      };

      googleDrive = googleDriveOptions;

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
          default = false;
          description = "Propagate deletions between markdown and docx directories.";
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

  mkGDriveMountOpts =
    m:
    optionals m.googleDrive.enable (
      optional (
        m.googleDrive.exportFormats != null
      ) "drive-export-formats=${m.googleDrive.exportFormats}"
      ++ optional (
        m.googleDrive.importFormats != null
      ) "drive-import-formats=${m.googleDrive.importFormats}"
      ++ optional (
        m.googleDrive.rootFolderId != null
      ) "drive-root-folder-id=${m.googleDrive.rootFolderId}"
    );

  mkGDriveArgs =
    s:
    optionals s.googleDrive.enable (
      optionals (s.googleDrive.exportFormats != null) [
        "--drive-export-formats"
        s.googleDrive.exportFormats
      ]
      ++ optionals (s.googleDrive.importFormats != null) [
        "--drive-import-formats"
        s.googleDrive.importFormats
      ]
      ++ [
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

  # Writable staging copy so rclone can persist config changes (token
  # refreshes, etc.) that it cannot write to a read-only secret.
  stagedConfigPath = name: "/run/rclone/${name}.conf";

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

        # Network & performance safeguards
        "transfers=4"
        "multi-thread-streams=4"
        "timeout=1m"

        # Chunked streaming optimization
        "vfs-read-chunk-size=64M"
        "vfs-read-chunk-size-limit=512M"
        "buffer-size=64M"

        # SFTP: disable remote hash checking (md5sum/sha1sum via SSH).
        # The shell-escaping of special characters in remote paths is
        # fragile and causes false "corrupted on transfer" errors.
        "sftp-disable-hashcheck"
      ]
      ++ optionals (m.configFile != null) [
        "x-systemd.requires=rclone-config.service"
        "x-systemd.after=rclone-config.service"
      ]
      ++ mkGDriveMountOpts m
      ++ m.extraOpts;
    };

  # ── Bisync services ───────────────────────────────────────────────────

  mkBisyncExec =
    scriptName: s: initArgs:
    let
      argv = [
        (getExe pkgs.rclone)
        "bisync"
        s.localPath
        s.remote
      ]
      ++ initArgs
      ++ mkGDriveArgs s
      ++ s.extraArgs;
      configArg = optionalString (
        s.configFile != null
      ) ''--config "$CREDENTIALS_DIRECTORY/rclone-config"'';
    in
    pkgs.writeShellScript scriptName ''
      exec ${escapeShellArgs argv} ${configArg}
    '';

  mkBisyncInitService =
    name: s:
    nameValuePair "rclone-bisync-${name}-init" {
      description = "Initial resync for rclone bisync ${name}";
      after = [ "network-online.target" ];
      wants = [ "network-online.target" ];
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
        ExecStart = mkBisyncExec "rclone-bisync-${name}-init" s [
          "--resync"
          "--resync-mode"
          "newer"
        ];
      }
      // optionalAttrs (s.configFile != null) {
        LoadCredential = [ "rclone-config:${s.configFile}" ];
      };
    };

  mkBisyncService =
    name: s:
    nameValuePair "rclone-bisync-${name}" {
      description = "Rclone bisync for ${name}";
      after = [ "network-online.target" ];
      wants = [ "network-online.target" ];
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
        ExecStart = mkBisyncExec "rclone-bisync-${name}" s [ ];
        ExecStartPost = optional s.markdownSync.enable "${mkMarkdownPostSync name s}";
        # No Restart=: the timer is the retry mechanism. A bisync failure
        # that needs --resync would otherwise loop uselessly.
      }
      // optionalAttrs (s.configFile != null) {
        LoadCredential = [ "rclone-config:${s.configFile}" ];
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
        // optionalAttrs (credMounts != { }) {
          # Stage credential-backed configs into a writable location: .mount
          # units cannot use LoadCredential, and rclone wants to persist token
          # refreshes, which a read-only secret would reject on every refresh.
          rclone-config = {
            description = "Stage rclone configs for credential-backed mounts";
            restartTriggers = mapAttrsToList (_name: m: m.configFile) credMounts;
            serviceConfig = {
              Type = "oneshot";
              # Keep the unit active so RuntimeDirectory survives while mounts
              # are using the staged configs.
              RemainAfterExit = true;
              RuntimeDirectory = "rclone";
              RuntimeDirectoryMode = "0700";
              ExecStart = pkgs.writeShellScript "rclone-config-stage" (
                ''
                  set -euo pipefail
                ''
                + concatStringsSep "\n" (
                  mapAttrsToList (
                    name: m:
                    "${pkgs.coreutils}/bin/install -m 600 ${escapeShellArg m.configFile} ${escapeShellArg (stagedConfigPath name)}"
                  ) credMounts
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
