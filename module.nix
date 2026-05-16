{
  config,
  pkgs,
  lib,
  ...
}:

with lib;

let
  cfg = config.services.rclone-remotes;

  filterDir = ./filters;

  # ── Submodule: live FUSE mount ────────────────────────────────────────
  mountSubmodule = types.submodule {
    options = {
      remote = mkOption {
        type = types.str;
        description = "Rclone remote path, e.g. `myremote:path`.";
        example = "webdav:documents";
      };
      localPath = mkOption {
        type = types.str;
        description = "Absolute local path to mount into.";
      };
      configFile = mkOption {
        type = types.nullOr types.str;
        default = cfg.defaultConfigFile;
        description = "Path to the rclone config file. Defaults to rclone's default (~/.config/rclone/rclone.conf) when null.";
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
        description = "Owner for the tmpfiles directory rule.";
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
        description = "Extra options appended to the rclone mount.";
      };
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
        type = types.str;
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
          "--max-lock" "5m"
          "--conflict-resolve" "newer"
          "--compare" "size,modtime,checksum"
        ];
        description = "Extra arguments passed to `rclone bisync`.";
      };

      googleDrive = {
        enable = mkEnableOption "Google Drive-specific bisync options";

        rootFolderId = mkOption {
          type = types.nullOr types.str;
          default = null;
          description = "Restrict sync to a specific Google Drive folder ID.";
          example = "14zaHa9I5dpMa4AaUTt_Mi7r2_AyT6654";
        };

        exportFormats = mkOption {
          type = types.str;
          default = "docx";
          description = "Comma-separated list of formats to export Google Docs as.";
        };

        importFormats = mkOption {
          type = types.str;
          default = "docx";
          description = "Comma-separated list of formats to import into Google Docs.";
        };
      };

      markdownSync = {
        enable = mkEnableOption "bidirectional markdown/docx sync";

        path = mkOption {
          type = types.nullOr types.str;
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

  # ── Mount option baseline ─────────────────────────────────────────────
  baseMountOpts = [
    "x-systemd.automount"
    "noauto"
    "_netdev"
    "x-systemd.idle-timeout=600"
    "x-systemd.mount-timeout=120s"
    "x-systemd.requires=network-online.target"
    "x-systemd.after=network-online.target"
    "x-systemd.after=nss-lookup.target"
    "rw"
    "allow_other"
    "vfs-cache-mode=writes"
    "dir-cache-time=5m"
    "poll-interval=30s"
    "multi-thread-streams=4"
    "transfers=8"
    "retries=10"
    "low-level-retries=20"
    "timeout=10m"
    "contimeout=120s"
    "no-checksum"
    "no-modtime"
    "vfs-read-chunk-size=64M"
    "vfs-read-chunk-size-limit=512M"
    "buffer-size=64M"
    "vfs-cache-max-age=24h"
    "vfs-read-ahead=128M"
    "attr-timeout=1m"
    "vfs-write-back=5s"
  ];

  # ── Markdown sync helpers ─────────────────────────────────────────────

  haskellEnv = pkgs.haskellPackages.ghcWithPackages (ps: [ ps.pandoc ]);

  mkMarkdownPreSync =
    name: syncConfig:
    let
      mdDir = syncConfig.markdownSync.path;
      docxDir = syncConfig.localPath;
      pandocBin = "${pkgs.pandoc}/bin/pandoc";
      mdToDocxArgs = escapeShellArgs syncConfig.markdownSync.mdToDocxArgs;
    in
    pkgs.writeShellScript "markdown-pre-sync-${name}" ''
      set -euo pipefail
      shopt -s globstar nullglob

      md_dir=${escapeShellArg mdDir}
      docx_dir=${escapeShellArg docxDir}

      for mdfile in "$md_dir"/**/*.md; do
        relpath="''${mdfile#"$md_dir"/}"
        docxfile="$docx_dir/''${relpath%.md}.docx"

        if [ ! -f "$docxfile" ] || [ "$mdfile" -nt "$docxfile" ]; then
          mkdir -p "$(dirname "$docxfile")"
          ref_args=()
          if [ -f "$docxfile" ]; then
            ref_args=("--reference-doc=$docxfile")
          fi
          ${pandocBin} "$mdfile" --wrap=preserve --filter ${toString filterDir}/md2docx.hs "''${ref_args[@]}" -o "$docxfile" ${mdToDocxArgs}
          touch -r "$mdfile" "$docxfile"
        fi
      done

      ${optionalString syncConfig.markdownSync.syncDeletions ''
        for docxfile in "$docx_dir"/**/*.docx; do
          relpath="''${docxfile#"$docx_dir"/}"
          mdfile="$md_dir/''${relpath%.docx}.md"
          if [ ! -f "$mdfile" ]; then
            rm "$docxfile"
          fi
        done
      ''}
    '';

  mkMarkdownPostSync =
    name: syncConfig:
    let
      mdDir = syncConfig.markdownSync.path;
      docxDir = syncConfig.localPath;
      pandocBin = "${pkgs.pandoc}/bin/pandoc";
      docxToMdArgs = escapeShellArgs syncConfig.markdownSync.docxToMdArgs;
    in
    pkgs.writeShellScript "markdown-post-sync-${name}" ''
      set -euo pipefail
      shopt -s globstar nullglob

      md_dir=${escapeShellArg mdDir}
      docx_dir=${escapeShellArg docxDir}

      for docxfile in "$docx_dir"/**/*.docx; do
        relpath="''${docxfile#"$docx_dir"/}"
        mdfile="$md_dir/''${relpath%.docx}.md"

        if [ ! -f "$mdfile" ] || [ "$docxfile" -nt "$mdfile" ]; then
          mkdir -p "$(dirname "$mdfile")"
          ${pandocBin} "$docxfile" --filter ${toString filterDir}/docx2md.hs -o "$mdfile" ${docxToMdArgs}
          touch -r "$docxfile" "$mdfile"
        fi
      done

      ${optionalString syncConfig.markdownSync.syncDeletions ''
        for mdfile in "$md_dir"/**/*.md; do
          relpath="''${mdfile#"$md_dir"/}"
          docxfile="$docx_dir/''${relpath%.md}.docx"
          if [ ! -f "$docxfile" ]; then
            rm "$mdfile"
          fi
        done
      ''}
    '';

  # ── Builders ──────────────────────────────────────────────────────────

  mkGDriveArgs = s: optionals s.googleDrive.enable (
    [
      "--drive-export-formats" s.googleDrive.exportFormats
      "--drive-import-formats" s.googleDrive.importFormats
      "--fix-case"
      "--slow-hash-sync-only"
    ] ++ optional (s.googleDrive.rootFolderId != null)
        "--drive-root-folder-id=${s.googleDrive.rootFolderId}"
  );

  # Derive the listing filename rclone bisync uses under ~/.cache/rclone/bisync/
  bisyncListingPath = s:
    let
      path1Safe = builtins.replaceStrings [ "/" " " ] [ "_" "_" ] (lib.removePrefix "/" s.localPath);
      path2Safe = builtins.replaceStrings [ ":" "/" " " ] [ "_" "_" "_" ] s.remote;
    in
    "%h/.cache/rclone/bisync/${path1Safe}..${path2Safe}.path1.lst";

  mkBisyncInitService = name: s: nameValuePair "rclone-bisync-${name}-init" {
    description = "Initial resync for rclone bisync ${name}";
    after = [ "network-online.target" ];
    wants = [ "network-online.target" ];
    wantedBy = [ "rclone-bisync-${name}.service" ];
    before = [ "rclone-bisync-${name}.service" ];
    unitConfig.ConditionPathExists = "!${bisyncListingPath s}";
    serviceConfig = {
      Type = "oneshot";
      User = s.user;
      Group = s.group;
      ExecStartPre = "${pkgs.coreutils}/bin/mkdir -p ${escapeShellArg s.localPath}";
      ExecStart = concatStringsSep " " ([
        "${getExe pkgs.rclone}"
        "bisync"
        (escapeShellArg s.localPath)
        (escapeShellArg s.remote)
        "--resync"
        "--resync-mode" "newer"
      ] ++ (optional (s.configFile != null) "--config=${escapeShellArg s.configFile}")
        ++ mkGDriveArgs s
        ++ s.extraArgs);
    };
  };

  mkFilesystem = _name: m:
    let
      effectiveConfig =
        if m.configFile != null then m.configFile
        else let
          userCfg = config.users.users.${m.user} or {};
          userHome = userCfg.home or "/home/${m.user}";
        in "${userHome}/.config/rclone/rclone.conf";
    in {
      device = m.remote;
      mountPoint = m.localPath;
      fsType = "rclone";
      noCheck = true;
      options = baseMountOpts
        ++ [ "config=${effectiveConfig}" ]
        ++ [
        "uid=${toString m.uid}"
        "gid=${toString m.gid}"
        "umask=022"
      ] ++ m.extraOpts;
      neededForBoot = false;
    };

  mkBisyncService = name: s: nameValuePair "rclone-bisync-${name}" {
    description = "Rclone bisync for ${name}";
    after = [ "network-online.target" ];
    wants = [ "network-online.target" ];
    path = optionals s.markdownSync.enable [ haskellEnv ];
    serviceConfig = {
      Type = "oneshot";
      User = s.user;
      Group = s.group;
      ExecStartPre = "${pkgs.coreutils}/bin/mkdir -p ${escapeShellArg s.localPath}";
      ExecStart = concatStringsSep " " ([
        "${getExe pkgs.rclone}"
        "bisync"
        (escapeShellArg s.localPath)
        (escapeShellArg s.remote)
      ] ++ (optional (s.configFile != null) "--config=${escapeShellArg s.configFile}")
        ++ mkGDriveArgs s
        ++ s.extraArgs);
      Restart = "on-failure";
      RestartSec = "60s";
    }
    // optionalAttrs s.markdownSync.enable {
      ExecStartPre = [
        "${pkgs.coreutils}/bin/mkdir -p ${escapeShellArg s.localPath}"
        "${mkMarkdownPreSync name s}"
      ];
      ExecStartPost = "${mkMarkdownPostSync name s}";
    };
  };

  mkBisyncTimer = name: s: nameValuePair "rclone-bisync-${name}" {
    description = "Timer for rclone bisync ${name}";
    wantedBy = [ "timers.target" ];
    timerConfig = {
      OnBootSec = s.onBootSec;
      OnUnitActiveSec = s.interval;
      Persistent = true;
      RandomizedDelaySec = "5m";
    };
  };

  mkTmpfile = _name: r:
    "d '${r.localPath}' ${r.dirPerms} ${r.user} ${r.group} -";

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
      description = "Reset failed rclone mounts after suspend/hibernate resume.";
    };

    mountResetDelay = mkOption {
      type = types.int;
      default = 15;
      description = "Seconds to wait after resume before resetting mounts (network stabilisation).";
    };
  };

  config = mkIf cfg.enable {

    assertions = mapAttrsToList (name: s: {
      assertion = s.markdownSync.enable -> s.markdownSync.path != null;
      message = "services.rclone-remotes.bisyncs.${name}.markdownSync.path must be set when markdownSync is enabled";
    }) cfg.bisyncs;

    environment.systemPackages = [ pkgs.rclone ];

    fileSystems = mapAttrs mkFilesystem cfg.mounts;

    systemd.services =
      listToAttrs (mapAttrsToList mkBisyncService cfg.bisyncs)
      // listToAttrs (mapAttrsToList mkBisyncInitService cfg.bisyncs)
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
            ExecStart = pkgs.writeShellScript "reset-rclone-mounts" ''
              ${pkgs.systemd}/bin/systemctl reset-failed 'home-*.mount' 2>/dev/null || true
              ${pkgs.systemd}/bin/systemctl reset-failed 'home-*.automount' 2>/dev/null || true
            '';
          };
        };
      };

    systemd.timers = listToAttrs (mapAttrsToList mkBisyncTimer cfg.bisyncs);

    systemd.tmpfiles.rules =
      (mapAttrsToList mkTmpfile cfg.mounts)
      ++ (mapAttrsToList mkTmpfile cfg.bisyncs);
  };
}
