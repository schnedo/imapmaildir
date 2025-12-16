{
  description = "Sync IMAP to maildir";

  inputs = {
    nixpkgs.url = "github:nixos/nixpkgs?ref=nixos-unstable";
    flake-parts.url = "github:hercules-ci/flake-parts";
  };

  outputs =
    {
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
          ...
        }:
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
              cargo
              clippy
              openssl.dev
              pkg-config
              rust-analyzer
              rustc
              rustfmt
              sqlitebrowser
            ];

            # LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath [
            # ];

          };

        };

    };
}
