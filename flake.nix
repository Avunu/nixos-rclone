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

          # Move/rename tracking: a relocation on one side must be *followed*
          # on the other, not duplicated. Exercises mirror-moves.sh directly.
          checks.move-tracking =
            pkgs.runCommand "test-move-tracking" { nativeBuildInputs = [ pkgs.bash ]; }
              ''
                set -euo pipefail
                shopt -s globstar nullglob
                source ${./mirror-moves.sh}

                S="$TMPDIR/vault"; D="$TMPDIR/docx"
                # A genuine move or rename preserves mtime on both sides.
                mk() { mkdir -p "$(dirname "$1")"; : > "$1"; touch -d "$2" "$1"; }

                # Steady state, including the same basename in two folders --
                # neither of these may be disturbed.
                mk "$S/Advisors Thank-you letter.md"          "2023-06-20 15:50:05.1"
                mk "$D/Advisors Thank-you letter.docx"        "2023-06-20 15:50:05.1"
                mk "$S/Letters/Advisors Thank-you letter.md"  "2024-01-02 03:04:05.2"
                mk "$D/Letters/Advisors Thank-you letter.docx" "2024-01-02 03:04:05.2"

                # 1. relocated AND edited in transit -> paired by basename
                mk "$D/Mediation Plan - Shenk & Burkholder.docx"       "2024-12-22 02:19:04.5"
                mk "$S/Archive/Mediation Plan - Shenk & Burkholder.md" "2026-08-07 10:39:10.9"
                # 2. whole-directory move
                mk "$D/Proj/a.docx"         "2025-03-03 03:03:03.3"
                mk "$S/Archive/Proj/a.md"   "2025-03-03 03:03:03.3"
                # 3. pure rename in place -> basename differs, paired by mtime
                mk "$D/Old Name.docx" "2026-05-05 05:05:05.5"
                mk "$S/New Name.md"   "2026-05-05 05:05:05.5"
                # 4. renamed AND relocated at once
                mk "$D/Notes from Ordination Discussion.docx" "2025-09-26 09:29:32.7"
                mk "$S/Archive/Ordination Notes.md"           "2025-09-26 09:29:32.7"
                # 5/6. a real delete and a real create, which must NOT be paired
                mk "$D/DeletedInVault.docx" "2026-01-01 01:01:01"
                mk "$S/BrandNew.md"         "2026-02-02 02:02:02"
                # 7. same basename moved twice over -> disambiguated by mtime
                mk "$D/x/Dup.docx" "2026-06-06 06:06:06.6"; mk "$S/p/Dup.md" "2026-06-06 06:06:06.6"
                mk "$D/y/Dup.docx" "2026-07-07 07:07:07.7"; mk "$S/q/Dup.md" "2026-07-07 07:07:07.7"

                mirror_moves "$S" .md "$D" .docx

                want() {
                  [ -e "$D/$1" ] || { echo "FAIL: expected '$1' to exist"; exit 1; }
                }
                gone() {
                  [ ! -e "$D/$1" ] || { echo "FAIL: expected '$1' to be gone"; exit 1; }
                }

                want "Advisors Thank-you letter.docx"
                want "Letters/Advisors Thank-you letter.docx"
                want "Archive/Mediation Plan - Shenk & Burkholder.docx"
                gone "Mediation Plan - Shenk & Burkholder.docx"
                want "Archive/Proj/a.docx";        gone "Proj/a.docx"
                want "New Name.docx";              gone "Old Name.docx"
                want "Archive/Ordination Notes.docx"
                gone "Notes from Ordination Discussion.docx"
                want "p/Dup.docx"; want "q/Dup.docx"; gone "x/Dup.docx"; gone "y/Dup.docx"
                # A delete is not a move, and neither is a create.
                want "DeletedInVault.docx"
                gone "BrandNew.docx"

                # Nothing invented, nothing lost: 9 docx in, 9 docx out.
                total=$(find "$D" -name '*.docx' | wc -l)
                [ "$total" -eq 9 ] || { echo "FAIL: $total docx files, expected 9"; find "$D"; exit 1; }

                touch $out
              '';

          # A rename made in the vault must reach the remote as a rename of the
          # *same* file, not as bisync's delete + create (which on Google Drive
          # mints a new file ID and trashes the original). Runs real rclone
          # bisync against local directories, with the remote side driven by
          # follow_remote_move exactly as the pre-sync hook drives it.
          checks.remote-renames =
            pkgs.runCommand "test-remote-renames"
              {
                nativeBuildInputs = [
                  pkgs.bash
                  pkgs.rclone
                ];
              }
              ''
                set -euo pipefail
                shopt -s globstar nullglob
                source ${./mirror-moves.sh}

                V="$TMPDIR/vault"; L="$TMPDIR/docx"; R="$TMPDIR/remote"; W="$TMPDIR/bisync"
                ids_file="$TMPDIR/vault.ids"
                export RCLONE_CONFIG="$TMPDIR/rclone.conf"
                mkdir -p "$V/Sub" "$L" "$R" "$W"

                # Stand-in for the pandoc pass: newer md -> docx, carrying mtime.
                convert() {
                  local md rel docx
                  for md in "$V"/**/*.md; do
                    rel="''${md#"$V"/}"; docx="$L/''${rel%.md}.docx"
                    if [ ! -f "$docx" ] || [ "$md" -nt "$docx" ]; then
                      mkdir -p "$(dirname "$docx")"
                      cp "$md" "$docx"; touch -r "$md" "$docx"
                    fi
                  done
                }
                bisync() { rclone bisync "$L" "$R" --workdir "$W" --verbose --color NEVER "$@" 2>&1 | tee "$TMPDIR/bisync.log"; }
                fail() { echo "FAIL: $*"; exit 1; }

                for n in "Statement on Gender Roles" Keep1 Keep2 Keep3 Sub/Nested Gone; do
                  echo "$n" > "$V/$n.md"; touch -d "2026-01-01 00:00:00" "$V/$n.md"
                done
                convert
                bisync --resync
                record_ids "$V" .md "$ids_file"

                remote="$R"
                rclone=(rclone)
                listing1=$(echo "$W"/*.path1.lst); listing2=$(echo "$W"/*.path2.lst)
                [ -f "$listing1" ] && [ -f "$listing2" ] || fail "no bisync listings"

                old_ino=$(stat -c %i "$R/Statement on Gender Roles.docx")
                nested_ino=$(stat -c %i "$R/Sub/Nested.docx")

                # Renamed AND edited, so neither basename nor mtime can pair it:
                # only the identity pass can. Plus a pure relocation, and a real
                # delete + create that must stay unpaired.
                mv "$V/Statement on Gender Roles.md" "$V/Scriptural Basis of Godly Femininity and Masculinity.md"
                echo edited >> "$V/Scriptural Basis of Godly Femininity and Masculinity.md"
                mkdir -p "$V/Archive"; mv "$V/Sub/Nested.md" "$V/Archive/Nested.md"
                rm "$V/Gone.md"; echo new > "$V/Brand New.md"

                mirror_moves "$V" .md "$L" .docx "$ids_file" follow_remote_move 2> "$TMPDIR/moves.log"
                cat "$TMPDIR/moves.log"
                grep -q "followed move by identity: Statement on Gender Roles.docx -> Scriptural Basis" "$TMPDIR/moves.log" \
                  || fail "rename+edit not paired by identity"
                grep -q "unpaired orphan 'Gone.docx'" "$TMPDIR/moves.log" || fail "delete was paired"
                grep -q "unpaired new file 'Brand New.md'" "$TMPDIR/moves.log" || fail "create was paired"

                # Server-side move on the local backend is rename(2): same inode.
                new_ino=$(stat -c %i "$R/Scriptural Basis of Godly Femininity and Masculinity.docx")
                [ "$new_ino" = "$old_ino" ] || fail "remote file was replaced, not renamed"
                [ "$(stat -c %i "$R/Archive/Nested.docx")" = "$nested_ino" ] || fail "nested move not renamed"
                [ ! -e "$R/Statement on Gender Roles.docx" ] || fail "old remote path survives"

                # What the rest of pre-sync does, then the sync itself.
                convert
                rm "$L/Gone.docx"
                bisync
                grep -q "Path1 *File changed: .* - Scriptural Basis" "$TMPDIR/bisync.log" \
                  || fail "edit not seen as an in-place change"
                if grep -E "Queue delete .*(Statement|Scriptural|Nested)|File is new .*(Scriptural|Nested)" "$TMPDIR/bisync.log"; then
                  fail "bisync replayed a move as delete + create"
                fi
                grep -q edited "$R/Scriptural Basis of Godly Femininity and Masculinity.docx" || fail "edit not uploaded"
                [ ! -e "$R/Gone.docx" ] && [ -e "$R/Brand New.docx" ] || fail "plain delete/create not synced"
                record_ids "$V" .md "$ids_file"

                # Offline remote: the run must stop before touching anything.
                mv "$V/Keep1.md" "$V/Renamed1.md"
                remote="nosuchremote:"
                if (mirror_moves "$V" .md "$L" .docx "$ids_file" follow_remote_move); then
                  fail "an unreachable remote did not abort the run"
                fi
                [ -e "$L/Keep1.docx" ] && [ ! -e "$L/Renamed1.docx" ] || fail "local move made despite abort"
                grep -q '"Keep1.docx"$' "$listing2" || fail "listing changed despite abort"

                # Already gone from the remote: follow locally, leave the rest to bisync.
                remote="$R"
                rm "$R/Keep1.docx"
                mirror_moves "$V" .md "$L" .docx "$ids_file" follow_remote_move
                [ -e "$L/Renamed1.docx" ] || fail "local move not made"
                convert
                bisync
                [ -e "$R/Renamed1.docx" ] && [ ! -e "$R/Keep1.docx" ] || fail "vanished-remote case not synced"

                bisync
                grep -q "No changes found" "$TMPDIR/bisync.log" || fail "pair did not converge"

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

          pre-commit.settings.hooks.move-tracking = {
            enable = true;
            name = "move-tracking";
            description = "Verify moves and renames are followed, not duplicated";
            entry = "nix build .#checks.${system}.move-tracking --no-link";
            language = "system";
            pass_filenames = false;
          };

          pre-commit.settings.hooks.remote-renames = {
            enable = true;
            name = "remote-renames";
            description = "Verify vault renames reach the remote as renames, not delete + create";
            entry = "nix build .#checks.${system}.remote-renames --no-link";
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
                  "d /srv/remote-data/notes 0777 root root -"
                  # bisync will not sync against an empty side.
                  "f /srv/remote-data/notes/seed.txt 0666 root root - seed"
                  "d /home/alice/vault 0755 alice users -"
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
                    settlePass = {
                      enable = true;
                      delay = 1;
                    };
                  };

                  bisyncs.notes = {
                    remote = "/srv/remote-data/notes";
                    localPath = "/home/alice/notes-docx";
                    configFile = "/etc/rclone-test.conf";
                    user = "alice";
                    onBootSec = "1h";
                    settlePass.enable = false;
                    markdownSync = {
                      enable = true;
                      path = "/home/alice/vault";
                    };
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

              with subtest("bisync config is staged writable, so token refreshes persist"):
                  # LoadCredential's $CREDENTIALS_DIRECTORY is read-only, which
                  # makes rclone fail every OAuth token refresh. The staged copy
                  # lives in a directory the unit's User= owns, so rclone can
                  # write the temp file it renames into place.
                  machine.succeed("test -f /run/rclone/bisync-test/rclone.conf")
                  owner = machine.succeed("stat -c %U /run/rclone/bisync-test").strip()
                  assert owner == "alice", f"staging dir owned by {owner!r}, expected 'alice'"
                  machine.succeed("sudo -u alice test -w /run/rclone/bisync-test")

              with subtest("settlePass runs a second bisync inside one service start"):
                  def successes():
                      return int(machine.succeed(
                          "journalctl -u rclone-bisync-test.service | grep -c 'Bisync successful' || true"
                      ).strip())

                  before = successes()
                  machine.succeed("systemctl start rclone-bisync-test.service")
                  delta = successes() - before
                  assert delta == 2, f"one start produced {delta} bisync passes, expected 2"

              with subtest("markdownSync: a vault rename renames the remote file, not delete + create"):
                  vault = "/home/alice/vault"
                  remote = "/srv/remote-data/notes"
                  for name in ["Statement on Gender Roles", "Draft", "Other"]:
                      machine.succeed(f"sudo -u alice sh -c 'echo \"# {name}\" > \"{vault}/{name}.md\"'")
                  machine.succeed("systemctl start rclone-bisync-notes.service")
                  machine.succeed(f"test -f '{remote}/Statement on Gender Roles.docx'")
                  # A server-side move on a local remote is rename(2), so the
                  # remote file keeps its inode only if it was really renamed.
                  ino = machine.succeed(f"stat -c %i '{remote}/Statement on Gender Roles.docx'").strip()

                  machine.succeed(f"sudo -u alice mv '{vault}/Statement on Gender Roles.md' '{vault}/Scriptural Basis.md'")
                  # Renamed *and* edited: only the identity pass can pair it.
                  machine.succeed(f"sudo -u alice mv '{vault}/Draft.md' '{vault}/Final.md'")
                  machine.succeed(f"sudo -u alice sh -c 'echo edited >> {vault}/Final.md'")
                  machine.succeed("systemctl start rclone-bisync-notes.service")

                  run = machine.succeed(
                      "journalctl -o cat _SYSTEMD_INVOCATION_ID=$(systemctl show -p InvocationID --value rclone-bisync-notes.service)"
                  )
                  print(run)
                  assert "followed move by identity: Statement on Gender Roles.docx -> Scriptural Basis.docx" in run
                  assert "followed move by identity: Draft.docx -> Final.docx" in run
                  assert "renamed on remote" in run
                  assert "Queue delete" not in run, "bisync replayed a rename as delete + create"
                  new = machine.succeed(f"stat -c %i '{remote}/Scriptural Basis.docx'").strip()
                  assert new == ino, f"remote file was replaced (inode {ino} -> {new}), not renamed"
                  machine.fail(f"test -e '{remote}/Statement on Gender Roles.docx'")
                  machine.fail(f"test -e '{remote}/Draft.docx'")
                  machine.succeed(f"test -f '{remote}/Final.docx'")
                  machine.succeed(f"test -f '{vault}/Final.md'")
                  machine.fail(f"test -e '{vault}/Draft.md'")
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
