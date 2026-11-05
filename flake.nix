{
  description = "Compressing, deduplicating filesystem for build artifacts on macOS";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";

  outputs =
    { self, nixpkgs }:
    let
      system = "aarch64-darwin";
    in
    {
      packages.${system}.default = nixpkgs.legacyPackages.${system}.callPackage ./nix/package.nix { };

      darwinModules.default = import ./nix/darwin.nix { inherit self; };

      homeModules.default = import ./nix/home.nix { inherit self; };
    };
}
