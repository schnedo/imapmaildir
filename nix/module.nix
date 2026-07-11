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
            onChange = lib.mkOption {
              type = lib.types.listOf lib.types.str;
              default = [ ];
              description = "Command to execute after remote changes got synced";
              example = [
                "notify-send"
                "new mail"
              ];
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
        in
        {
          "imapmaildir/accounts/${name}.toml" = {
            # todo: onChange as soon as idle is implemented
            source =
              let
                inherit (account.imap) port;
              in
              toml.generate "${name}.toml" {
                inherit (account.imap) host;
                # todo: assert use tls
                port = if builtins.isNull port then 993 else port;
                inherit (account.imapmaildir) mailboxes;
                idle_timeout = "${builtins.toString account.imapmaildir.idle.timeoutSeconds}s";
                on_local_change = account.imapmaildir.onChange;
                maildir_base_path = account.maildir.absPath;
                auth = {
                  type = "Plain";
                  user = account.userName;
                  # todo: assert passwordCommand not null
                  password_cmd =
                    if builtins.isList account.passwordCommand then
                      account.passwordCommand
                    else
                      [ account.passwordCommand ];
                };
              };
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
