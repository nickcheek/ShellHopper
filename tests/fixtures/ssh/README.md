# Local SSH verification fixture

This fixture runs one OpenSSH server in Docker and publishes it only on a random
`127.0.0.1` port. It creates fresh client and server Ed25519 keys under `/tmp`,
uses a fixture-only known-hosts file, and never reads or writes `~/.ssh/config`
or `~/.ssh/known_hosts`.

## Start it

From the repository root:

```sh
./tests/fixtures/ssh/start.sh
```

Copy the three `export` commands printed by the script into the shell where you
will run verification. Confirm the isolated endpoint directly if wanted:

```sh
ssh -F "$SHELLHOPPER_SSH_CONFIG" shellhopper-fixture 'printf "fixture ready\n"'
```

## Manual ShellHopper verification

Build normally, then launch the binary with the fixture's disposable home. This
keeps ShellHopper's own config and generated shell files out of your real home:

```sh
cargo build
HOME="$SHELLHOPPER_SSH_FIXTURE_HOME" \
SHELLHOPPER_SSH_CONFIG="$SHELLHOPPER_SSH_CONFIG" \
./target/debug/shellhopper
```

The sidebar will contain only `shellhopper-fixture` under `Fixture`.

1. Select the fixture host. Enter `printf 'queued:%s\n' "$$"` and immediately
   click **Run on selected hosts**. One SSH tab should open, wait for readiness,
   print the command result, and remain connected.
2. Run `printf 'reuse:%s\n' "$$"` on the same selected host. The existing tab
   should be reused rather than opening a duplicate; the shell PID should match.
3. Type `exit` in that SSH tab and wait for the connection to close. Run
   `printf 'reconnect:%s\n' "$$"` from the sidebar. The same tab should reconnect,
   print the queued result with a new shell PID, and remain open.

## Opt-in Rust integration check

An ignored test exercises the same readiness, command queue, session reuse, and
reconnect path against this loopback fixture. It runs only when explicitly
requested after exporting the variables printed by `start.sh`:

```sh
cargo test ssh_fixture_queues_reuses_and_reconnects -- --ignored --nocapture
```

## Stop it

```sh
./tests/fixtures/ssh/stop.sh
```

The stop script removes the container and its `/tmp` key directory. The local
Docker image contains no credentials and can be removed separately at any time.
