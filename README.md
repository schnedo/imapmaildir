# imapmaildir

`imapmaildir` is a mail client, that uses IMAP to pull the mail from your mail
server to your local maildir directory.

`imapmaildir` is inspired by
[offlineIMAP](https://github.com/OfflineIMAP/offlineimap) and
[mbsync](https://github.com/gburd/isync). While these tools run periodically
for syncing mails, `imapmaildir` is intended to run as a background service and
listen to changes to local and remote mail. As such it is the ideal mail
receiving agent for mail clients like [mutt](https://gitlab.com/muttmua/mutt)
or [aerc](https://git.sr.ht/~rjarry/aerc).

> [!CAUTION]
> The project is still in early development and not ready for
> production use. Expect bugs and crashes.

## Usage

The supported way of using `imapmaildir` is by using the [home
manager](https://github.com/nix-community/home-manager) module provided by this
repository's flake.

### Installation

Example for usage in a flake based nixos config:

```nix
{
    inputs = {
        nixpkgs.url = "github:nixos/nixpkgs?ref=nixos-unstable";
        imapmaildir = {
            url = "github:schnedo/imapmaildir";
            inputs.nixpkgs.follows = "nixpkgs";
        };
    };

    outputs = { imapmaildir, nixpkgs, ... }: {
        nixosConfigurations.mySystem = nixpkgs.lib.nixosSystem {
            system = "x86_64-linux";
            modules = [
                {
                    home-manager.sharedModules = [
                        imapmaildir.homeModules.imapmaildir
                    ];
                }
            ];
        };
    };
}
```

### Configuration

Configure using the home-manager module:

```nix
{
    accounts.email.accounts.<name>.imapmaildir = {
        enable = true;
        # List the mailboxes you want to sync. Currently, there is no autodiscover.
        mailboxes = [ ];
        # Optional service configuration.
        # There are default values in place so you do not need to configure this.
        service = {
            # Name of the systemd service without '.service' suffix
            name = "custom-service-name";
            # Interval between syncs
            intervalSec = 60;
            # Custom service config as in https://home-manager-options.extranix.com/?query=systemd.user.services.%3Cname%3E&release=master
            extraConfig = {};
        };
    };
}
```
