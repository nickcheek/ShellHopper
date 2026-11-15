#!/bin/sh
set -eu

fixture_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
container=shellhopper-ssh-fixture
image=shellhopper-ssh-fixture:local

for command in docker ssh-keygen; do
    command -v "$command" >/dev/null 2>&1 || {
        echo "Missing required command: $command" >&2
        exit 1
    }
done

if docker container inspect "$container" >/dev/null 2>&1; then
    echo "The fixture is already running. Stop it first with $fixture_dir/stop.sh" >&2
    exit 1
fi

state=$(mktemp -d /tmp/shellhopper-ssh-fixture.XXXXXX)
cleanup_on_error() {
    docker rm -f "$container" >/dev/null 2>&1 || true
    rm -rf -- "$state"
}
trap cleanup_on_error HUP INT TERM EXIT

mkdir -p "$state/home/Library/Application Support/ShellHopper"
ssh-keygen -q -t ed25519 -N '' -C shellhopper-fixture-client -f "$state/client_key"
ssh-keygen -q -t ed25519 -N '' -C shellhopper-fixture-host -f "$state/ssh_host_ed25519_key"
cp "$state/client_key.pub" "$state/authorized_keys"

docker build -t "$image" "$fixture_dir"
docker run --detach --rm \
    --name "$container" \
    --label org.shellhopper.fixture=true \
    --label "org.shellhopper.fixture.state=$state" \
    --publish 127.0.0.1::22 \
    --volume "$state/authorized_keys:/run/shellhopper/authorized_keys:ro" \
    --volume "$state/ssh_host_ed25519_key:/run/shellhopper/ssh_host_ed25519_key:ro" \
    "$image" >/dev/null

attempt=0
until docker exec "$container" test -s /run/sshd.pid >/dev/null 2>&1; do
    attempt=$((attempt + 1))
    if [ "$attempt" -ge 50 ]; then
        echo "Fixture SSH server did not become ready." >&2
        docker logs "$container" >&2 || true
        exit 1
    fi
    sleep 0.1
done

port=$(docker port "$container" 22/tcp | awk -F: 'NR == 1 { print $NF }')
host_key=$(awk '{ print $1 " " $2 }' "$state/ssh_host_ed25519_key.pub")
printf '[127.0.0.1]:%s %s\n' "$port" "$host_key" > "$state/known_hosts"

cat > "$state/ssh_config" <<EOF
Host shellhopper-fixture
    HostName 127.0.0.1
    Port $port
    User shellhopper
    IdentityFile $state/client_key
    IdentitiesOnly yes
    BatchMode yes
    StrictHostKeyChecking yes
    UserKnownHostsFile $state/known_hosts
    GlobalKnownHostsFile /dev/null
    LogLevel ERROR
EOF

cat > "$state/home/Library/Application Support/ShellHopper/config.toml" <<'EOF'
groups = ["Fixture"]

[[hosts]]
name = "shellhopper-fixture"
address = "shellhopper-fixture"
user = ""
port = 22
identity_file = ""
group = "Fixture"
EOF

trap - HUP INT TERM EXIT

echo "Loopback SSH fixture is ready on 127.0.0.1:$port"
echo
echo "Copy these exports into the shell where you run verification:"
printf "export SHELLHOPPER_SSH_CONFIG='%s/ssh_config'\n" "$state"
printf "export SHELLHOPPER_SSH_FIXTURE_HOME='%s/home'\n" "$state"
echo "export SHELLHOPPER_RUN_SSH_FIXTURE=1"
echo
echo "Stop and erase the ephemeral keys with: $fixture_dir/stop.sh"
