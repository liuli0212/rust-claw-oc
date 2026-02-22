#!/bin/bash
export PATH=$PATH:~/.cargo/bin
cargo run << 'IN'
Write a bash script to analyze the rust code in the src/ directory using clippy and save the output to report.txt
exit
IN
