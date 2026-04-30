{
  description = "NixOS module for rclone FUSE mounts and bidirectional sync with optional pandoc conversion";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs?ref=nixos-unstable";
  };

  outputs =
    { self, nixpkgs }:
    {
      nixosModules = {
        rclone-remotes = import ./module.nix;
        default = self.nixosModules.rclone-remotes;
      };
    };
}
