# Compiled pandoc filters shared by module.nix and flake.nix.
pkgs:
let
  haskellEnv = pkgs.haskellPackages.ghcWithPackages (ps: [ ps.pandoc ]);
  compile =
    name: src:
    pkgs.runCommand "${name}-filter" { nativeBuildInputs = [ haskellEnv ]; } ''
      mkdir -p $out/bin
      ghc -outputdir "$TMPDIR" ${src} -o $out/bin/${name}
    '';
in
{
  md2docx = compile "md2docx" ./md2docx.hs;
  docx2md = compile "docx2md" ./docx2md.hs;
}
