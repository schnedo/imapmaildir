{
  description = "Sync IMAP to maildir";

  inputs = {
    # keep-sorted start block=yes
    flake-parts.url = "github:hercules-ci/flake-parts";
    git-hooks = {
      url = "github:cachix/git-hooks.nix";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    nixpkgs.url = "github:nixos/nixpkgs?ref=nixos-unstable";
    treefmt-nix = {
      url = "github:numtide/treefmt-nix";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    # keep-sorted end
  };

  outputs =
    {
      self,
      flake-parts,
      git-hooks,
      treefmt-nix,
      ...
    }@inputs:
    flake-parts.lib.mkFlake { inherit inputs; } {
      imports = [
        # keep-sorted start
        git-hooks.flakeModule
        treefmt-nix.flakeModule
        # keep-sorted end
      ];

      systems = [
        "x86_64-linux"
      ];

      perSystem =
        {
          pkgs,
          lib,
          config,
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

            packages =
              with pkgs;
              [
                # keep-sorted start
                cargo
                cargo-nextest
                clippy
                openssl.dev
                pkg-config
                rust-analyzer
                rustc
                rustfmt
                self'.packages.showDependencyGraph
                sqlitebrowser
                # keep-sorted end
              ]
              ++ config.pre-commit.settings.enabledPackages;

            shellHook = ''
              ${config.pre-commit.shellHook}
            '';

            # LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath [
            # ];

          };

          pre-commit = {
            check.enable = true;
            settings = {
              enabledPackages = with pkgs; [
                gitleaks
              ];
              hooks = {
                # keep-sorted start block=yes
                cargo-check.enable = true;
                clippy.enable = true;
                gitleaks = {
                  enable = true;
                  name = "Detect hardcoded secrets";
                  entry = "${lib.getExe pkgs.gitleaks} git --pre-commit --redact --staged --verbose";
                  pass_filenames = false;
                };
                keep-sorted.enable = true;
                no-commit-to-branch = {
                  enable = true;
                  settings = {
                    branch = [
                      "main"
                    ];
                  };
                };
                reuse.enable = false;
                treefmt.enable = true;
                # keep-sorted end
              };
            };
          };

          treefmt = {
            programs = {
              # keep-sorted start block=yes
              deadnix.enable = true;
              nixfmt.enable = true;
              rustfmt.enable = true;
              statix.enable = true;
              taplo.enable = true;
              # keep-sorted end
            };
            settings = {
              excludes = [
                "**/secrets.yaml"
                "git/lazygit/config.yml"
              ];
            };
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
