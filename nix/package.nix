{
  pkgs ? import <nixpkgs> { },
  ...
}:
let
  cargoToml = pkgs.lib.importTOML ../Cargo.toml;
in
pkgs.rustPlatform.buildRustPackage {
  inherit (cargoToml.package) version;
  pname = cargoToml.package.name;
  src = ../.;
  cargoLock = {
    lockFile = ../Cargo.lock;
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
}
