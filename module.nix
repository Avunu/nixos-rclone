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
        type = types.str;
        default = cfg.defaultConfigFile;
        description = "Path to the rclone config file.";
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
        type = types.str;
        default = cfg.defaultConfigFile;
        description = "Path to the rclone config file.";
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
        ];
        description = "Extra arguments passed to `rclone bisync`.";
      };

      pandoc = {
        enable = mkEnableOption "bidirectional pandoc markdown/docx conversion";

        markdownPath = mkOption {
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
          description = "Extra pandoc arguments for markdown to docx conversion.";
          example = [ "--reference-doc=/path/to/template.docx" ];
        };

        docxToMdArgs = mkOption {
          type = types.listOf types.str;
          default = [ "--wrap=none" ];
          description = "Extra pandoc arguments for docx to markdown conversion.";
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

  # ── Pandoc helpers ────────────────────────────────────────────────────

  haskellEnv = pkgs.haskellPackages.ghcWithPackages (ps: [ ps.pandoc ]);

  mkPandocPreSync =
    name: syncConfig:
    let
      mdDir = syncConfig.pandoc.markdownPath;
      docxDir = syncConfig.localPath;
      pandocBin = "${pkgs.pandoc}/bin/pandoc";
      mdToDocxArgs = escapeShellArgs syncConfig.pandoc.mdToDocxArgs;
    in
    pkgs.writeShellScript "pandoc-pre-sync-${name}" ''
      set -euo pipefail
      shopt -s globstar nullglob

      for mdfile in "${mdDir}"/**/*.md; do
        relpath="''${mdfile#${mdDir}/}"
        docxfile="${docxDir}/''${relpath%.md}.docx"

        if [ ! -f "$docxfile" ] || [ "$mdfile" -nt "$docxfile" ]; then
          mkdir -p "$(dirname "$docxfile")"
          ${pandocBin} "$mdfile" --wrap=preserve --filter ${toString filterDir}/strip-heading-ids.hs -o "$docxfile" ${mdToDocxArgs}
          touch -r "$mdfile" "$docxfile"
        fi
      done

      ${optionalString syncConfig.pandoc.syncDeletions ''
        for docxfile in "${docxDir}"/**/*.docx; do
          relpath="''${docxfile#${docxDir}/}"
          mdfile="${mdDir}/''${relpath%.docx}.md"
          if [ ! -f "$mdfile" ]; then
            rm "$docxfile"
          fi
        done
      ''}
    '';

  mkPandocPostSync =
    name: syncConfig:
    let
      mdDir = syncConfig.pandoc.markdownPath;
      docxDir = syncConfig.localPath;
      pandocBin = "${pkgs.pandoc}/bin/pandoc";
      docxToMdArgs = escapeShellArgs syncConfig.pandoc.docxToMdArgs;
    in
    pkgs.writeShellScript "pandoc-post-sync-${name}" ''
      set -euo pipefail
      shopt -s globstar nullglob

      for docxfile in "${docxDir}"/**/*.docx; do
        relpath="''${docxfile#${docxDir}/}"
        mdfile="${mdDir}/''${relpath%.docx}.md"

        if [ ! -f "$mdfile" ] || [ "$docxfile" -nt "$mdfile" ]; then
          mkdir -p "$(dirname "$mdfile")"
          ${pandocBin} "$docxfile" --filter ${toString filterDir}/compact-lists.hs -o "$mdfile" ${docxToMdArgs}
          touch -r "$docxfile" "$mdfile"
        fi
      done

      ${optionalString syncConfig.pandoc.syncDeletions ''
        for mdfile in "${mdDir}"/**/*.md; do
          relpath="''${mdfile#${mdDir}/}"
          docxfile="${docxDir}/''${relpath%.md}.docx"
          if [ ! -f "$docxfile" ]; then
            rm "$mdfile"
          fi
        done
      ''}
    '';

  # ── Builders ──────────────────────────────────────────────────────────

  mkFilesystem = _name: m: {
    device = m.remote;
    mountPoint = m.localPath;
    fsType = "rclone";
    noCheck = true;
    options = baseMountOpts ++ [
      "config=${m.configFile}"
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
    path = optionals s.pandoc.enable [ haskellEnv ];
    serviceConfig = {
      Type = "oneshot";
      User = s.user;
      Group = s.group;
      ExecStartPre = "${pkgs.coreutils}/bin/mkdir -p ${s.localPath}";
      ExecStart = concatStringsSep " " ([
        "${getExe pkgs.rclone}"
        "bisync"
        s.localPath
        s.remote
        "--config=${s.configFile}"
      ] ++ s.extraArgs);
      Restart = "on-failure";
      RestartSec = "60s";
    }
    // optionalAttrs s.pandoc.enable {
      ExecStartPre = [
        "${pkgs.coreutils}/bin/mkdir -p ${s.localPath}"
        "${mkPandocPreSync name s}"
      ];
      ExecStartPost = "${mkPandocPostSync name s}";
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
    "d ${r.localPath} ${r.dirPerms} ${r.user} ${r.group} -";

in
{
  options.services.rclone-remotes = {

    enable = mkEnableOption "rclone remote mounts and bisync services";

    # ── Global defaults ─────────────────────────────────────────────────
    defaultConfigFile = mkOption {
      type = types.str;
      default = "/etc/rclone.conf";
      description = "Default rclone config file when a remote doesn't specify one.";
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
      assertion = s.pandoc.enable -> s.pandoc.markdownPath != null;
      message = "services.rclone-remotes.bisyncs.${name}.pandoc.markdownPath must be set when pandoc is enabled";
    }) cfg.bisyncs;

    environment.systemPackages = [ pkgs.rclone ];

    fileSystems = mapAttrs mkFilesystem cfg.mounts;

    systemd.services =
      listToAttrs (mapAttrsToList mkBisyncService cfg.bisyncs)
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
