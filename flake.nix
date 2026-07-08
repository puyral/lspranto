{
  description = "lspranto";

  inputs = {
    nixpkgs.url = "nixpkgs/nixos-unstable";

    flake-parts = {
      url = "github:hercules-ci/flake-parts";
    };
    rust-flake.url = "github:juspay/rust-flake";
    treefmt-nix.url = "github:numtide/treefmt-nix";

  };

  outputs =
    inputs@{ flake-parts, ... }:
    flake-parts.lib.mkFlake { inherit inputs; } (attrs: {
      imports = [
        inputs.rust-flake.flakeModules.default
        inputs.rust-flake.flakeModules.nixpkgs
        inputs.treefmt-nix.flakeModule
      ];
      perSystem = { self', ... }: {
        devShells.default = self'.devShells.rust;
        packages.default = self'.packages.lspranto;

        treefmt.programs = {
          nixfmt.enable = true;
          rustfmt.enable = true;
        };
      };

      systems = [ "x86_64-linux" ];

    });
}
