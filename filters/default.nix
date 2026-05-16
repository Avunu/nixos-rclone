{ haskellPackages, runCommand }:
let
  haskellEnv = haskellPackages.ghcWithPackages (ps: [ ps.pandoc ]);
  compile = name: runCommand "${name}-filter" { nativeBuildInputs = [ haskellEnv ]; } ''
    mkdir -p $out/bin
    ghc -outputdir "$TMPDIR" ${./. + "/${name}.hs"} -o $out/bin/${name}
  '';
in
{
  md2docx = compile "md2docx";
  docx2md = compile "docx2md";
}
