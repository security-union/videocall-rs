{pkgs ? import <nixpkgs> {}}:
pkgs.mkShell {
  nativeBuildInputs = with pkgs; [
    zola
    pre-commit

    # Formatters
    treefmt
    nodePackages.prettier
    alejandra
    djlint
  ];
}
