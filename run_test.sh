#!/bin/bash
export PATH=$PATH:~/.cargo/bin
cargo run << 'IN'
ls -l
exit
IN
