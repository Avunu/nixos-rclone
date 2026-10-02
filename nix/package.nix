{
  lib,
  rustPlatform,
  rclone,
}:

let
  manifest = lib.importTOML ../Cargo.toml;
in
rustPlatform.buildRustPackage {
  pname = manifest.package.name;
  inherit (manifest.package) version;

  src = lib.fileset.toSource {
    root = ../.;
    fileset = lib.fileset.unions [
      ../Cargo.toml
      ../Cargo.lock
      ../src
      (lib.fileset.maybeMissing ../tests)
      ../fixtures
    ];
  };

  # Ingest Cargo.lock directly: no cargoHash to maintain, and Dependabot's cargo
  # bumps need no follow-up edit. Works only while every dependency comes from a
  # registry; a git dependency would need `outputHashes` here.
  cargoLock.lockFile = ../Cargo.lock;

  # Integration tests drive a real `rclone rcd` against local directories.
  nativeCheckInputs = [ rclone ];

  meta = {
    description = "Supervisor daemon for the nixos-rclone module";
    homepage = "https://github.com/Avunu/nixos-rclone";
    license = lib.licenses.mit;
    mainProgram = "rclone-remotes";
    platforms = lib.platforms.linux;
  };
}
