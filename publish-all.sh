#!/bin/bash

set -e

DIR="$( cd "$( dirname "${BASH_SOURCE[0]}" )" >/dev/null 2>&1 && pwd )"

DIRS="$DIR/engine $DIR/macro $DIR $DIR/gen/build"

for CRATE in $DIRS; do
  pushd $CRATE
  echo "Publish: $CRATE"
  cargo publish
  popd
  sleep 3 # sometimes crates.io takes a moment, and our
          # crates are interdependent.
done
