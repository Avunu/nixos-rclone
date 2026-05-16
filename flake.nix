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

          pre-commit.settings.hooks.paths-with-spaces = {
            enable = true;
            name = "paths-with-spaces";
            description = "Verify paths with spaces are handled correctly in markdown sync";
            entry = "nix build .#checks.${system}.paths-with-spaces --no-link";
            language = "system";
            pass_filenames = false;
          };

          devShells.default = pkgs.mkShell {
            buildInputs = [ pkgs.pandoc ];
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
