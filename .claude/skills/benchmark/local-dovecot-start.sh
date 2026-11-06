#!/bin/sh
mkdir -p /tmp/imaptest/run /tmp/imaptest/home
REPO=$(cd "$(dirname "$0")/../../.." && pwd)
cat > /tmp/imaptest/dovecot.conf <<CONF
dovecot_config_version = 2.4.3
dovecot_storage_version = 2.4.3
base_dir = /tmp/imaptest/run
state_dir = /tmp/imaptest/run
log_path = /tmp/imaptest/dovecot.log
default_internal_user =
default_internal_group =
default_login_user =
protocols {
  imap = yes
}
service imap-login {
  inet_listener imap {
    port = 0
  }
  inet_listener imaps {
    port = 31993
    ssl = yes
  }
  chroot =
}
service anvil {
  chroot =
}
mail_driver = maildir
mailbox_list_layout = fs
mail_path = ~/mail
mail_home = /tmp/imaptest/home/%{user | lower}
mail_uid = 1000
mail_gid = 100
namespace inbox {
  inbox = yes
  separator = /
}
passdb static {
  password = password
}
userdb static {
  fields {
    uid = 1000
    gid = 100
    home = /tmp/imaptest/home/%{user | lower}
  }
}
ssl_server_cert_file = $REPO/tests/fixtures/mock/certificate.crt
ssl_server_key_file = $REPO/tests/fixtures/mock/private_key.pem
CONF
cd /tmp/imaptest && (timeout 1800 nix shell nixpkgs#dovecot -c dovecot -F -c /tmp/imaptest/dovecot.conf </dev/null >/tmp/imaptest/stdout 2>&1 &)
sleep 20
tail -n1 /tmp/imaptest/dovecot.log
