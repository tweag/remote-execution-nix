#!/usr/bin/env bash

# A hacky script to test the BuildDerivation worker op, which only activates on distributed builds.
# You'll have to change the hardcoded paths to your own home directory.

cd crates/drv-adapter && cargo build && cd ../..
cargo build

TEST_ROOT=/home/jneeman/tweag/nix-rev2/test-remote-store

#builder="ssh-ng://localhost?remote-store=$TEST_ROOT/remote?remote-program=/home/jneeman/tweag/nix-remote-rust/nix-remote-rust.sh - - 1 1 foo"
builder="ssh-ng://localhost?remote-program=/home/jneeman/tweag/nix-rev2/nix-remote-rust.sh - - 1 1 foo"

#chmod -R +w $TEST_ROOT/remote || true
#rm -rf $TEST_ROOT/remote/* || true
chmod -R +w $TEST_ROOT/local || true
#rm -rf $TEST_ROOT/local/* || true

nix build -L -v -o result --max-jobs 0 \
 --builders-use-substitutes \
 .#test-slow \
 --store $TEST_ROOT/local \
 --builders "$builder" --impure

 #.#test \
 #--expr '(builtins.getFlake "nixpkgs").legacyPackages.${builtins.currentSystem}.writeText "current-time" "${builtins.toString builtins.currentTime}"' \

