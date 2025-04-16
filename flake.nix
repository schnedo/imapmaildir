{
  description = "Sync IMAP to maildir";

  inputs = {
    nixpkgs.url = "github:nixos/nixpkgs?ref=nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs =
    {
      self,
      nixpkgs,
      flake-utils,
    }:
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        pkgs = import nixpkgs {
          inherit system;
        };
      in
      {

        packages.default =
          let
            cargoToml = pkgs.lib.importTOML ./Cargo.toml;
          in
          pkgs.rustPlatform.buildRustPackage {
            inherit (cargoToml.package) version;
            pname = cargoToml.package.name;

            src = ./.;
            cargoLock = {
              lockFile = ./Cargo.lock;
            };
            buildInputs = with pkgs; [
              openssl.dev
            ];
            nativeBuildInputs = with pkgs; [
              pkg-config
            ];

            meta = {
              description = "Sync emails via imap to maildir";
              mainProgram = cargoToml.package.name;
              homepage = "https://github.com/schnedo/imapmaildir";
              license = pkgs.lib.licenses.gpl3;
              maintainers = [ "schnedo" ];
            };
          };

        devShells.default = pkgs.mkShell {

          packages = with pkgs; [
            rustup
            openssl.dev
            pkg-config
          ];

          # LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath [
          # ];

        };

      }
    );
}
