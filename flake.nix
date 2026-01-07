{
  description = "Sync IMAP to maildir";

  inputs = {
    nixpkgs.url = "github:nixos/nixpkgs?ref=nixos-unstable";
    flake-parts.url = "github:hercules-ci/flake-parts";
  };

  outputs =
    {
      self,
      flake-parts,
      ...
    }@inputs:
    flake-parts.lib.mkFlake { inherit inputs; } {
      systems = [
        "x86_64-linux"
      ];

      perSystem =
        {
          pkgs,
          self',
          ...
        }:
        {

          packages = {

            default = self'.packages.imapmaildir;

            imapmaildir = pkgs.callPackage ./nix/package.nix { };

            showDependencyGraph = pkgs.writeShellApplication {
              name = "showDependencyGraph";
              runtimeInputs = with pkgs; [
                cargo
                cargo-modules
                xdot
              ];
              text = ''
                cargo-modules dependencies --no-externs --no-private --no-owns --no-fns | xdot -
              '';
            };

          };

          devShells.default = pkgs.mkShell {

            packages = with pkgs; [
              cargo
              clippy
              openssl.dev
              pkg-config
              rust-analyzer
              rustc
              rustfmt
              self'.packages.showDependencyGraph
              sqlitebrowser
            ];

            # LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath [
            # ];

          };

        };

      flake = {
        homeModules = {
          default = self.homeModules.imapmaildir;

          imapmaildir = import ./nix/module.nix {
            inherit self;
          };

        };
      };

    };
}
