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
        # Optional syncing of keywords with notmuch tags, see "Syncing tags with notmuch" below
        notmuch = {
            enable = true;
            # Tags of keywords, that are named differently
            tags = { "$Junk" = "spam"; };
        };
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

Hooks run without the terminal in their own process group, so pressing Ctrl-C
does not interrupt them. Their output gets logged, stdout as info and stderr as
warning. Processes, that a hook leaves running in the background, should
redirect their output, as it cannot get logged anymore once `imapmaildir`
exited.

If the hook fails to start or exits with a non-zero status, `imapmaildir`
stops syncing after finishing pending work, like when shutting down, and exits
with an error. Set `onChangeFailure` (`on_local_change_failure`) to `"warn"` to
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

### Syncing tags with notmuch

`imapmaildir` can sync keywords of mails, i.e. IMAP flags without system flag
like `$Junk`, with tags of [notmuch](https://notmuchmail.org) in both
directions. On Gmail, it syncs labels via `X-GM-LABELS` instead. Enable it with
`notmuch.enable` or a `[notmuch]` section in the account config:

```toml
[notmuch]
# notmuch configuration file and profile. Default to notmuch's lookup, e.g.
# $NOTMUCH_CONFIG or $XDG_CONFIG_HOME/notmuch/default/config
# config = "/home/me/.notmuch-config"
# profile = "work"
# Tags, that are not synced, in addition to the ones notmuch manages itself:
# unread, replied, flagged, draft, passed, deleted, new, attachment, signed and
# encrypted
ignore_tags = ["todo"]

# Tags of keywords, that are named differently
[notmuch.tags]
"$Junk" = "spam"
```

- System flags like `\Seen` are no keywords. notmuch syncs them with tags like
  `unread` via the maildir file names, if `maildir.synchronize_flags` is set.
- `imapmaildir` indexes the mails it syncs itself, so that `notmuch new` is not
  needed for them. New mails get the tags of `new.tags`, except synced ones,
  which only come from the server. Therefore the maildir must be below the mail
  root of notmuch. Tags of other mails, e.g. ones moved in by other programs,
  get synced once notmuch indexed them.
- Keywords and tags changed on both sides keep the server's state.
- Keywords are case-insensitive on the server, but tags are not. A tag differing
  from an existing keyword only in case, e.g. `Work` for `work`, takes the
  server's spelling once the server reports the mail's keywords.
- When syncing a mailbox with notmuch for the first time, after changing
  `ignore_tags` or `tags` or after the notmuch database got replaced, e.g. by
  `notmuch compact` or recreating it, the tags of all mails get merged with the
  keywords of the server: missing keywords get added as tags, tags only in
  notmuch get synced to the server. Nothing gets removed. Synced tags of
  `new.tags`, e.g. `inbox` on Gmail, stay local then, as `notmuch new` adds them
  to every mail. They get synced again, once removed and added again. The same
  applies to new mails uploaded to the server. Add tags, that should stay
  local, to `ignore_tags` before.
- Tags belong to messages, so tag changes get synced to all copies of a message.
- Tag changes in notmuch get noticed by watching the files of its database with
  inotify. Therefore the database must not be changed on another host, e.g. via
  a network filesystem.
- Tags the server refuses, e.g. as they are too long, stay local.
- Mails deleted on the server stay in notmuch for five minutes, so that moving
  them between synced mailboxes keeps their tags.
- `inbox`, which notmuch adds to new mails, is only synced on Gmail.
- Gmail labels map to tags of the same name, except for system labels:
  `\Inbox` is `inbox`, `\Important` is `important` and `\Sent` is `sent`.
  `\Starred` and `\Draft` are synced as flags. Others, e.g. `\Trash`, are not
  synced, as they move mails out of the mailbox of all mails. As labels of a
  mail are only complete in Gmail's mailbox of all mails, `[Gmail]/All Mail` or
  its localized name must be the only synced mailbox.
- Syncing tags needs libnotmuch 5.4 (notmuch 0.32) or later, which gets loaded
  at runtime.

### Shutting down

On `SIGTERM`, `SIGINT` or `SIGHUP`, `imapmaildir` stops syncing and finishes
pending work: changes already detected get synced, the local state gets stored
and pending change hooks run. An interrupted initial download resumes on the next start.
Communication with the server, that did not finish `shutdownTimeoutSeconds`
(`shutdown_timeout` in the account config, default `5s`) after the signal, gets
abandoned. Mails being uploaded then may get uploaded again on the next start.
A second signal terminates `imapmaildir` immediately.
