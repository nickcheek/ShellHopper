#!/bin/sh
set -eu

test -s /run/shellhopper/authorized_keys
test -s /run/shellhopper/ssh_host_ed25519_key

cp /run/shellhopper/ssh_host_ed25519_key /run/sshd/ssh_host_ed25519_key
chmod 600 /run/sshd/ssh_host_ed25519_key
cp /run/shellhopper/authorized_keys /home/shellhopper/.ssh/authorized_keys
chown shellhopper:shellhopper /home/shellhopper/.ssh/authorized_keys
chmod 600 /home/shellhopper/.ssh/authorized_keys

# Keep the account unlocked while password authentication remains disabled.
printf 'shellhopper:%s\n' "$(head -c 24 /dev/urandom | base64)" | chpasswd

exec /usr/sbin/sshd -D -e -f /etc/ssh/sshd_config
