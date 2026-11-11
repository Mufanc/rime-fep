#!/bin/sh
set -eu

build_dir=$(mktemp -d)
export CMAKE_BUILD_PARALLEL_LEVEL=4

trap 'rm -rf "$build_dir"' EXIT
cd "$build_dir"

fetch() {
    curl --fail --location --retry 3 "$1" --output "$2"
    printf '%s  %s\n' "$3" "$2" | sha256sum --check
}

fetch https://archives.boost.io/release/1.85.0/source/boost_1_85_0.tar.bz2 boost.tar.bz2 \
    7009fe1faa1697476bdc7027703a2badb84e849b7b0baad5086b087b971f8617
fetch https://github.com/jbeder/yaml-cpp/archive/refs/tags/0.8.0.tar.gz yaml-cpp.tar.gz \
    fbe74bbdcee21d656715688706da3c8becfd946d92cd44705cc6098bb23b3a16
fetch https://github.com/google/leveldb/archive/refs/tags/1.23.tar.gz leveldb.tar.gz \
    9a37f8a6174f09bd622bc723b55881dc541cd50747cbd08831c2a82d620f6d76
fetch https://github.com/BYVoid/OpenCC/archive/refs/tags/ver.1.1.9.tar.gz opencc.tar.gz \
    ad4bcd8d87219a240a236d4a55c9decd2132a9436697d2882ead85c8939b0a99
fetch https://github.com/rime/librime/archive/refs/tags/1.17.0.tar.gz librime.tar.gz \
    a60274da5d8b8a7187e6c7e9ba5023334ed7bdd182535e93c4e96de8cf188377

tar -xjf boost.tar.bz2 boost_1_85_0/boost

for archive in yaml-cpp leveldb opencc librime; do
    tar -xzf "$archive.tar.gz"
done

cat > musl.cmake <<'EOF'
set(CMAKE_SYSTEM_NAME Linux)
set(CMAKE_SYSTEM_PROCESSOR x86_64)
set(CMAKE_CROSSCOMPILING_EMULATOR /usr/bin/env)
set(CMAKE_C_COMPILER x86_64-linux-musl-gcc)
set(CMAKE_CXX_COMPILER x86_64-linux-musl-g++)
set(CMAKE_AR x86_64-linux-musl-ar)
set(CMAKE_RANLIB x86_64-linux-musl-ranlib)
set(CMAKE_INSTALL_PREFIX /opt/rime-musl CACHE PATH "")
set(CMAKE_PREFIX_PATH /opt/rime-musl)
set(CMAKE_C_FLAGS_RELEASE "-Os -DNDEBUG -ffunction-sections -fdata-sections")
set(CMAKE_CXX_FLAGS_RELEASE "-Os -DNDEBUG -ffunction-sections -fdata-sections")
set(CMAKE_EXE_LINKER_FLAGS "-static -Wl,--gc-sections")
EOF

cmake -S yaml-cpp-0.8.0 -B build-yaml -DCMAKE_TOOLCHAIN_FILE="$build_dir/musl.cmake" \
    -DCMAKE_BUILD_TYPE=Release -DYAML_CPP_BUILD_TESTS=OFF -DYAML_CPP_BUILD_TOOLS=OFF \
    -DYAML_BUILD_SHARED_LIBS=OFF
cmake --build build-yaml
cmake --install build-yaml

cmake -S leveldb-1.23 -B build-leveldb -DCMAKE_TOOLCHAIN_FILE="$build_dir/musl.cmake" \
    -DCMAKE_BUILD_TYPE=Release -DBUILD_SHARED_LIBS=OFF -DLEVELDB_BUILD_TESTS=OFF \
    -DLEVELDB_BUILD_BENCHMARKS=OFF -DHAVE_SNAPPY=OFF
cmake --build build-leveldb
cmake --install build-leveldb

cmake -S OpenCC-ver.1.1.9 -B build-opencc -DCMAKE_TOOLCHAIN_FILE="$build_dir/musl.cmake" \
    -DCMAKE_BUILD_TYPE=Release -DBUILD_SHARED_LIBS=OFF -DENABLE_GTEST=OFF -DBUILD_TESTING=OFF
cmake --build build-opencc
cmake --install build-opencc
cp build-opencc/deps/marisa-0.2.6/libmarisa.a /opt/rime-musl/lib/
cp -R OpenCC-ver.1.1.9/deps/marisa-0.2.6/include/. /opt/rime-musl/include/

cmake -S librime-1.17.0 -B build-rime -DCMAKE_TOOLCHAIN_FILE="$build_dir/musl.cmake" \
    -DCMAKE_BUILD_TYPE=Release -DBUILD_SHARED_LIBS=OFF -DBUILD_STATIC=ON \
    -DBUILD_TEST=OFF -DENABLE_LOGGING=OFF -DENABLE_EXTERNAL_PLUGINS=OFF \
    -DBOOST_ROOT="$build_dir/boost_1_85_0" -DBoost_NO_SYSTEM_PATHS=ON
cmake --build build-rime
cmake --install build-rime

# Keep the C++ runtime beside Rime so the linker uses Rust's musl libc.
cp "$(x86_64-linux-musl-g++ -print-file-name=libstdc++.a)" /opt/rime-musl/lib/

for library in rime opencc marisa leveldb yaml-cpp stdc++; do
    test -s "/opt/rime-musl/lib/lib$library.a"
done
