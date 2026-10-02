# Test-only SSH key

`dvb` authenticates to SFTP by shelling out to `ssh`, and `ssh` in batch mode
cannot do password authentication without `sshpass`. The SFTP integration test
in `tests/sftp.rs` therefore uses a key pair instead.

**This private key is committed on purpose and is worthless outside the test.**
It grants access only to an ephemeral `atmoz/sftp` container that exists for the
lifetime of one `cargo test` run. Never reuse it anywhere, and never point a real
server at `tests/fixtures/sftp_test_key.pub`.