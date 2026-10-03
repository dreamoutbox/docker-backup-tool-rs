#!/bin/sh
# Prepare the SFTP user for the dvb integration tests.
#
# Mounted into `/etc/sftp.d/`, which the atmoz/sftp entrypoint runs at startup
# after the users are created. This image version only picks up keys dropped into
# `/home/<user>/.ssh/keys/` *while the user is being created*, which a bind mount
# cannot do without also creating root-owned parent directories.
#
# Three ownership rules have to hold simultaneously, and sshd checks all of them:
#   * the chroot directory itself must be root-owned and not writable by others,
#   * `~/.ssh` and `authorized_keys` must be owned by the user (StrictModes),
#   * the upload directory must be owned by the user so SFTP can write to it.
set -eu

USER_NAME="${SFTP_USER:-dvb}"
HOME_DIR="/home/${USER_NAME}"
KEY_SRC="${SFTP_KEY_SRC:-/run/keys/dvb_test_key.pub}"
UPLOAD_DIR="${SFTP_UPLOAD_DIR:-backups}"

# The chroot root stays root-owned; sshd refuses to start otherwise.
chown root:root "${HOME_DIR}"
chmod 755 "${HOME_DIR}"

# Key material belongs to the user.
mkdir -p "${HOME_DIR}/.ssh"
cat "${KEY_SRC}" > "${HOME_DIR}/.ssh/authorized_keys"
chmod 700 "${HOME_DIR}/.ssh"
chmod 600 "${HOME_DIR}/.ssh/authorized_keys"
chown -R "${USER_NAME}" "${HOME_DIR}/.ssh"

# Writable target for uploads, inside the chroot.
mkdir -p "${HOME_DIR}/${UPLOAD_DIR}"
chown -R "${USER_NAME}" "${HOME_DIR}/${UPLOAD_DIR}"
chmod 700 "${HOME_DIR}/${UPLOAD_DIR}"
