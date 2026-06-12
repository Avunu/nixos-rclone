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

  serviceEnvPackages = with pkgs; [
    uutils-coreutils-noprefix
    rclone
  ];

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

      googleDrive = {
        enable = mkEnableOption "Google Drive-specific mount options (export/import formats for Workspace files)";

        rootFolderId = mkOption {
          type = types.nullOr types.str;
          default = null;
          description = "Restrict mount to a specific Google Drive folder ID.";
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
          "--max-lock"
          "5m"
          "--conflict-resolve"
          "newer"
          "--compare"
          "size,modtime,checksum"
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

  # ── Markdown sync helpers ─────────────────────────────────────────────

  haskellEnv = pkgs.haskellPackages.ghcWithPackages (ps: [ ps.pandoc ]);
  compile =
    name: src:
    pkgs.runCommand "${name}-filter" { nativeBuildInputs = [ haskellEnv ]; } ''
      mkdir -p $out/bin
      ghc -outputdir "$TMPDIR" ${src} -o $out/bin/${name}
    '';

  md2docxFilter = compile "md2docx" ./filters/md2docx.hs;
  docx2mdFilter = compile "docx2md" ./filters/docx2md.hs;

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
          ${pandocBin} "$mdfile" --from=markdown+lists_without_preceding_blankline --wrap=preserve --filter ${md2docxFilter}/bin/md2docx "''${ref_args[@]}" -o "$docxfile" ${mdToDocxArgs}
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
          ${pandocBin} "$docxfile" --filter ${docx2mdFilter}/bin/docx2md -o "$mdfile" ${docxToMdArgs}
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

  # Derive the listing filename rclone bisync uses under ~/.cache/rclone/bisync/
  bisyncListingPath =
    s:
    let
      path1Safe = builtins.replaceStrings [ "/" " " ] [ "_" "_" ] (lib.removePrefix "/" s.localPath);
      path2Safe = builtins.replaceStrings [ ":" "/" " " ] [ "_" "_" "_" ] s.remote;
    in
    "%h/.cache/rclone/bisync/${path1Safe}..${path2Safe}.path1.lst";

  # ── Mount helpers ─────────────────────────────────────────────────────

  sysMountOpts = concatStringsSep "," [
    "noauto"
    "_netdev"
  ];

  # Rclone mount flags — all native `--flag=value` style (NOT passed via
  # `-o` to FUSE).  This avoids the "not supported with this FUSE backend"
  # error and works identically in .mount + .service units.
  mkRcloneFlags = m:
    [
      "--allow-other"
      "--uid=${toString m.uid}"
      "--gid=${toString m.gid}"
      "--umask=022"
      "--vfs-cache-mode=full"
      "--dir-cache-time=5m"
      "--vfs-cache-max-age=24h"
      "--transfers=4"
      "--multi-thread-streams=4"
      "--timeout=1m"
      "--vfs-read-chunk-size=64M"
      "--vfs-read-chunk-size-limit=512M"
      "--buffer-size=64M"

      # SFTP: disable remote hash checking (md5sum/sha1sum via SSH).
      # The shell-escaping of special characters (spaces, parentheses, etc.)
      # in remote paths is fragile and causes false "corrupted on transfer"
      # errors.  We fall back to comparing size + modtime instead.
      "--sftp-disable-hashcheck"
    ]
    ++ (map (opt: "--${opt}") (mkGDriveMountOpts m))
    ++ m.extraOpts;

  # Mount exec for non-credential mounts — .mount + .automount path.
  # Points at the user's default rclone config.
  mkMountExec = name: m:
    let
      userCfg = config.users.users.${m.user} or { };
      userHome = userCfg.home or "/home/${m.user}";
    in
    pkgs.writeShellScript "rclone-mount-${name}" ''
      set -euo pipefail
      exec ${pkgs.rclone}/bin/rclone mount \
        --config ${escapeShellArg "${userHome}/.config/rclone/rclone.conf"} \
        ${escapeShellArgs (mkRcloneFlags m)} \
        ${escapeShellArg m.remote} ${escapeShellArg m.localPath}
    '';

  # Mount exec for credential mounts — .service with LoadCredential.
  # Copies the credential to a writable RuntimeDirectory so rclone
  # can persist config changes (token refreshes, etc.).
  mkCredMountExec = name: m:
    let
      flags = mkRcloneFlags m;
    in
    pkgs.writeShellScript "rclone-cred-mount-${name}" ''
      set -euo pipefail
      config="/run/rclone-${name}/rclone.conf"
      cp "$CREDENTIALS_DIRECTORY/rclone-config" "$config"
      exec ${pkgs.rclone}/bin/rclone mount \
        --config "$config" \
        ${escapeShellArgs flags} \
        ${escapeShellArg m.remote} ${escapeShellArg m.localPath}
    '';

  # systemd.mounts entry for mounts WITHOUT a custom configFile
  # (automount on first access, unmount after idle).
  mkMount = name: m: {
    what = m.remote;
    where = m.localPath;
    type = "rclone";
    options = sysMountOpts;
    unitConfig = {
      Requires = [ "network-online.target" ];
      After = [ "network-online.target" ];
    };
    mountConfig.ExecMount = "${mkMountExec name m}";
  };

  # systemd.services entry for mounts WITH a custom configFile.
  # Uses LoadCredential to inject the agenix secret, and runs as
  # root so fusermount3 can mount with --allow-other.
  mkMountService = name: m:
    nameValuePair "rclone-mount-${name}" {
      description = "Rclone mount for ${name}";
      after = [ "network-online.target" ];
      wants = [ "network-online.target" ];
      wantedBy = [ "default.target" ];
      serviceConfig = {
        Type = "simple";
        RuntimeDirectory = "rclone-${name}";
        LoadCredential = "rclone-config:${m.configFile}";
        ExecStart = "${mkCredMountExec name m}";
        Restart = "on-failure";
        RestartSec = "10s";
        # Capability for FUSE mount with --allow-other
        AmbientCapabilities = "CAP_SYS_ADMIN";
      };
    };

  mkBisyncInitService =
    name: s:
    nameValuePair "rclone-bisync-${name}-init" {
      description = "Initial resync for rclone bisync ${name}";
      after = [ "network-online.target" ];
      wants = [ "network-online.target" ];
      wantedBy = [ "rclone-bisync-${name}.service" ];
      before = [ "rclone-bisync-${name}.service" ];
      unitConfig.ConditionPathExists = "!${bisyncListingPath s}";
      path = serviceEnvPackages;
      serviceConfig = (
        {
          Type = "oneshot";
          User = s.user;
          Group = s.group;
          ExecStartPre = "${pkgs.coreutils}/bin/mkdir -p ${escapeShellArg s.localPath}";
          ExecStart =
            let
              configArg =
                if s.configFile != null then ''--config "$CREDENTIALS_DIRECTORY/rclone-config"'' else "";
              args = concatStringsSep " " (
                [
                  "${getExe pkgs.rclone}"
                  "bisync"
                  (escapeShellArg s.localPath)
                  (escapeShellArg s.remote)
                  "--resync"
                  "--resync-mode"
                  "newer"
                ]
                ++ mkGDriveArgs s
                ++ s.extraArgs
              );
            in
            pkgs.writeShellScript "rclone-bisync-${name}-init" ''
              exec ${args} ${configArg}
            '';
        }
        // optionalAttrs (s.configFile != null) {
          LoadCredential = [ "rclone-config:${s.configFile}" ];
        }
      );
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
      serviceConfig = (
        {
          Type = "oneshot";
          User = s.user;
          Group = s.group;
          ExecStartPre = "${pkgs.coreutils}/bin/mkdir -p ${escapeShellArg s.localPath}";
          ExecStart =
            let
              configArg =
                if s.configFile != null then ''--config "$CREDENTIALS_DIRECTORY/rclone-config"'' else "";
              args = concatStringsSep " " (
                [
                  "${getExe pkgs.rclone}"
                  "bisync"
                  (escapeShellArg s.localPath)
                  (escapeShellArg s.remote)
                ]
                ++ mkGDriveArgs s
                ++ s.extraArgs
              );
            in
            pkgs.writeShellScript "rclone-bisync-${name}" ''
              exec ${args} ${configArg}
            '';
          Restart = "on-failure";
          RestartSec = "60s";
        }
        // optionalAttrs (s.configFile != null) {
          LoadCredential = [ "rclone-config:${s.configFile}" ];
        }
        // optionalAttrs s.markdownSync.enable {
          ExecStartPre = [
            "${pkgs.coreutils}/bin/mkdir -p ${escapeShellArg s.localPath}"
            "${mkMarkdownPreSync name s}"
          ];
          ExecStartPost = "${mkMarkdownPostSync name s}";
        }
      );
    };

  mkBisyncTimer =
    name: s:
    nameValuePair "rclone-bisync-${name}" {
      description = "Timer for rclone bisync ${name}";
      wantedBy = [ "timers.target" ];
      timerConfig = {
        OnBootSec = s.onBootSec;
        OnUnitActiveSec = s.interval;
        Persistent = true;
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

    # ── Mounts without a custom configFile — .mount + .automount ────────
    systemd.mounts = mapAttrsToList mkMount (filterAttrs (_name: m: m.configFile == null) cfg.mounts);

    systemd.automounts = mapAttrsToList (name: m: {
      where = m.localPath;
      wantedBy = [ "local-fs.target" ];
      automountConfig.TimeoutIdleSec = "600";
    }) (filterAttrs (_name: m: m.configFile == null) cfg.mounts);

    # ── Mounts WITH a custom configFile — .service with LoadCredential ──
    systemd.services =
      listToAttrs (mapAttrsToList mkMountService (filterAttrs (_name: m: m.configFile != null) cfg.mounts))
      // listToAttrs (mapAttrsToList mkBisyncService cfg.bisyncs)
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
              ${pkgs.systemd}/bin/systemctl restart 'rclone-mount-*' 2>/dev/null || true
            '';
          };
        };
      };

    systemd.timers = listToAttrs (mapAttrsToList mkBisyncTimer cfg.bisyncs);

    systemd.tmpfiles.rules =
      (mapAttrsToList mkTmpfile cfg.mounts) ++ (mapAttrsToList mkTmpfile cfg.bisyncs);
  };
}
