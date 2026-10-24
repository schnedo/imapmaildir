# imapmaildir

![maintenance-status](https://img.shields.io/badge/maintenance-experimental-blue.svg)

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
> production use. There will be breaking changes without further notice.

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
                    nix.settings = {
                        substituters = [
                            "https://schnedo.cachix.org"
                        ];
                        trusted-public-keys = [
                          "schnedo.cachix.org-1:SOK0hFcugOr4sde3Rw8+DsbdMHbg/R358weVZMhosC4="
                        ];
                    };
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
        # Optional OAuth2 login (OAUTHBEARER or XOAUTH2), e.g. for Gmail without app passwords.
        # imapmaildir does not run the OAuth2 flow itself. Use a helper like oama or pizauth,
        # that prints a valid access token.
        oauth2 = {
            enable = true;
            # Defaults to the account's passwordCommand
            tokenCommand = [ "oama" "access" "me@example.com" ];
        };
        # Optional hook informed about remote changes, see "Change hooks" below
        onChange = [ "notify-send" "new mail" ];
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

### Change hooks

`imapmaildir` can inform other programs, e.g. mail indexers like
[notmuch](https://notmuchmail.org) or notification daemons, about changes of
the remote state, that got applied to the local maildir. `onChange`
(`on_local_change` in the account config) runs once per batch of changes of a
mailbox and gets them as tab-separated lines on stdin. Changes get merged per
mail, e.g. a downloaded mail flagged right after is reported as a single
download. A batch gets passed to the hook `change_hook_throttle` (default
`100ms`) after its first change.

Each line has the format

```text
<mailbox>\t<type>\t<path>\t<details>\t<old_path>
```

| type         | meaning                                         | details example           |
| ------------ | ----------------------------------------------- | ------------------------- |
| `downloaded` | mail new on remote, stored locally              | `uid=12 flags=FS`         |
| `uploaded`   | local mail now on remote, file renamed with UID | `uid=7 flags=S`           |
| `deleted`    | mail deleted on remote, removed locally         | `uid=3`                   |
| `flags`      | flags changed on remote, applied locally        | `uid=5 added=F removed=S` |
| `overflow`   | too many changes, rescan the mailbox            |                           |

- `path` is the absolute current path of the mail, or the removed one for
  `deleted`. `old_path` is the previous path for `uploaded` and `flags`, as
  both rename the file, and empty otherwise.
- `details` are space-separated `key=value` pairs. Flags are maildir letters,
  including ones without IMAP equivalent like `P`.
- Backslashes, tabs and newlines in mailbox names and paths are escaped as
  `\\`, `\t` and `\n`.
- Only changes of the remote state get reported. Housekeeping renames, e.g.
  moving delivered mails from `new` to `cur`, are not, so paths of earlier
  lines may become stale, if other programs changed them before the hook fired.
- The first download into an empty maildir gets reported as a single
  `overflow` line instead of one line per mail.
- At most `change_hook_max_pending` (default 10000) changes get queued per
  mailbox. Beyond that, changes of the mailbox get dropped and a single
  `overflow` line asks for a rescan. Syncing never waits for the hook.

If the hook fails to start or exits with a non-zero status, `imapmaildir`
stops syncing. Set `onChangeFailure` (`on_local_change_failure`) to `"warn"` to
only log a warning instead.

Example hook script:

```sh
while IFS="$(printf '\t')" read -r mailbox type path details old_path; do
    case "$type" in
        downloaded) notify-send "new mail in $mailbox" ;;
        overflow) notmuch new ;;
    esac
done
```
