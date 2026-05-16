{ haskellPackages, runCommand }:
let
  haskellEnv = haskellPackages.ghcWithPackages (ps: [ ps.pandoc ]);
in
runCommand "docx2md-filter" { nativeBuildInputs = [ haskellEnv ]; } ''
  mkdir -p $out/bin
  ghc -outputdir "$TMPDIR" ${./docx2md.hs} -o $out/bin/docx2md
''
