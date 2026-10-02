{
  description = "NixOS module for rclone FUSE mounts and event-driven bidirectional sync, with markdown/docx conversion";

  # The project's public binary cache, filled by CI from main. Only honoured
  # for trusted users who accept it (--accept-flake-config).
  nixConfig = {
    extra-substituters = [ "https://nixos-rclone.cachix.org" ];
    extra-trusted-public-keys = [
      "nixos-rclone.cachix.org-1:y67XDcu9PSJL5n6GnU3Ju+PPCZh3JETrfwpxm9AndzE="
    ];
  };

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
          rclone-remotes = pkgs.callPackage ./nix/package.nix { };

        in
        {
          packages.default = rclone-remotes;
          packages.rclone-remotes = rclone-remotes;

          # `cargo build`/`cargo test` themselves run in the package's checkPhase;
          # these add the lints, reusing the same vendored cargoDeps.
          checks.rclone-remotes = rclone-remotes;

          checks.clippy = rclone-remotes.overrideAttrs (old: {
            pname = "rclone-remotes-clippy";
            nativeBuildInputs = old.nativeBuildInputs ++ [ pkgs.clippy ];
            doCheck = false;
            buildPhase = "cargo clippy --offline --all-targets -- -D warnings";
            installPhase = "touch $out";
            dontFixup = true;
          });

          checks.rustfmt =
            pkgs.runCommand "rclone-remotes-rustfmt" { nativeBuildInputs = [ pkgs.rustfmt ]; }
              ''
                cd ${inputs.self}
                rustfmt --check --edition 2024 $(find src tests -name '*.rs' 2>/dev/null)
                touch $out
              '';

          pre-commit.check.enable = false;

          # The bash/pandoc checks that used to live here (paths with spaces,
          # move tracking, remote renames, the markdown round trip) are Rust
          # tests now, run by the package build above.
          pre-commit.settings.hooks.nixfmt.enable = true;
          pre-commit.settings.hooks.rustfmt.enable = true;

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
                  mountResetDelay = 1;

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
                    # Keep the first scheduled pull out of the test's way. These two
                    # still use the pre-daemon option names, which must keep
                    # working (with a warning) as shims.
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
                    pull.onBoot = "1h";
                    settle.enable = false;
                    markdownSync = {
                      enable = true;
                      path = "/home/alice/vault";
                    };
                  };
                };
              };

            testScript = ''
              import json

              machine.wait_for_unit("multi-user.target")

              def ctl(name, action):
                  """Run `rclone-remotes ctl`; returns the pair's status."""
                  return json.loads(machine.succeed(f"rclone-remotes ctl --name {name} {action}"))

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

              with subtest("bisync services are long-running daemons, not timers"):
                  machine.wait_for_unit("rclone-bisync-test.service")
                  machine.wait_for_unit("rclone-bisync-notes.service")
                  machine.fail("systemctl cat rclone-bisync-test.timer")
                  machine.fail("systemctl cat rclone-bisync-test-init.service")
                  assert ctl("test", "status")["state"] == "idle"

              with subtest("bisync: first sync initialises the pair by itself and seeds the remote"):
                  machine.succeed("sudo -u alice touch /home/alice/sync/a.txt /home/alice/sync/b.txt")
                  st = ctl("test", "sync")
                  assert st["resyncs"] == 1, f"expected one initial resync, got {st}"
                  machine.succeed("test -f /srv/remote-data/syncdir/a.txt")

              with subtest("bisync: deletions propagate, and the pair is not resynced again"):
                  # Keep b.txt: bisync (correctly) refuses to sync a directory
                  # that became completely empty.
                  machine.succeed("rm /home/alice/sync/a.txt")
                  st = ctl("test", "sync")
                  machine.succeed("test ! -e /srv/remote-data/syncdir/a.txt")
                  machine.succeed("test -f /srv/remote-data/syncdir/b.txt")
                  assert st["resyncs"] == 1, f"pair was resynced again: {st}"

              with subtest("a remote-side change is pulled on demand"):
                  machine.succeed("echo remote > /srv/remote-data/syncdir/from-remote.txt")
                  ctl("test", "sync")
                  machine.succeed("grep -q remote /home/alice/sync/from-remote.txt")

              with subtest("bisync config is staged writable, so token refreshes persist"):
                  # LoadCredential's $CREDENTIALS_DIRECTORY is read-only, which
                  # makes rclone fail every OAuth token refresh. The daemon
                  # copies it into its own RuntimeDirectory, which the unit's
                  # User= owns, so rclone can write the temp file it renames
                  # into place.
                  machine.succeed("test -f /run/rclone-remotes/test/rclone.conf")
                  owner = machine.succeed("stat -c %U /run/rclone-remotes/test").strip()
                  assert owner == "alice", f"runtime dir owned by {owner!r}, expected 'alice'"
                  machine.succeed("sudo -u alice test -w /run/rclone-remotes/test")
                  machine.succeed("sudo -u alice test -w /run/rclone-remotes/test/rclone.conf")
                  machine.fail("sudo -u nobody ls /run/rclone-remotes/test")

              with subtest("settle runs a second bisync inside one pull"):
                  before = ctl("test", "status")["passes"]
                  after = ctl("test", "sync")["passes"]
                  assert after - before == 2, f"one pull produced {after - before} bisync passes, expected 2"

              with subtest("push: local changes reach the remote immediately, with no pull"):
                  machine.succeed("sudo -u alice sh -c 'echo pushed > /home/alice/sync/instant.txt'")
                  machine.wait_until_succeeds("grep -q pushed /srv/remote-data/syncdir/instant.txt", timeout=30)
                  machine.succeed("sudo -u alice sh -c 'echo changed > /home/alice/sync/instant.txt'")
                  machine.wait_until_succeeds("grep -q changed /srv/remote-data/syncdir/instant.txt", timeout=30)
                  machine.succeed("sudo -u alice rm /home/alice/sync/instant.txt")
                  machine.wait_until_fails("test -e /srv/remote-data/syncdir/instant.txt", timeout=30)
                  # (Not 2: with settle on, the follow-up pass may carry the second
                  # edit itself, and the push is then rightly dropped as an echo.)
                  assert ctl("test", "status")["pushed"]["uploaded"] >= 1

              with subtest("push: a rename is a server-side move that the next pull leaves alone"):
                  machine.succeed("sudo -u alice sh -c 'echo moveme > \"/home/alice/sync/Before Rename.txt\"'")
                  machine.wait_until_succeeds("test -f '/srv/remote-data/syncdir/Before Rename.txt'", timeout=30)
                  # Let the pair settle, so the rename is not merged with the creation.
                  machine.sleep(4)
                  ino = machine.succeed("stat -c %i '/srv/remote-data/syncdir/Before Rename.txt'").strip()
                  machine.succeed("sudo -u alice mv '/home/alice/sync/Before Rename.txt' '/home/alice/sync/After Rename.txt'")
                  machine.wait_until_succeeds("test -f '/srv/remote-data/syncdir/After Rename.txt'", timeout=30)
                  machine.fail("test -e '/srv/remote-data/syncdir/Before Rename.txt'")
                  new = machine.succeed("stat -c %i '/srv/remote-data/syncdir/After Rename.txt'").strip()
                  assert new == ino, f"remote file was replaced (inode {ino} -> {new}), not moved"
                  assert ctl("test", "status")["pushed"]["moved"] >= 1
                  ctl("test", "sync")
                  new = machine.succeed("stat -c %i '/srv/remote-data/syncdir/After Rename.txt'").strip()
                  assert new == ino, "the next pull replaced the moved file"

              with subtest("push: the inotify watch limit leaves room for large trees"):
                  limit = int(machine.succeed("sysctl -n fs.inotify.max_user_watches").strip())
                  assert limit >= 524288, limit

              with subtest("markdownSync: a vault rename renames the remote file, not delete + create"):
                  vault = "/home/alice/vault"
                  remote = "/srv/remote-data/notes"
                  for name in ["Statement on Gender Roles", "Draft", "Other"]:
                      machine.succeed(f"sudo -u alice sh -c 'echo \"# {name}\" > \"{vault}/{name}.md\"'")
                  ctl("notes", "sync")
                  machine.succeed(f"test -f '{remote}/Statement on Gender Roles.docx'")
                  # A server-side move on a local remote is rename(2), so the
                  # remote file keeps its inode only if it was really renamed.
                  ino = machine.succeed(f"stat -c %i '{remote}/Statement on Gender Roles.docx'").strip()

                  machine.succeed(f"sudo -u alice mv '{vault}/Statement on Gender Roles.md' '{vault}/Scriptural Basis.md'")
                  # Renamed *and* edited: only the identity pass can pair it.
                  machine.succeed(f"sudo -u alice mv '{vault}/Draft.md' '{vault}/Final.md'")
                  machine.succeed(f"sudo -u alice sh -c 'echo edited >> {vault}/Final.md'")
                  since = machine.succeed("date +%s").strip()
                  ctl("notes", "sync")

                  run = machine.succeed(f"journalctl -o cat -u rclone-bisync-notes.service --since @{since}")
                  print(run)
                  assert "followed move" in run and "Scriptural Basis.docx" in run, "the rename was not followed"
                  assert "Final.docx" in run
                  assert "Queue delete" not in run, "bisync replayed a rename as delete + create"
                  new = machine.succeed(f"stat -c %i '{remote}/Scriptural Basis.docx'").strip()
                  assert new == ino, f"remote file was replaced (inode {ino} -> {new}), not renamed"
                  machine.fail(f"test -e '{remote}/Statement on Gender Roles.docx'")
                  machine.fail(f"test -e '{remote}/Draft.docx'")
                  machine.succeed(f"test -f '{remote}/Final.docx'")
                  machine.succeed(f"test -f '{vault}/Final.md'")
                  machine.fail(f"test -e '{vault}/Draft.md'")

              with subtest("mount-reset detaches a stale mount so the automount can re-arm"):
                  # Resume leaves a mount whose rclone died uncleanly: the entry
                  # lingers and every access fails with ENOTCONN.
                  machine.succeed("cat /mnt/test/seed.txt")
                  machine.succeed("pkill -9 -f '[r]clone.* mount '")
                  machine.wait_until_fails("stat /mnt/test/seed.txt", timeout=30)
                  machine.succeed("systemctl start rclone-mount-reset.service")
                  out = machine.succeed("cat /mnt/test/seed.txt")
                  assert "hello" in out, f"mount did not recover: {out!r}"
            '';
          };

          devShells.default = pkgs.mkShell {
            packages = [
              pkgs.rclone
              pkgs.cargo
              pkgs.rustc
              pkgs.clippy
              pkgs.rustfmt
              pkgs.rust-analyzer
              pkgs.cargo-deny
              pkgs.gcc
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
