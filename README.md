# imapmaildir

`imapmaildir` is a mail client, that uses IMAP to pull the mail from your mail server to your local maildir directory.

`imapmaildir` is inspired by [offlineIMAP](https://github.com/OfflineIMAP/offlineimap) and [mbsync](https://github.com/gburd/isync).
While these tools run periodically for syncing mails, `imapmaildir` is intended to run as a background service and listen to changes to local and remote mail.
As such it is the ideal mail receiving agent for mail clients like [mutt](https://gitlab.com/muttmua/mutt) or [aerc](https://git.sr.ht/~rjarry/aerc).

[!CAUTION]
The project is still in early development and not ready for production use
