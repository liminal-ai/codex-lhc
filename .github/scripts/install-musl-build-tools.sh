#!/usr/bin/env bash
set -euo pipefail

: "${TARGET:?TARGET environment variable is required}"
: "${GITHUB_ENV:?GITHUB_ENV environment variable is required}"

linux_uapi_version="6.8.0-25.25cross1"
case "${TARGET}" in
  x86_64-unknown-linux-musl)
    arch="x86_64"
    probe_arch_macro="__x86_64__"
    linux_uapi_package="linux-libc-dev-amd64-cross_${linux_uapi_version}_all.deb"
    linux_uapi_sha256="bc504dcc35c15ff606df44ca081d0abaa613b2f3b7a56896d6211eced1368af3"
    linux_uapi_component="universe"
    ;;
  aarch64-unknown-linux-musl)
    arch="aarch64"
    probe_arch_macro="__aarch64__"
    linux_uapi_package="linux-libc-dev-arm64-cross_${linux_uapi_version}_all.deb"
    linux_uapi_sha256="6a5a00b8ba8de66862e05493def87a5cbbb23b949601e45a5e3cbde56505cb6b"
    linux_uapi_component="main"
    ;;
  *)
    echo "Unexpected musl target: ${TARGET}" >&2
    exit 1
    ;;
esac

apt_update_args=()
if [[ -n "${APT_UPDATE_ARGS:-}" ]]; then
  # shellcheck disable=SC2206
  apt_update_args=(${APT_UPDATE_ARGS})
fi

apt_install_args=()
if [[ -n "${APT_INSTALL_ARGS:-}" ]]; then
  # shellcheck disable=SC2206
  apt_install_args=(${APT_INSTALL_ARGS})
fi

sudo apt-get update "${apt_update_args[@]}"
sudo apt-get install -y "${apt_install_args[@]}" ca-certificates curl musl-tools pkg-config libcap-dev g++ clang libc++-dev libc++abi-dev lld xz-utils

libcap_version="2.75"
libcap_sha256="de4e7e064c9ba451d5234dd46e897d7c71c96a9ebf9a0c445bc04f4742d83632"
libcap_tarball_name="libcap-${libcap_version}.tar.xz"
libcap_download_url="https://mirrors.edge.kernel.org/pub/linux/libs/security/linux-privs/libcap2/${libcap_tarball_name}"

# Use the musl toolchain as the Rust linker to avoid Zig injecting its own CRT.
if command -v "${arch}-linux-musl-gcc" >/dev/null; then
  musl_linker="$(command -v "${arch}-linux-musl-gcc")"
elif command -v musl-gcc >/dev/null; then
  musl_linker="$(command -v musl-gcc)"
else
  echo "musl gcc not found after install; arch=${arch}" >&2
  exit 1
fi

zig_target="${TARGET/-unknown-linux-musl/-linux-musl}"
runner_temp="${RUNNER_TEMP:-/tmp}"
tool_root="${runner_temp}/codex-musl-tools-${TARGET}"
mkdir -p "${tool_root}"

# musl intentionally does not ship Linux UAPI headers. Use a pinned Ubuntu
# cross-toolchain package for the target architecture instead of exposing the
# host's /usr/include tree. The extracted directory contains the exported
# linux/, asm/, and asm-generic/ closure, but no glibc userspace headers.
linux_uapi_url="https://archive.ubuntu.com/ubuntu/pool/${linux_uapi_component}/c/cross-toolchain-base/${linux_uapi_package}"
linux_uapi_deb="${tool_root}/${linux_uapi_package}"
linux_uapi_root="${tool_root}/linux-uapi-${linux_uapi_version}-${linux_uapi_sha256}"
linux_uapi_include="${linux_uapi_root}/usr/${arch}-linux-gnu/include"

if [[ ! -f "${linux_uapi_deb}" ]]; then
  curl -fsSL "${linux_uapi_url}" -o "${linux_uapi_deb}"
fi
echo "${linux_uapi_sha256}  ${linux_uapi_deb}" | sha256sum -c -

