{
  self,
  ...
}:
{
  pkgs,
  lib,
  config,
  ...
}:
{
  options.accounts.email.accounts = lib.mkOption {
    type = lib.types.attrsOf (
      lib.types.submodule (
        {
          name,
          ...
        }:
        {
          options.imapmaildir = {
            enable = lib.mkEnableOption "imapmaildir";
            idle = lib.mkOption {
              type = lib.types.submodule {
                options = {
                  enable = lib.mkOption {
                    type = lib.types.bool;
                    default = true;
                    description = "Enable continuously listening to remote changes via imap IDLE and to local changes via inotify";
                  };
                  timeoutSeconds = lib.mkOption {
                    type = lib.types.int;
                    default = 29 * 60;
                    description = "Time until imap IDLE is cancelled and reissued to prevent connection timeout";
                  };
                };
              };
            };
            oauth2 = lib.mkOption {
              type = lib.types.submodule {
                options = {
                  enable = lib.mkEnableOption "OAuth2 login via OAUTHBEARER or XOAUTH2 instead of a password";
                  tokenCommand = lib.mkOption {
                    type =
                      with lib.types;
                      nullOr (either str (listOf str));
                    default = null;
                    description = "Command printing a valid OAuth2 access token. Defaults to the account's passwordCommand";
                    example = [
                      "oama"
                      "access"
                      "me@example.com"
                    ];
                  };
                };
              };
              default = { };
            };
            onChange = lib.mkOption {
              type = lib.types.listOf lib.types.str;
              default = [ ];
              description = "Command to execute per batch of remote changes applied to a mailbox. Gets the changes as tab-separated lines on stdin";
              example = [
                "notify-send"
                "new mail"
              ];
            };
            onChangeFailure = lib.mkOption {
              type = lib.types.enum [
                "exit"
                "warn"
              ];
              default = "exit";
              description = "Whether a failing onChange command stops syncing or only logs a warning";
            };
            changeHookThrottleMilliseconds = lib.mkOption {
              type = lib.types.nullOr lib.types.ints.positive;
              default = null;
              description = "Time after the first change of a batch, until onChange runs. Defaults to 100";
            };
            changeHookMaxPending = lib.mkOption {
              type = lib.types.nullOr lib.types.ints.positive;
              default = null;
              description = "Changes queued per hook, beyond which they get dropped for a single overflow line. Defaults to 10000";
            };
            shutdownTimeoutSeconds = lib.mkOption {
              type = lib.types.nullOr lib.types.ints.positive;
              default = null;
              description = "Time after a shutdown signal, after which unfinished communication with the server gets abandoned. Defaults to 5";
            };
            mailboxes = lib.mkOption {
              type = lib.types.listOf lib.types.str;
              default = [ ];
            };
            service = lib.mkOption {
              type = lib.types.submodule {
                options = {
                  name = lib.mkOption {
                    type = lib.types.str;
                    default = "imapmaildir-sync-${name}";
                  };
                  intervalSec = lib.mkOption {
                    type = lib.types.int;
                    default = 5 * 60;
                    description = "Time in seconds between consecutive sync runs. Only relevant if idle is disabled";
                  };
                  extraConfig = lib.mkOption {
                    type = lib.types.attrs;
                  };
                };
              };
            };
          };
        }
      )
    );
  };

  config =
    let
      accounts =
        let
          inherit (config.accounts.email) accounts;
          isEnabled = _: account: account.imapmaildir.enable;
        in
        lib.filterAttrs isEnabled accounts;
      enable = (builtins.length (builtins.attrNames accounts)) > 0;

      mkService =
        name: account:
        let
          inherit (account.imapmaildir) idle service;
        in
        {
          "${service.name}" = lib.mkMerge [
            (lib.mkIf
              (idle.enable && !(service.extraConfig ? Install && service.extraConfig.Install ? WantedBy))
              {
                Install = {
                  WantedBy = [
                    "default.target"
                  ];
                };
              }
            )
            service.extraConfig
            {
              Unit = {
                Description = "mail sync via imapmaildir for account ${name}";
              };
              Service = {
                Type = "exec";
                # imapmaildir finishes pending changes and hooks on SIGTERM, so they must not get
                # killed alongside it
                KillMode = "mixed";
                ExecStart = "${
                  lib.getExe self.packages.${pkgs.stdenv.hostPlatform.system}.imapmaildir
                } --account ${name} sync${lib.optionalString idle.enable " --idle"}";
              };
            }
          ];
        };

      mkTimer =
        name: account:
        let
          inherit (account.imapmaildir.service) name;
        in
        lib.mkIf (!account.imapmaildir.idle.enable) {
          "${name}" = {
            Unit = {
              Description = "timer for ${name}";
            };
            Timer = {
              OnStartupSec = 0;
              OnUnitInactiveSec = account.imapmaildir.service.intervalSec;
            };
            Install = {
              WantedBy = [
                "timers.target"
              ];
            };
          };
        };

      mkConfig =
        name: account:
        let
          toml = pkgs.formats.toml { };
          toCmd = cmd: if builtins.isList cmd then cmd else [ cmd ];
          inherit (account.imapmaildir) oauth2;
        in
        {
          "imapmaildir/accounts/${name}.toml" = {
            source =
              let
                inherit (account.imap) port;
              in
              toml.generate "${name}.toml" (
                {
                  inherit (account.imap) host;
                  # todo: assert use tls
                  port = if builtins.isNull port then 993 else port;
                  inherit (account.imapmaildir) mailboxes;
                  idle_timeout = "${builtins.toString account.imapmaildir.idle.timeoutSeconds}s";
                  on_local_change = account.imapmaildir.onChange;
                  on_local_change_failure = account.imapmaildir.onChangeFailure;
                  maildir_base_path = account.maildir.absPath;
                  auth =
                    if oauth2.enable then
                      {
                        type = "OAuth2";
                        user = account.userName;
                        token_cmd = toCmd (
                          if builtins.isNull oauth2.tokenCommand then
                            account.passwordCommand
                          else
                            oauth2.tokenCommand
                        );
                      }
                    else
                      {
                        type = "Plain";
                        user = account.userName;
                        # todo: assert passwordCommand not null
                        password_cmd = toCmd account.passwordCommand;
                      };
                }
                // lib.optionalAttrs (!builtins.isNull account.imapmaildir.changeHookThrottleMilliseconds) {
                  change_hook_throttle = "${builtins.toString account.imapmaildir.changeHookThrottleMilliseconds}ms";
                }
                // lib.optionalAttrs (!builtins.isNull account.imapmaildir.changeHookMaxPending) {
                  change_hook_max_pending = account.imapmaildir.changeHookMaxPending;
                }
                // lib.optionalAttrs (!builtins.isNull account.imapmaildir.shutdownTimeoutSeconds) {
                  shutdown_timeout = "${builtins.toString account.imapmaildir.shutdownTimeoutSeconds}s";
                }
              );
          };
        };

      mapAccounts = f: lib.mkMerge (builtins.attrValues (builtins.mapAttrs f accounts));
    in
    lib.mkIf enable {
      xdg.configFile = mapAccounts mkConfig;
      systemd.user = {
        services = mapAccounts mkService;
        timers = mapAccounts mkTimer;
      };
    };
}
