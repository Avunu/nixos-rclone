{
  description = "NixOS module for rclone FUSE mounts and bidirectional sync with optional pandoc conversion";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-parts.url = "github:hercules-ci/flake-parts";
    git-hooks-nix.url = "github:cachix/git-hooks.nix";
  };

  outputs =
    inputs:
    inputs.flake-parts.lib.mkFlake { inherit inputs; } {
      imports = [
        inputs.git-hooks-nix.flakeModule
      ];

      systems = [
        "x86_64-linux"
        "aarch64-linux"
      ];

      perSystem =
        {
          config,
          pkgs,
          system,
          ...
        }:
        let
          filters = import ./filters pkgs;

          # Fake pandoc: records --reference-doc arg to $REFS_LOG, touches -o output
          fakePandoc = pkgs.writeShellScript "fake-pandoc" ''
            ref_doc=""
            out_file=""
            while [[ $# -gt 0 ]]; do
              case "$1" in
                --reference-doc=*) ref_doc="''${1#--reference-doc=}" ;;
                -o) out_file="$2"; shift ;;
              esac
              shift
            done
            [[ -n "$ref_doc" ]] && echo "$ref_doc" >> "''${REFS_LOG:-/dev/null}"
            [[ -n "$out_file" ]] && touch "$out_file"
          '';
        in
        {
          checks.paths-with-spaces =
            pkgs.runCommand "test-paths-with-spaces" { nativeBuildInputs = [ pkgs.bash ]; }
              ''
                set -euo pipefail
                shopt -s globstar nullglob

                md_dir="$TMPDIR/LT Vault"
                docx_dir="$TMPDIR/LT GDrive"
                export REFS_LOG="$TMPDIR/refs.log"

                mkdir -p "$md_dir/Meeting Notes"
                touch "$md_dir/Hello World.md"
                touch "$md_dir/Meeting Notes/Jan Session.md"

                # First pass: no existing docx, ref_args should be empty
                for mdfile in "$md_dir"/**/*.md; do
                  relpath="''${mdfile#"$md_dir"/}"
                  docxfile="$docx_dir/''${relpath%.md}.docx"
                  mkdir -p "$(dirname "$docxfile")"
                  ref_args=()
                  if [ -f "$docxfile" ]; then
                    ref_args=("--reference-doc=$docxfile")
                  fi
                  ${fakePandoc} "$mdfile" "''${ref_args[@]}" -o "$docxfile"
                  touch -r "$mdfile" "$docxfile"
                done

                # Verify docx files with spaces in their paths were created
                test -f "$docx_dir/Hello World.docx" \
                  || { echo "FAIL: 'Hello World.docx' not created"; exit 1; }
                test -f "$docx_dir/Meeting Notes/Jan Session.docx" \
                  || { echo "FAIL: 'Meeting Notes/Jan Session.docx' not created"; exit 1; }

                # --reference-doc must NOT have been called on first pass
                if [ -f "$REFS_LOG" ]; then
                  echo "FAIL: --reference-doc was passed on first pass when docx didn't exist yet"
                  exit 1
                fi

                # Second pass: touch one md file to trigger --reference-doc
                touch "$md_dir/Hello World.md"

                for mdfile in "$md_dir"/**/*.md; do
                  relpath="''${mdfile#"$md_dir"/}"
                  docxfile="$docx_dir/''${relpath%.md}.docx"
                  if [ ! -f "$docxfile" ] || [ "$mdfile" -nt "$docxfile" ]; then
                    ref_args=()
                    if [ -f "$docxfile" ]; then
                      ref_args=("--reference-doc=$docxfile")
                    fi
                    ${fakePandoc} "$mdfile" "''${ref_args[@]}" -o "$docxfile"
                    touch -r "$mdfile" "$docxfile"
                  fi
                done

                # Verify --reference-doc was passed with the full path including spaces
                grep -qF "$docx_dir/Hello World.docx" "$REFS_LOG" \
                  || { echo "FAIL: --reference-doc not recorded with correct spaced path"; cat "$REFS_LOG" || true; exit 1; }

                # Verify only the updated file triggered --reference-doc
                grep -qF "Jan Session" "$REFS_LOG" \
                  && { echo "FAIL: Jan Session.docx was re-processed when it shouldn't have been"; exit 1; } || true

                touch $out
              '';

          pre-commit.check.enable = false;

          pre-commit.settings.hooks.paths-with-spaces = {
            enable = true;
            name = "paths-with-spaces";
            description = "Verify paths with spaces are handled correctly in markdown sync";
            entry = "nix build .#checks.${system}.paths-with-spaces --no-link";
            language = "system";
            pass_filenames = false;
          };

          packages.md2docx-filter = filters.md2docx;
          packages.docx2md-filter = filters.docx2md;

          checks.round-trip = pkgs.runCommand "test-round-trip" { nativeBuildInputs = [ pkgs.pandoc ]; } ''
            set -euo pipefail

            pandoc ${inputs.self}/fixtures/test.md \
              --from=markdown+lists_without_preceding_blankline \
              --wrap=preserve \
              --filter ${config.packages.md2docx-filter}/bin/md2docx \
              -o test.docx

            pandoc test.docx \
              --filter ${config.packages.docx2md-filter}/bin/docx2md \
              -o result.md

            diff ${inputs.self}/fixtures/test.md result.md || {
              echo "--- expected (fixture) ---"
              cat ${inputs.self}/fixtures/test.md
              echo "--- got (round-trip) ---"
              cat result.md
              exit 1
            }

            touch $out
          '';

          pre-commit.settings.hooks.round-trip = {
            enable = true;
            name = "round-trip";
            description = "Verify markdown→docx→markdown round-trip matches fixture";
            entry = "nix build .#checks.${system}.round-trip --no-link";
            language = "system";
            pass_filenames = false;
          };

          # End-to-end VM test: automount + credential staging + bisync
          # init-once semantics and deletion propagation.
          checks.module-test = pkgs.testers.runNixOSTest {
            name = "rclone-remotes";

            nodes.machine =
              { pkgs, ... }:
              {
                imports = [ ./module.nix ];

                virtualisation.memorySize = 1024;

                users.users.alice = {
                  isNormalUser = true;
                  uid = 1000;
                  group = "users";
                };

                # Local-backed remote so no network is needed.
                environment.etc."rclone-test.conf".text = ''
                  [testremote]
                  type = alias
                  remote = /srv/remote-data
                '';

                systemd.tmpfiles.rules = [
                  "d /srv/remote-data 0777 root root -"
                  "d /srv/remote-data/mountdir 0777 root root -"
                  "d /srv/remote-data/syncdir 0777 root root -"
                ];

                services.rclone-remotes = {
                  enable = true;
                  defaultUser = "alice";
                  defaultGroup = "users";

                  mounts.test = {
                    remote = "testremote:mountdir";
                    localPath = "/mnt/test";
                    configFile = "/etc/rclone-test.conf";
                  };

                  bisyncs.test = {
                    # A plain local path, not the alias remote: rclone
                    # canonicalizes aliases when naming its bisync listing
                    # files, which would defeat the init-once condition.
                    remote = "/srv/remote-data/syncdir";
                    localPath = "/home/alice/sync";
                    configFile = "/etc/rclone-test.conf";
                    user = "alice";
                    # Keep the timer out of the test's way.
                    onBootSec = "1h";
                  };
                };
              };

            testScript = ''
              machine.wait_for_unit("multi-user.target")

              with subtest("automount triggers on access, credential staging works"):
                  machine.succeed("echo hello > /srv/remote-data/mountdir/seed.txt")
                  machine.wait_for_unit("mnt-test.automount")
                  out = machine.succeed("cat /mnt/test/seed.txt")
                  assert "hello" in out, f"unexpected mount content: {out!r}"
                  machine.succeed("systemctl is-active rclone-config.service")
                  machine.succeed("test -f /run/rclone/test.conf")

              with subtest("writes through the mount reach the backing dir"):
                  machine.succeed("echo back > /mnt/test/write.txt")
                  machine.wait_until_succeeds(
                      "test -f /srv/remote-data/mountdir/write.txt", timeout=60
                  )

              with subtest("bisync: initial resync seeds the remote"):
                  machine.succeed("sudo -u alice mkdir -p /home/alice/sync")
                  machine.succeed("sudo -u alice touch /home/alice/sync/a.txt /home/alice/sync/b.txt")
                  machine.succeed("systemctl start rclone-bisync-test.service")
                  machine.succeed("test -f /srv/remote-data/syncdir/a.txt")

              with subtest("bisync: deletions propagate, init is condition-skipped"):
                  # Keep b.txt: bisync (correctly) refuses to sync a directory
                  # that became completely empty.
                  machine.succeed("rm /home/alice/sync/a.txt")
                  machine.succeed("systemctl start rclone-bisync-test.service")
                  machine.succeed("test ! -e /srv/remote-data/syncdir/a.txt")
                  machine.succeed("test -f /srv/remote-data/syncdir/b.txt")
                  # rclone logs this once per actual run; a condition-skipped
                  # start logs nothing, so the count is the number of resyncs.
                  runs = machine.succeed(
                      "journalctl -u rclone-bisync-test-init.service | grep -c 'Bisync successful' || true"
                  ).strip()
                  assert runs == "1", f"init resync ran {runs} times, expected 1"
            '';
          };

          devShells.default = pkgs.mkShell {
            packages = [
              pkgs.pandoc
              pkgs.rclone
              (pkgs.haskellPackages.ghcWithPackages (ps: [ ps.pandoc ]))
            ];
            shellHook = config.pre-commit.installationScript;
          };
        };

      flake = {
        nixosModules = {
          rclone-remotes = import ./module.nix;
          default = inputs.self.nixosModules.rclone-remotes;
        };
      };
    };
}