if [[ ! -f "${linux_uapi_include}/linux/sched.h" ||
      ! -f "${linux_uapi_include}/linux/loop.h" ||
      ! -f "${linux_uapi_include}/asm/types.h" ]]; then
  mkdir -p "${linux_uapi_root}"
  dpkg-deb -x "${linux_uapi_deb}" "${linux_uapi_root}"
fi

for required_header in linux/sched.h linux/loop.h asm/types.h; do
  if [[ ! -f "${linux_uapi_include}/${required_header}" ]]; then
    echo "Pinned Linux UAPI package is missing ${required_header}; target=${TARGET}" >&2
    exit 1
  fi
done
for forbidden_header in features.h stdio.h; do
  if [[ -e "${linux_uapi_include}/${forbidden_header}" ]]; then
    echo "Linux UAPI closure unexpectedly contains userspace header ${forbidden_header}" >&2
    exit 1
  fi
done

libcap_root="${tool_root}/libcap-${libcap_version}"
libcap_src_root="${libcap_root}/src"
libcap_prefix="${libcap_root}/prefix"
libcap_pkgconfig_dir="${libcap_prefix}/lib/pkgconfig"

if [[ ! -f "${libcap_prefix}/lib/libcap.a" ]]; then
  mkdir -p "${libcap_src_root}" "${libcap_prefix}/lib" "${libcap_prefix}/include/sys" "${libcap_prefix}/include/linux" "${libcap_pkgconfig_dir}"
  libcap_tarball="${libcap_root}/${libcap_tarball_name}"

  curl -fsSL "${libcap_download_url}" -o "${libcap_tarball}"
  echo "${libcap_sha256}  ${libcap_tarball}" | sha256sum -c -

  tar -xJf "${libcap_tarball}" -C "${libcap_src_root}"
  libcap_source_dir="${libcap_src_root}/libcap-${libcap_version}"
  make -C "${libcap_source_dir}/libcap" -j"$(nproc)" \
    CC="${musl_linker}" \
    AR=ar \
    RANLIB=ranlib

  cp "${libcap_source_dir}/libcap/libcap.a" "${libcap_prefix}/lib/libcap.a"
  cp "${libcap_source_dir}/libcap/include/uapi/linux/capability.h" "${libcap_prefix}/include/linux/capability.h"
  cp "${libcap_source_dir}/libcap/../libcap/include/sys/capability.h" "${libcap_prefix}/include/sys/capability.h"

  cat > "${libcap_pkgconfig_dir}/libcap.pc" <<EOF
prefix=${libcap_prefix}
exec_prefix=\${prefix}
libdir=\${prefix}/lib
includedir=\${prefix}/include

Name: libcap
Description: Linux capabilities
Version: ${libcap_version}
Libs: -L\${libdir} -lcap
Cflags: -I\${includedir}
EOF
fi

sysroot=""
if command -v zig >/dev/null; then
  zig_bin="$(command -v zig)"
  cc="${tool_root}/zigcc"
  cxx="${tool_root}/zigcxx"

  cat >"${cc}" <<EOF
#!/usr/bin/env bash
set -euo pipefail

