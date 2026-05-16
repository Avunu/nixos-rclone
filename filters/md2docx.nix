{ haskellPackages, runCommand }:
let
  haskellEnv = haskellPackages.ghcWithPackages (ps: [ ps.pandoc ]);
in
runCommand "md2docx-filter" { nativeBuildInputs = [ haskellEnv ]; } ''
  mkdir -p $out/bin
  ghc -outputdir "$TMPDIR" ${./md2docx.hs} -o $out/bin/md2docx
''
