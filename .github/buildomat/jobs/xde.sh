#!/bin/bash
#:
#: name = "opte-xde"
#: variety = "basic"
#: target = "helios"
#: rust_toolchain = "nightly"
#: output_rules = [
#:   "=/work/debug/xde.dbg",
#:   "=/work/debug/xde.dbg.sha256",
#:   "=/work/release/xde",
#:   "=/work/release/xde.sha256",
#: ]
#:

set -o errexit
set -o pipefail
set -o xtrace

#
# TGT_BASE allows one to run this more easily in their local
# environment:
#
#   TGT_BASE=/var/tmp ./xde.sh
#
TGT_BASE=${TGT_BASE:=/work}

DBG_SRC=target/x86_64-unknown-unknown/debug
DBG_TGT=$TGT_BASE/debug

REL_SRC=target/x86_64-unknown-unknown/release
REL_TGT=$TGT_BASE/release

mkdir -p $DBG_TGT $REL_TGT

function header {
	echo "# ==== $* ==== #"
}

cargo --version
rustc --version

pushd xde

header "check style"
ptime -m cargo fmt -- --check

header "build xde (debug)"
ptime -m ./build-debug.sh

header "build xde (release)"
ptime -m ./build.sh

#
# Inspect the kernel module for bad relocations in case the old
# codegen issue ever shows its face again.
#
if elfdump $DBG_SRC/xde.dbg | grep GOTPCREL; then
	echo "found GOTPCREL relocation in debug build"
	exit 1
fi

if elfdump $REL_SRC/xde | grep GOTPCREL; then
	echo "found GOTPCREL relocation in release build"
	exit 1
fi

cp $DBG_SRC/xde.dbg $DBG_TGT/
sha256sum $DBG_TGT/xde.dbg > $DBG_TGT/xde.dbg.sha256

cp $REL_SRC/xde $REL_TGT/
sha256sum $REL_TGT/xde > $REL_TGT/xde.sha256
