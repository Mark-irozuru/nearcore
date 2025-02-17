#!/usr/bin/env bash
DIR="$( cd "$( dirname "${BASH_SOURCE[0]}" )" >/dev/null 2>&1 && pwd )"
cd ${DIR}
./build.sh
export HOST_DIR=${DIR}/../../../..
srcdir=nearcore
if [[ -n "${BUILDKITE}" ]]; then
    srcdir=runtime-params-estimator-qemu
fi
docker run \
     --rm --mount type=bind,source=$HOST_DIR,target=/host \
     --cap-add=SYS_PTRACE --security-opt seccomp=unconfined \
     -i -t rust-emu \
     /usr/bin/env bash -c "
set -ex
cd /host/${srcdir}/runtime/runtime-params-estimator
pushd test-contract; ./build.sh; popd
mkdir /tmp/data
cargo build --release --package runtime-params-estimator --features required
./emu-cost/counter_plugin/qemu-x86_64 -cpu Westmere-v1 -plugin file=./emu-cost/counter_plugin/libcounter.so ../../target/release/runtime-params-estimator --home /tmp/data --additional-accounts-num=200000 --accounts-num 20000 --iters 1 --warmup-iters 1
"
