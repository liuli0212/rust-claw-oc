#!/bin/bash
export PATH=$PATH:~/.cargo/bin
cargo run << 'IN'
please run a bash command to echo hello world
exit
IN