args=()
skip_next=0
pending_include=0
for arg in "\$@"; do
  if [[ "\${pending_include}" -eq 1 ]]; then
    pending_include=0
    if [[ "\${arg}" == /usr/include || "\${arg}" == /usr/include/* ]]; then
      # Keep host-only headers available, but after the target sysroot headers.
      args+=("-idirafter" "\${arg}")
    else
      args+=("-I" "\${arg}")
    fi
    continue
  fi

  if [[ "\${skip_next}" -eq 1 ]]; then
    skip_next=0
    continue
  fi
  case "\${arg}" in
    --target)
      skip_next=1
      continue
      ;;
    --target=*|-target=*|-target)
      # Drop any explicit --target/-target flags. Zig expects -target and
      # rejects Rust triples like *-unknown-linux-musl.
      if [[ "\${arg}" == "-target" ]]; then
        skip_next=1
      fi
      continue
      ;;
    -I)
      pending_include=1
      continue
      ;;
    -I/usr/include|-I/usr/include/*)
      # Avoid making glibc headers win over musl headers.
      args+=("-idirafter" "\${arg#-I}")
      continue
      ;;
    -Wp,-U_FORTIFY_SOURCE)
      # aws-lc-sys emits this GCC preprocessor forwarding form in debug
      # builds, but zig cc expects the define flag directly.
      args+=("-U_FORTIFY_SOURCE")
      continue
      ;;
  esac
  args+=("\${arg}")
done

# Zig enables UBSan for debug C builds by default. Rust links these objects
# without Zig's sanitizer runtime, so keep native dependencies uninstrumented.
exec "${zig_bin}" cc -target "${zig_target}" "\${args[@]}" -fno-sanitize=undefined
EOF
  cat >"${cxx}" <<EOF
#!/usr/bin/env bash
set -euo pipefail

args=()
skip_next=0
pending_include=0
for arg in "\$@"; do
  if [[ "\${pending_include}" -eq 1 ]]; then
    pending_include=0
    if [[ "\${arg}" == /usr/include || "\${arg}" == /usr/include/* ]]; then
      # Keep host-only headers available, but after the target sysroot headers.
      args+=("-idirafter" "\${arg}")
    else
      args+=("-I" "\${arg}")
    fi
    continue
  fi

  if [[ "\${skip_next}" -eq 1 ]]; then
    skip_next=0
    continue
  fi
  case "\${arg}" in
    --target)
      # Drop explicit --target and its value: we always pass zig's -target below.
      skip_next=1
      continue
      ;;
    --target=*|-target=*|-target)
      # Zig expects -target and rejects Rust triples like *-unknown-linux-musl.
      if [[ "\${arg}" == "-target" ]]; then
        skip_next=1
      fi
      continue
      ;;
    -I)
      pending_include=1
      continue
      ;;
    -I/usr/include|-I/usr/include/*)
      # Avoid making glibc headers win over musl headers.
      args+=("-idirafter" "\${arg#-I}")
      continue
      ;;
    -Wp,-U_FORTIFY_SOURCE)
      # aws-lc-sys emits this GCC forwarding form in debug builds; zig c++
      # expects the define flag directly.
      args+=("-U_FORTIFY_SOURCE")
      continue
      ;;
  esac
  args+=("\${arg}")
done

# Zig enables UBSan for debug C++ builds by default. Rust links these objects
# without Zig's sanitizer runtime, so keep native dependencies uninstrumented.
exec "${zig_bin}" c++ -target "${zig_target}" "\${args[@]}" -fno-sanitize=undefined
EOF
  chmod +x "${cc}" "${cxx}"

  sysroot="$("${zig_bin}" cc -target "${zig_target}" -print-sysroot 2>/dev/null || true)"
else
  cc="${musl_linker}"

  if command -v "${arch}-linux-musl-g++" >/dev/null; then
    cxx="$(command -v "${arch}-linux-musl-g++")"
  elif command -v musl-g++ >/dev/null; then
    cxx="$(command -v musl-g++)"
  else
    cxx="${cc}"
  fi
fi

if [[ -n "${sysroot}" && "${sysroot}" != "/" ]]; then
  echo "BORING_BSSL_SYSROOT=${sysroot}" >> "$GITHUB_ENV"
  boring_sysroot_var="BORING_BSSL_SYSROOT_${TARGET}"
  boring_sysroot_var="${boring_sysroot_var//-/_}"
  echo "${boring_sysroot_var}=${sysroot}" >> "$GITHUB_ENV"
fi

linux_uapi_cflag="-idirafter${linux_uapi_include}"
cflags="-pthread ${linux_uapi_cflag}"
cxxflags="-pthread ${linux_uapi_cflag}"
if [[ "${TARGET}" == "aarch64-unknown-linux-musl" ]]; then
  # BoringSSL enables -Wframe-larger-than=25344 under clang and treats warnings as errors.
  cflags="${cflags} -Wno-error=frame-larger-than"
  cxxflags="${cxxflags} -Wno-error=frame-larger-than"
fi

# Compile through the selected target compiler before exporting it. -idirafter
# keeps musl's compiler-provided userspace headers ahead of the pinned UAPI
# closure while still resolving Linux and target asm headers needed by bwrap.
if ! printf '%s\n' \
  '#include <linux/sched.h>' \
  '#include <linux/loop.h>' \
  '#include <asm/types.h>' \
  "#ifndef ${probe_arch_macro}" \
  '#error target compiler architecture does not match Linux UAPI closure' \
  '#endif' \
  'int codex_linux_uapi_probe(void) {' \
  '  return (int)(sizeof(struct clone_args) + sizeof(struct loop_info64) + sizeof(__u64));' \
  '}' | "${cc}" -pthread "${linux_uapi_cflag}" -x c -c -o "${tool_root}/linux-uapi-probe.o" -; then
  echo "Linux UAPI compile probe failed; target=${TARGET} include=${linux_uapi_include}" >&2
  exit 1
fi

echo "CFLAGS=${cflags}" >> "$GITHUB_ENV"
echo "CXXFLAGS=${cxxflags}" >> "$GITHUB_ENV"
echo "CC=${cc}" >> "$GITHUB_ENV"
echo "TARGET_CC=${cc}" >> "$GITHUB_ENV"
target_cc_var="CC_${TARGET}"
target_cc_var="${target_cc_var//-/_}"
echo "${target_cc_var}=${cc}" >> "$GITHUB_ENV"
echo "CXX=${cxx}" >> "$GITHUB_ENV"
echo "TARGET_CXX=${cxx}" >> "$GITHUB_ENV"
target_cxx_var="CXX_${TARGET}"
target_cxx_var="${target_cxx_var//-/_}"
echo "${target_cxx_var}=${cxx}" >> "$GITHUB_ENV"

cargo_linker_var="CARGO_TARGET_${TARGET^^}_LINKER"
cargo_linker_var="${cargo_linker_var//-/_}"
echo "${cargo_linker_var}=${musl_linker}" >> "$GITHUB_ENV"

echo "CMAKE_C_COMPILER=${cc}" >> "$GITHUB_ENV"
echo "CMAKE_CXX_COMPILER=${cxx}" >> "$GITHUB_ENV"
echo "CMAKE_ARGS=-DCMAKE_HAVE_THREADS_LIBRARY=1 -DCMAKE_USE_PTHREADS_INIT=1 -DCMAKE_THREAD_LIBS_INIT=-pthread -DTHREADS_PREFER_PTHREAD_FLAG=ON" >> "$GITHUB_ENV"

# Allow pkg-config resolution during cross-compilation.
echo "PKG_CONFIG_ALLOW_CROSS=1" >> "$GITHUB_ENV"
pkg_config_path="${libcap_pkgconfig_dir}"
if [[ -n "${PKG_CONFIG_PATH:-}" ]]; then
  pkg_config_path="${pkg_config_path}:${PKG_CONFIG_PATH}"
fi
echo "PKG_CONFIG_PATH=${pkg_config_path}" >> "$GITHUB_ENV"
pkg_config_path_var="PKG_CONFIG_PATH_${TARGET}"
pkg_config_path_var="${pkg_config_path_var//-/_}"
echo "${pkg_config_path_var}=${libcap_pkgconfig_dir}" >> "$GITHUB_ENV"
pkg_config_libdir_var="PKG_CONFIG_LIBDIR_${TARGET}"
pkg_config_libdir_var="${pkg_config_libdir_var//-/_}"
# Do not let musl cross-builds resolve native libraries from the host glibc
# pkg-config directories. libcap is the only target package provided here.
echo "${pkg_config_libdir_var}=${libcap_pkgconfig_dir}" >> "$GITHUB_ENV"

if [[ -n "${sysroot}" && "${sysroot}" != "/" ]]; then
  echo "PKG_CONFIG_SYSROOT_DIR=${sysroot}" >> "$GITHUB_ENV"
  pkg_config_sysroot_var="PKG_CONFIG_SYSROOT_DIR_${TARGET}"
  pkg_config_sysroot_var="${pkg_config_sysroot_var//-/_}"
  echo "${pkg_config_sysroot_var}=${sysroot}" >> "$GITHUB_ENV"
fi
