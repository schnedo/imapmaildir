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

            watchCoverage = pkgs.writeShellApplication {
              name = "watchCoverage";
              runtimeInputs = with pkgs; [
                entr
                fd
                cargo-llvm-cov
              ];
              text = ''
                export LLVM_COV=${lib.getExe' pkgs.rustc.llvmPackages.llvm "llvm-cov"}
                export LLVM_PROFDATA=${lib.getExe' pkgs.rustc.llvmPackages.llvm "llvm-profdata"}
                cargo llvm-cov nextest --no-tests warn --html --open "$@"
                fd -e rs | entr -ccp cargo llvm-cov nextest --no-tests warn --html "$@"
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
                self'.packages.watchCoverage
                sqlitebrowser
                # keep-sorted end
              ]
              ++ config.pre-commit.settings.enabledPackages;

            shellHook =
              # bash
              ''
                if [[ ! $SKIP_SHELL_HOOK ]]; then
                  if systemctl --user start podman.socket && [[ -S "$XDG_RUNTIME_DIR"/podman/podman.sock ]]; then
                    export DOCKER_HOST="unix://$XDG_RUNTIME_DIR/podman/podman.sock"
                  fi
                  ${config.pre-commit.shellHook}
                fi
              '';

            env = {
            };
            # LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath [
            # ];

          };

          pre-commit = {
            check.enable = true;
            settings = {
              package = pkgs.prek;
              hooks = {
                # keep-sorted start block=yes
                cargo-check.enable = true;
                clippy.enable = true;
                commitlint = {
                  enable = true;
                  extraPackages = [
                    pkgs.commitlint
                  ];
                  name = "Lint commit messages";
                  entry = "${lib.getExe pkgs.commitlint} --edit";
                  stages = [ "commit-msg" ];
                  pass_filenames = false;
                  always_run = true;
                };
                gitleaks = {
                  enable = true;
                  extraPackages = [
                    pkgs.gitleaks
                  ];
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

        };

      flake = {
        homeModules = {
          default = self.homeModules.imapmaildir;

          imapmaildir = import ./nix/module.nix {
            inherit self;
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

    };
}
