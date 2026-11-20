#!/bin/sh
# runs inside skyios-mesadev (debian:bookworm + mesa build deps) with this
# directory at /w: adds skyprog (VS+FS SPIR-V -> ir3 + a6xx program state) to
# the tree next to ir3c and builds it incrementally in /w/build2
set -e
cd /w/mesa-src
cp /w/skyprog.cc src/freedreno/ir3/sky_prog.cc
if ! grep -q sky_prog src/freedreno/ir3/meson.build; then
  cat >> src/freedreno/ir3/meson.build <<'EOM'

# skyios: VS+FS SPIR-V -> ir3 + a6xx program register state (host tool)
sky_prog = executable(
  'skyprog',
  ['sky_prog.cc', freedreno_xml_header_files],
  link_with: libfreedreno_ir3,
  dependencies: [idep_libir3decode, idep_mesautil, idep_nir, idep_vtn, idep_libfreedreno_common, dep_dl, dep_unwind],
  include_directories: [inc_freedreno, inc_include, inc_src],
  cpp_args: ['-Wno-missing-field-initializers', '-Wno-c99-designator', '-fpermissive'],
  build_by_default: true,
)
EOM
fi
ninja -C /w/build2 src/freedreno/ir3/skyprog >/w/ninja3.log 2>&1 || { grep -E "error|Error" /w/ninja3.log | head -40; tail -5 /w/ninja3.log; exit 1; }
echo built
