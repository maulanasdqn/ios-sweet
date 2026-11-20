// Programs compiled offline from CoreAnimation's metallib (tools/air-spec:
// airspec.py specialises a function for one constant set, air2glsl.py turns
// it into GLSL, skyprog compiles the pair with mesa's ir3 and emits the a6xx
// program registers). SkyPrograms.h (generated) holds the table.
#include <stdint.h>

// one pushed constant-buffer range: Metal buffer `buffer` bytes
// [start, start + size) -> const file at byte offset `offset`
typedef struct {
    uint32_t buffer, start, size, offset;
} SkyProgUBO;

typedef struct {
    const char *name;
    const uint32_t *vs;
    uint32_t vs_dwords;
    const uint32_t *fs;
    uint32_t fs_dwords;
    const uint32_t (*regs)[2];
    uint32_t nregs;
    uint32_t vs_constlen;
    const uint32_t *vs_imm;
    uint32_t vs_imm_dwords, vs_imm_base; // base in dwords
    const SkyProgUBO *vs_ubo;
    uint32_t vs_nubo;
    uint32_t fs_constlen;
    const uint32_t *fs_imm;
    uint32_t fs_imm_dwords, fs_imm_base;
    const SkyProgUBO *fs_ubo;
    uint32_t fs_nubo;
    const uint8_t *attrs; // VFD fetch order -> Metal vertex attribute (255: unused hole)
    uint32_t nattrs;
    uint32_t vs_preload, fs_preload; // instruction cache units for CP_LOAD_STATE6
    uint32_t ntex;                   // texture/sampler slots (SP_PS_CONFIG NTEX/NSAMP)
} SkyProg;

// texture slot source: Metal fragment texture index, or SKY_TEX_FB (a copy
// of the colour attachment for framebuffer fetch)
#define SKY_TEX_FB 0xff
// sampler slot source: Metal fragment sampler index, or a constexpr sampler
// (Metal's 64-bit constant sampler state)
typedef struct {
    uint8_t metal; // 0xff: constant
    uint64_t bits;
} SkyProgSampler;

typedef struct {
    const char *vkey, *fkey; // function name[sorted non-zero constants]
    const SkyProg *prog;
    uint8_t ntex, nsamp;
    uint8_t tex[16];
    SkyProgSampler samp[16];
    uint8_t fbfetch;
} SkyProgEntry;
