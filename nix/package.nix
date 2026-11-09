{
  pkgs ? import <nixpkgs> { },
  withNotmuch ? true,
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
  useNextest = true;
  # todo: let integration tests run
  cargoTestFlags = [
    "--lib"
  ];
  checkFlags = [
    "--skip"
    "imap::transport::connection"
  ];
  preCheck = ''
    export LIBSQLITE3_SYS_USE_PKG_CONFIG=0
  ''
  + pkgs.lib.optionalString withNotmuch ''
    export LD_LIBRARY_PATH=${pkgs.lib.getLib pkgs.notmuch}/lib''${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}
  '';
  postFixup = pkgs.lib.optionalString withNotmuch ''
    patchelf --add-rpath ${pkgs.lib.getLib pkgs.notmuch}/lib $out/bin/imapmaildir
  '';
  buildInputs = with pkgs; [
    openssl.dev
  ];
  preBuild = ''
    export LIBSQLITE3_SYS_USE_PKG_CONFIG=0
  '';
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
