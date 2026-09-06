#!/bin/bash
# Recreate ~/playground for the pre-flight demo (sparse files: 0 real disk use).
P=${1:-$HOME/playground}; rm -rf "$P"; mkdir -p "$P/build/objs" "$P/src"; cd "$P" || exit 1
git init -q && echo "*.o" > .gitignore && echo 'fn main(){}' > src/main.rs && git add -A && git -c user.name=demo -c user.email=demo@local commit -qm init
for i in $(seq 1 1204); do truncate -s 4096 "build/objs/f$i.o"; done; truncate -s 300M build/big.bin
echo "API_KEY=abc" > .env; echo "edited" >> src/main.rs
echo "playground ready at $P"
