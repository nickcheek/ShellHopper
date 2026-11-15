#!/bin/sh
set -eu

container=shellhopper-ssh-fixture

if ! docker container inspect "$container" >/dev/null 2>&1; then
    echo "The ShellHopper SSH fixture is not running."
    exit 0
fi

state=$(docker container inspect \
    --format '{{ index .Config.Labels "org.shellhopper.fixture.state" }}' \
    "$container")
fixture=$(docker container inspect \
    --format '{{ index .Config.Labels "org.shellhopper.fixture" }}' \
    "$container")

if [ "$fixture" != true ]; then
    echo "Refusing to remove an unrecognized container named $container" >&2
    exit 1
fi

docker rm -f "$container" >/dev/null
case "$state" in
    /tmp/shellhopper-ssh-fixture.*|/private/tmp/shellhopper-ssh-fixture.*)
        rm -rf -- "$state"
        ;;
    *)
        echo "Container stopped, but refusing to remove unexpected state path: $state" >&2
        exit 1
        ;;
esac

echo "Stopped the fixture and removed its ephemeral keys."
