// Adreno 618 command processor bring-up, after the GMU has granted GPU access
// (OOB gpu_set acked): GPU memory in the adreno SMMU, SQE microcode,
// ringbuffer, CP_ME_INIT and a first command. Follows Linux a6xx hw_init()
// (research/linux-ref/drivers/gpu/drm/msm/adreno/a6xx_gpu.c), a618 subset.

use crate::xnu::{Dma, Mmio, delay_us};

#[path = "cp3d.rs"]
mod cp3d;
#[path = "cpuser.rs"]
mod cpuser;

const SMMU: u64 = 0x0504_0000;
const SMMU_SIZE: usize = 0x10000;
const PAGE: usize = 0x1000;
const CB_BASE: usize = 0x8000;
const CB: usize = 0;
const SMR: usize = 0;
const GPU_SID: u32 = 0;
const VMID: u32 = 1;

// GPU virtual layout, below the 4 GiB TCR limit and away from the UCHE GMEM
// range at 1 MiB
const IOVA_BASE: u64 = 0x0100_0000;
const TABLES: usize = 3 * PAGE;
const SQE_OFF: usize = 0;
const SQE_MAX: usize = 0x8000;
const RING_OFF: usize = SQE_OFF + SQE_MAX;
const RING_BYTES: usize = 0x8000;
const SCRATCH_OFF: usize = RING_OFF + RING_BYTES;
const FENCE_OFF: usize = SCRATCH_OFF + 0x40;
const CCU_TS_OFF: usize = SCRATCH_OFF + 0x80;
const DST_OFF: usize = SCRATCH_OFF + PAGE;
const DST_W: u32 = 64;
const DST_H: u32 = 64;
const DST_PITCH: u32 = DST_W * 4;
const DST_BYTES: usize = (DST_PITCH * DST_H) as usize;
// 3D self test: shaders, vertices (cp3d.rs)
const D3_OFF: usize = DST_OFF + DST_BYTES;
const D3_BYTES: usize = 5 * PAGE;
const WINDOW: usize = D3_OFF + D3_BYTES;

#[cfg(feature = "sqefw")]
static SQE_FW: &[u8] = include_bytes!(env!("SWEET_SQE_FW"));
#[cfg(not(feature = "sqefw"))]
static SQE_FW: &[u8] = &[];

const RBBM_VBIF_CLIENT_QOS_CNTL: usize = 0x0010;
const RBBM_GBIF_HALT: usize = 0x0016;
const RBBM_INT_0_STATUS: usize = 0x0201;
const RBBM_STATUS: usize = 0x0210;
const CP_RB_BASE: usize = 0x0800;
const CP_RB_CNTL: usize = 0x0802;
const CP_RB_RPTR: usize = 0x0806;
const CP_RB_WPTR: usize = 0x0807;
const CP_SQE_CNTL: usize = 0x0808;
const CP_HW_FAULT: usize = 0x0821;
const CP_INTERRUPT_STATUS: usize = 0x0823;
const CP_PROTECT_STATUS: usize = 0x0824;
const CP_STATUS_1: usize = 0x0825;
const CP_SQE_INSTR_BASE: usize = 0x0830;
const CP_ADDR_MODE_CNTL: usize = 0x0842;
const CP_SCRATCH0: usize = 0x0883;
const CP_ROQ_THRESHOLDS_1: usize = 0x08c1;
const CP_ROQ_THRESHOLDS_2: usize = 0x08c2;
const CP_MEM_POOL_SIZE: usize = 0x08c3;
const CP_SQE_STAT_ADDR: usize = 0x0908;
const CP_SQE_STAT_DATA: usize = 0x0909;
const CP_AHB_CNTL: usize = 0x098d;
const VSC_ADDR_MODE_CNTL: usize = 0x0c01;
const UCHE_ADDR_MODE_CNTL: usize = 0x0e00;
const UCHE_WRITE_RANGE_MAX: usize = 0x0e05;
const UCHE_WRITE_THRU_BASE: usize = 0x0e07;
const UCHE_TRAP_BASE: usize = 0x0e09;
const UCHE_GMEM_RANGE_MIN: usize = 0x0e0b;
const UCHE_GMEM_RANGE_MAX: usize = 0x0e0d;
const UCHE_CACHE_WAYS: usize = 0x0e17;
const UCHE_FILTER_CNTL: usize = 0x0e18;
const UCHE_CLIENT_PF: usize = 0x0e19;
const GBIF_HALT: usize = 0x3c45;
const GRAS_A2D_BLT_CNTL: u32 = 0x8400;
const GRAS_A2D_DEST_TL: u32 = 0x8405;
const GRAS_ADDR_MODE_CNTL: usize = 0x8601;
const RB_A2D_BLT_CNTL: u32 = 0x8c00;
const RB_A2D_PIXEL_CNTL: u32 = 0x8c01;
const RB_A2D_DEST_BUFFER_INFO: u32 = 0x8c17;
const RB_A2D_DEST_FLAG_BUFFER_BASE: u32 = 0x8c20;
const RB_A2D_CLEAR_COLOR_DW0: u32 = 0x8c2c;
const RB_DBG_ECO_CNTL: u32 = 0x8e04;
const RB_ADDR_MODE_CNTL: usize = 0x8e05;
const RB_CCU_CNTL: u32 = 0x8e07;
const VPC_ADDR_MODE_CNTL: usize = 0x9601;
const PC_ADDR_MODE_CNTL: usize = 0x9e01;
const VFD_ADDR_MODE_CNTL: usize = 0xa601;
const SP_A2D_OUTPUT_INFO: u32 = 0xacc0;
const SP_ADDR_MODE_CNTL: usize = 0xae01;
const TPL1_ADDR_MODE_CNTL: usize = 0xb601;
const HLSQ_ADDR_MODE_CNTL: usize = 0xbe05;
const RBBM_SECVID_TSB_ADDR_MODE_CNTL: usize = 0xf810;
const RBBM_SECVID_TRUST_CNTL: usize = 0xf400;
const RBBM_SECVID_TSB_TRUSTED_BASE: usize = 0xf800;
const RBBM_SECVID_TSB_TRUSTED_SIZE: usize = 0xf802;
const RBBM_SECVID_TSB_CNTL: usize = 0xf803;

// MSM_GPU_RB_CNTL_DEFAULT for a 32 KiB ring and 32-byte blocks, plus NO_UPDATE
// (a618 has no expanded APRIV, so the rptr shadow write-back stays off)
const RB_CNTL: u32 = 12 | (2 << 8) | (1 << 27);

const CP_NOP: u32 = 0x10;
const CP_WAIT_FOR_IDLE: u32 = 0x26;
const CP_BLIT: u32 = 0x2c;
const CP_MEM_WRITE: u32 = 0x3d;
const CP_EVENT_WRITE: u32 = 0x46;
const CP_ME_INIT: u32 = 0x48;
const CP_SET_SECURE_MODE: u32 = 0x66;

// vgt_event_type
const CACHE_FLUSH_TS: u32 = 0x04;
const PC_CCU_INVALIDATE_DEPTH: u32 = 0x18;
const PC_CCU_INVALIDATE_COLOR: u32 = 0x19;
const PC_CCU_FLUSH_DEPTH_TS: u32 = 0x1c;
const PC_CCU_FLUSH_COLOR_TS: u32 = 0x1d;
const FD_LABEL: u32 = 0x3f;

const BLIT_OP_SCALE: u32 = 3;
const FMT6_8_8_8_8_UNORM: u32 = 0x30;
// a6xx_gen1 (a618): one CCU, sysmem/bypass colour cache at GMEM 64 KiB
// (mesa ccu_offset_bypass = num_ccu * A6XX_CCU_DEPTH_SIZE)
const CCU_CNTL_BYPASS: u32 = (0x10000 >> 12) << 23;
// mesa freedreno_devices.py, GPUId 615/616/618/619 raw_magic_regs, written
// once per context by fd6_emit_static_non_context_regs(). No QCTDD04536579
// quirk on a618, so RB_DBG_ECO_CNTL is not toggled around CP_BLIT.
const A618_MAGIC: [(u32, u32); 12] = [
    (0x9804, 0x1f),         // PC_MODE_CNTL
    (0x9805, 0),            // PC_POWER_CNTL
    (0xa0f8, 0),            // VFD_POWER_CNTL
    (0xb600, 0x0010_8000),  // TPL1_DBG_ECO_CNTL
    (0x8600, 0x0000_0880),  // GRAS_DBG_ECO_CNTL
    (0xae03, 0x0000_0430),  // SP_CHICKEN_BITS
    (0xae00, 0),            // SP_DBG_ECO_CNTL
    (0xbe04, 0x0008_0000),  // HLSQ_DBG_ECO_CNTL
    (0x9600, 0),            // VPC_DBG_ECO_CNTL
    (0x0e12, 1),            // UCHE_UNKNOWN_0E12
    (0x8e01, 1),            // RB_RBP_CNTL
    (RB_DBG_ECO_CNTL, 0x0410_0000),
];
// a subset of fd6_emit_static_context_regs()
const STATIC_CTX: [(u32, u32); 3] = [
    (0x8811, 0x10), // RB_MODE_CNTL
    (0x8110, 0x2),  // GRAS_MODE_CNTL
    (0x80a0, 0x2),  // GRAS_SC_CNTL ccusinglecachelinesize 2
];
const UCHE_TRAP: u64 = 0x0001_ffff_ffff_f000;
const GMEM_MIN: u64 = 0x10_0000;
const GMEM_SIZE: u64 = 0x8_0000;

fn parity(v: u32) -> u32 {
    let x = v ^ (v >> 4) ^ (v >> 8) ^ (v >> 12) ^ (v >> 16) ^ (v >> 20) ^ (v >> 24) ^ (v >> 28);
    (0x9669 >> (x & 0xf)) & 1
}

fn pkt4(reg: u32, cnt: u32) -> u32 {
    (4 << 28) | cnt | (parity(cnt) << 7) | ((reg & 0x3ffff) << 8) | (parity(reg) << 27)
}

fn pkt7(op: u32, cnt: u32) -> u32 {
    (7 << 28) | cnt | (parity(cnt) << 15) | ((op & 0x7f) << 16) | (parity(op) << 23)
}

struct Gpu(Mmio);

impl Gpu {
    fn read(&self, reg: usize) -> u32 {
        self.0.read(reg * 4)
    }

    fn write(&self, reg: usize, v: u32) {
        self.0.write(reg * 4, v)
    }

    fn write64(&self, reg: usize, v: u64) {
        self.write(reg, v as u32);
        self.write(reg + 1, (v >> 32) as u32);
    }
}

struct GpuMem {
    dma: Dma,
}

impl GpuMem {
    fn iova(&self, off: usize) -> u64 {
        IOVA_BASE + off as u64
    }

    fn w32(&self, off: usize, v: u32) {
        self.dma.w32(TABLES + off, v)
    }

    fn r32(&self, off: usize) -> u32 {
        self.dma.r32(TABLES + off)
    }
}

fn pte_page(pa: u64) -> u64 {
    pa | (1 << 10) | (1 << 6) | 0b11
}

fn pte_table(pa: u64) -> u64 {
    pa | 0b11
}

// Same context bank programming as gmumem::setup() (cb4, SID 5), here for the
// GPU stream (SID 0) in cb0 with its own VMID.
fn mem_setup() -> Option<GpuMem> {
    let smmu = Mmio::map(SMMU, SMMU_SIZE)?;
    let Some(dma) = Dma::alloc(TABLES + WINDOW) else {
        crate::log!("SWEET CP: contiguous alloc failed");
        return None;
    };
    let iova = IOVA_BASE as usize;
    dma.w64(((iova >> 30) & 0x1ff) * 8, pte_table(dma.pa + PAGE as u64));
    dma.w64(PAGE + ((iova >> 21) & 0x1ff) * 8, pte_table(dma.pa + 2 * PAGE as u64));
    for i in 0..WINDOW / PAGE {
        dma.w64(2 * PAGE + (((iova >> 12) & 0x1ff) + i) * 8, pte_page(dma.pa + (TABLES + i * PAGE) as u64));
    }
    dma.clean();
    let l1 = dma.pa;
    let cb = CB_BASE + CB * PAGE;
    smmu.write(cb, 0);
    smmu.write(PAGE + 0x800 + 4 * CB, 1);
    smmu.write(PAGE + 4 * CB, (1 << 16) | (0xf << 12) | (3 << 8) | VMID);
    smmu.write(cb + 0x10, (7 << 15) | 2);
    smmu.write(cb + 0x30, (1 << 23) | 32);
    smmu.write(cb + 0x20, l1 as u32);
    smmu.write(cb + 0x24, (l1 >> 32) as u32);
    smmu.write(cb + 0x38, 0x44);
    smmu.write(cb + 0x3c, 0);
    smmu.write(cb + 0x58, smmu.read(cb + 0x58));
    smmu.write(0x68, 0);
    smmu.write(0x70, 0);
    for _ in 0..1000 {
        if smmu.read(0x74) & 1 == 0 {
            break;
        }
        delay_us(1);
    }
    // CFCFG=0 (terminate/abort, not stall): a fault returns DECERR to the GPU
    // so the CP continues past faulting user IBs instead of hanging forever.
    // HUPCF=1: allow transactions past a pending fault (already set).
    smmu.write(cb, (1 << 6) | (1 << 2) | (1 << 1) | 1);
    smmu.write(0xc00 + 4 * SMR, CB as u32);
    smmu.write(0x800 + 4 * SMR, (1 << 31) | GPU_SID);
    crate::log!(
        "SWEET CP: {:#x} bytes at pa {:#x}, iova {IOVA_BASE:#x} via cb{CB} (sctlr {:#010x}), smr[{SMR}] {:#010x} s2cr {:#010x}",
        dma.size,
        dma.pa,
        smmu.read(cb),
        smmu.read(0x800 + 4 * SMR),
        smmu.read(0xc00 + 4 * SMR)
    );
    Some(GpuMem { dma })
}

fn smmu_fault() {
    let Some(smmu) = Mmio::map(SMMU, SMMU_SIZE) else {
        return;
    };
    let cb = CB_BASE + CB * PAGE;
    crate::log!(
        "SWEET CP: cb{CB} fsr {:#x} far {:#x} fsynr0 {:#x}; gfsr {:#x} gfsynr1 {:#x}",
        smmu.read(cb + 0x58),
        (smmu.read(cb + 0x64) as u64) << 32 | smmu.read(cb + 0x60) as u64,
        smmu.read(cb + 0x68),
        smmu.read(0x48),
        smmu.read(0x54)
    );
}

// a6xx_ucode_check_version(), a630_sqe.fw: patched (low nibble 0xa) or >= 1.90
fn sqe_ok(words: &[u32]) -> bool {
    words.len() > 3 && ((words[0] & 0xf == 0xa && words[2] & 0xf >= 1) || words[0] & 0xfff >= 0x190)
}

struct Ring<'a> {
    mem: &'a GpuMem,
    wptr: u32,
}

impl Ring<'_> {
    fn emit(&mut self, v: u32) {
        self.mem.w32(RING_OFF + 4 * self.wptr as usize, v);
        self.wptr = (self.wptr + 1) % (RING_BYTES / 4) as u32;
    }

    fn reg(&mut self, reg: u32, vals: &[u32]) {
        self.emit(pkt4(reg, vals.len() as u32));
        for &v in vals {
            self.emit(v);
        }
    }

    fn event(&mut self, ev: u32, at: Option<(u64, u32)>) {
        match at {
            Some((a, v)) => {
                self.emit(pkt7(CP_EVENT_WRITE, 4));
                self.emit(ev);
                self.emit(a as u32);
                self.emit((a >> 32) as u32);
                self.emit(v);
            }
            None => {
                self.emit(pkt7(CP_EVENT_WRITE, 1));
                self.emit(ev);
            }
        }
    }

    fn wfi(&mut self) {
        self.emit(pkt7(CP_WAIT_FOR_IDLE, 0));
    }

    // a6xx_submit() tail: CACHE_FLUSH_TS writes the seqno once all earlier
    // work and the cache flush are done (no IRQ bit, we poll)
    fn fence(&mut self, seq: u32) {
        let a = self.mem.iova(FENCE_OFF);
        self.event(CACHE_FLUSH_TS, Some((a, seq)));
    }

    // Per-frame submit: done when the CP consumed the ring and the trailing
    // CACHE_FLUSH_TS wrote `seq` (RBBM_STATUS is not used: 0110c read
    // 0x00c00001 right after an idle fill). 2 us polls, 1 s limit.
    fn publish(&mut self, gpu: &Gpu) {
        self.mem.dma.clean();
        unsafe { core::arch::asm!("dsb sy", options(nostack, preserves_flags)) };
        gpu.write(CP_RB_WPTR, self.wptr);
    }

    // a6xx_flush() + a6xx_idle(): publish the ring, then wait for the CP to
    // drain it and for RBBM to report no busy block besides the AHB master
    fn submit(&mut self, gpu: &Gpu, what: &str) -> bool {
        self.mem.dma.clean();
        unsafe { core::arch::asm!("dsb sy", options(nostack, preserves_flags)) };
        gpu.write(CP_RB_WPTR, self.wptr);
        for i in 0..20_000 {
            let rptr = gpu.read(CP_RB_RPTR);
            if rptr == self.wptr && gpu.read(RBBM_STATUS) & !1 == 0 {
                crate::log!("SWEET CP: {what}: done (rptr {rptr}, {} us)", i * 10);
                return true;
            }
            delay_us(10);
        }
        crate::log!(
            "SWEET CP: {what}: TIMEOUT rptr {} wptr {} status {:#010x} int0 {:#010x} hw_fault {:#010x} cp_int {:#010x} protect {:#010x} status_1 {:#010x}",
            gpu.read(CP_RB_RPTR),
            self.wptr,
            gpu.read(RBBM_STATUS),
            gpu.read(RBBM_INT_0_STATUS),
            gpu.read(CP_HW_FAULT),
            gpu.read(CP_INTERRUPT_STATUS),
            gpu.read(CP_PROTECT_STATUS),
            gpu.read(CP_STATUS_1)
        );
        gpu.write(CP_SQE_STAT_ADDR, 1);
        crate::log!("SWEET CP: sqe pc {:#06x}", gpu.read(CP_SQE_STAT_DATA));
        smmu_fault();
        false
    }
}

pub fn bringup() -> bool {
    let Some(gpu) = Mmio::map(crate::gpu::GPU, 0x40000).map(Gpu) else {
        return false;
    };
    if SQE_FW.len() <= 4 || SQE_FW.len() - 4 > SQE_MAX || SQE_FW.len() % 4 != 0 {
        crate::log!("SWEET CP: no usable SQE firmware in this kernel ({} bytes)", SQE_FW.len());
        return false;
    }
    // adreno_fw_create_bo(): the first dword of the file is not loaded
    let mut head = [0u32; 4];
    for (i, w) in SQE_FW[4..20].chunks_exact(4).enumerate() {
        head[i] = u32::from_le_bytes([w[0], w[1], w[2], w[3]]);
    }
    if !sqe_ok(&head) {
        crate::log!("SWEET CP: SQE ucode too old ({:#x}, want >= 0x190)", head[0] & 0xfff);
        return false;
    }
    let Some(mem) = mem_setup() else {
        return false;
    };
    for (i, w) in SQE_FW[4..].chunks_exact(4).enumerate() {
        mem.w32(SQE_OFF + 4 * i, u32::from_le_bytes([w[0], w[1], w[2], w[3]]));
    }
    for i in 0..RING_BYTES / 4 {
        mem.w32(RING_OFF + 4 * i, 0);
    }
    mem.w32(SCRATCH_OFF, 0);
    mem.dma.clean();
    crate::log!("SWEET CP: sqe ucode {} bytes, version {:#x}", SQE_FW.len() - 4, head[0] & 0xfff);

    gpu.write(GBIF_HALT, 0);
    gpu.read(GBIF_HALT);
    gpu.write(RBBM_GBIF_HALT, 0);
    gpu.read(RBBM_GBIF_HALT);
    gpu.write(RBBM_SECVID_TSB_CNTL, 0);
    gpu.write64(RBBM_SECVID_TSB_TRUSTED_BASE, 0);
    gpu.write(RBBM_SECVID_TSB_TRUSTED_SIZE, 0);
    for r in [
        CP_ADDR_MODE_CNTL,
        VSC_ADDR_MODE_CNTL,
        GRAS_ADDR_MODE_CNTL,
        RB_ADDR_MODE_CNTL,
        PC_ADDR_MODE_CNTL,
        HLSQ_ADDR_MODE_CNTL,
        VFD_ADDR_MODE_CNTL,
        VPC_ADDR_MODE_CNTL,
        UCHE_ADDR_MODE_CNTL,
        SP_ADDR_MODE_CNTL,
        TPL1_ADDR_MODE_CNTL,
        RBBM_SECVID_TSB_ADDR_MODE_CNTL,
    ] {
        gpu.write(r, 1);
    }
    gpu.write(RBBM_VBIF_CLIENT_QOS_CNTL, 3);
    gpu.write64(UCHE_WRITE_RANGE_MAX, UCHE_TRAP + 0xfc0);
    gpu.write64(UCHE_TRAP_BASE, UCHE_TRAP);
    gpu.write64(UCHE_WRITE_THRU_BASE, UCHE_TRAP);
    gpu.write64(UCHE_GMEM_RANGE_MIN, GMEM_MIN);
    gpu.write64(UCHE_GMEM_RANGE_MAX, GMEM_MIN + GMEM_SIZE - 1);
    gpu.write(UCHE_FILTER_CNTL, 0x804);
    gpu.write(UCHE_CACHE_WAYS, 0x4);
    gpu.write(UCHE_CLIENT_PF, (1 << 7) | 1);
    gpu.write(CP_ROQ_THRESHOLDS_2, 0x0100_00c0);
    gpu.write(CP_ROQ_THRESHOLDS_1, 0x8040_362c);
    gpu.write(CP_MEM_POOL_SIZE, 128);
    gpu.write(CP_AHB_CNTL, 1);

    gpu.write64(CP_SQE_INSTR_BASE, mem.iova(SQE_OFF));
    gpu.write64(CP_RB_BASE, mem.iova(RING_OFF));
    gpu.write(CP_RB_CNTL, RB_CNTL);
    crate::log!(
        "SWEET CP: rb cntl {:#010x} rptr {} wptr {}, starting SQE",
        gpu.read(CP_RB_CNTL),
        gpu.read(CP_RB_RPTR),
        gpu.read(CP_RB_WPTR)
    );
    crate::log!("SWEET CP: rbbm secvid trust cntl {:#010x}", gpu.read(RBBM_SECVID_TRUST_CNTL));
    gpu.write(CP_SQE_CNTL, 1);

    // a6xx_cp_init()
    let mut ring = Ring { mem: &mem, wptr: 0 };
    ring.emit(pkt7(CP_ME_INIT, 8));
    for v in [0x2f, 0x3, 0x2000_0000, 0, 0, 0, 0, 0] {
        ring.emit(v);
    }
    if !ring.submit(&gpu, "CP_ME_INIT") {
        return false;
    }

    // a6xx_hw_init(): with the zap shader authenticated by TZ the CP may drop
    // the GPU out of secure mode; without it RB/CCU writes never land (0110c)
    // and a direct RBBM_SECVID_TRUST_CNTL write hangs (0110e)
    if crate::zap::load() {
        ring.emit(pkt7(CP_SET_SECURE_MODE, 1));
        ring.emit(0);
        if ring.submit(&gpu, "CP_SET_SECURE_MODE(0)") {
            crate::log!("SWEET CP: rbbm secvid trust cntl now {:#010x}", gpu.read(RBBM_SECVID_TRUST_CNTL));
        }
    }

    // first command: one register write and one memory write by the CP
    const REG_MAGIC: u32 = 0x5ca1_ab1e;
    const MEM_MAGIC: u32 = 0xc0ff_ee01;
    ring.emit(pkt4(CP_SCRATCH0 as u32, 1));
    ring.emit(REG_MAGIC);
    let a = mem.iova(SCRATCH_OFF);
    ring.emit(pkt7(CP_MEM_WRITE, 3));
    ring.emit(a as u32);
    ring.emit((a >> 32) as u32);
    ring.emit(MEM_MAGIC);
    ring.emit(pkt7(CP_NOP, 0));
    let ok = ring.submit(&gpu, "first command");
    mem.dma.flush(TABLES + SCRATCH_OFF, 64);
    let (reg, word) = (gpu.read(CP_SCRATCH0), mem.r32(SCRATCH_OFF));
    crate::log!(
        "SWEET CP: scratch reg {reg:#010x} (want {REG_MAGIC:#x}), memory {word:#010x} (want {MEM_MAGIC:#x}){}",
        if ok && reg == REG_MAGIC && word == MEM_MAGIC { ": FIRST GPU COMMAND EXECUTED" } else { "" }
    );
    if word != MEM_MAGIC {
        smmu_fault();
        return false;
    }

    // fence: CACHE_FLUSH_TS memory write of a sequence number
    ring.fence(1);
    let ok = ring.submit(&gpu, "fence 1");
    let f = fence_read(&mem);
    crate::log!("SWEET CP: fence {f} (want 1){}", if ok && f == 1 { ": FENCE SIGNALLED" } else { "" });
    if f != 1 {
        smmu_fault();
        return false;
    }
    if !solid_fill(&gpu, &mem, &mut ring) {
        return false;
    }
    // a hung 3D test leaves the CP stuck: no present engine then
    if cp3d::triangle(&gpu, &mem, &mut ring) == cp3d::Outcome::Hung
        || cp3d::texquad(&gpu, &mem, &mut ring) == cp3d::Outcome::Hung
    {
        return false;
    }
    let wptr = ring.wptr;
    engine_init(gpu, mem, wptr)
}

// the fence write is the last packet of a present, so seeing it also means
// the CP has consumed the ring up to `wptr`
fn wait_fence(gpu: &Gpu, mem: &GpuMem, wptr: u32, seq: u32) -> bool {
    for _ in 0..500_000 {
        if gpu.read(CP_RB_RPTR) == wptr && fence_read(mem) == seq {
            return true;
        }
        delay_us(2);
    }
    crate::log!(
        "SWEET CP: present TIMEOUT rptr {} wptr {} fence {} want {seq} status {:#010x} int0 {:#010x}",
        gpu.read(CP_RB_RPTR),
        wptr,
        fence_read(mem),
        gpu.read(RBBM_STATUS),
        gpu.read(RBBM_INT_0_STATUS)
    );
    // where the CP stopped inside the indirect buffers (a6xx.xml CP_IB1/IB2)
    crate::log!(
        "SWEET CP: ib1 {:#x} rem {} ib2 {:#x} rem {}",
        (gpu.read(0x929) as u64) << 32 | gpu.read(0x928) as u64,
        gpu.read(0x92a),
        (gpu.read(0x92c) as u64) << 32 | gpu.read(0x92b) as u64,
        gpu.read(0x92d)
    );
    smmu_fault();
    false
}

fn fence_read(mem: &GpuMem) -> u32 {
    mem.dma.flush(TABLES + SCRATCH_OFF, PAGE);
    mem.r32(FENCE_OFF)
}

// 2D engine solid colour fill, the sequence of freedreno fd6_blitter.c
// (emit_setup + clear blit) / turnip r2d_*: CCU to bypass mode, BLT_CNTL with
// SOLID_COLOR, per-channel UNORM8 clear values, linear RGBA8 destination,
// CP_BLIT(BLIT_OP_SCALE), then flush CCU colour and the caches by fence.
fn solid_fill(gpu: &Gpu, mem: &GpuMem, ring: &mut Ring) -> bool {
    const POISON: u32 = 0xdead_beef;
    // R, G, B, A; WZYX swap puts R in the low byte
    const RGBA: [u32; 4] = [0x20, 0x80, 0xe0, 0xff];
    const WANT: u32 = 0xffe0_8020;
    const X0: u32 = 8;
    const Y0: u32 = 8;
    const X1: u32 = 56; // exclusive
    const Y1: u32 = 40;
    for i in 0..DST_BYTES / 4 {
        mem.w32(DST_OFF + 4 * i, POISON);
    }
    mem.dma.clean();
    let ts = mem.iova(CCU_TS_OFF);

    for (r, v) in A618_MAGIC.iter().chain(STATIC_CTX.iter()) {
        ring.reg(*r, &[*v]);
    }

    ring.event(PC_CCU_FLUSH_COLOR_TS, Some((ts, 1)));
    ring.event(PC_CCU_FLUSH_DEPTH_TS, Some((ts, 2)));
    ring.event(PC_CCU_INVALIDATE_COLOR, None);
    ring.event(PC_CCU_INVALIDATE_DEPTH, None);
    ring.wfi();
    ring.reg(RB_CCU_CNTL, &[CCU_CNTL_BYPASS]);

    let fmt = FMT6_8_8_8_8_UNORM;
    // ROTATE 0, SOLID_COLOR, COLOR_FORMAT, MASK 0xf, IFMT R2D_UNORM8
    let blt = (1 << 7) | (fmt << 8) | (0xf << 20);
    ring.reg(RB_A2D_BLT_CNTL, &[blt]);
    ring.reg(GRAS_A2D_BLT_CNTL, &[blt]);
    ring.reg(SP_A2D_OUTPUT_INFO, &[(fmt << 3) | (0xf << 12)]);
    ring.reg(RB_A2D_PIXEL_CNTL, &[0]);
    let dst = mem.iova(DST_OFF);
    // DEST_BUFFER_INFO (linear, WZYX), BASE lo/hi, PITCH >> 6
    ring.reg(RB_A2D_DEST_BUFFER_INFO, &[fmt, dst as u32, (dst >> 32) as u32, DST_PITCH >> 6]);
    ring.reg(RB_A2D_DEST_FLAG_BUFFER_BASE, &[0, 0, 0]);
    ring.reg(RB_A2D_CLEAR_COLOR_DW0, &RGBA);
    ring.reg(GRAS_A2D_DEST_TL, &[X0 | (Y0 << 16), (X1 - 1) | ((Y1 - 1) << 16)]);

    // emit_blit_fini(): LABEL event, then (a618 blit_wfi_quirk, turnip) WFI
    ring.event(FD_LABEL, None);
    ring.wfi();
    ring.emit(pkt7(CP_BLIT, 1));
    ring.emit(BLIT_OP_SCALE);

    ring.event(PC_CCU_FLUSH_COLOR_TS, Some((ts, 3)));
    ring.wfi();
    let fseq = fence_read(mem) + 1;
    ring.fence(fseq);
    let ok = ring.submit(gpu, "solid fill");
    let f = fence_read(mem);
    mem.dma.flush(TABLES + DST_OFF, DST_BYTES);
    let (mut inside, mut outside, mut poison_in) = (0u32, 0u32, 0u32);
    let mut first_bad = None;
    for y in 0..DST_H {
        for x in 0..DST_W {
            let v = mem.r32(DST_OFF + (y * DST_PITCH + 4 * x) as usize);
            let inr = (X0..X1).contains(&x) && (Y0..Y1).contains(&y);
            if inr && v == WANT {
                inside += 1;
            } else if !inr && v == POISON {
                outside += 1;
            } else {
                if inr && v == POISON {
                    poison_in += 1;
                }
                if first_bad.is_none() {
                    first_bad = Some((x, y, v));
                }
            }
        }
    }
    let want_in = (X1 - X0) * (Y1 - Y0);
    let want_out = DST_W * DST_H - want_in;
    crate::log!(
        "SWEET CP: fill fence {f} (want {fseq}) ccu ts {:#x}; rect pixels ok {inside}/{want_in} (untouched {poison_in}), border intact {outside}/{want_out}, px(8,8) {:#010x} px(0,0) {:#010x}",
        mem.r32(CCU_TS_OFF),
        mem.r32(DST_OFF + (Y0 * DST_PITCH + 4 * X0) as usize),
        mem.r32(DST_OFF)
    );
    crate::log!(
        "SWEET CP: after fill: rb blt {:#010x} gras blt {:#010x} dst info {:#x} base {:#x} br {:#010x} color0 {:#x} ccu {:#010x} eco {:#010x} int0 {:#010x} hw_fault {:#x} status {:#010x}",
        gpu.read(RB_A2D_BLT_CNTL as usize),
        gpu.read(GRAS_A2D_BLT_CNTL as usize),
        gpu.read(RB_A2D_DEST_BUFFER_INFO as usize),
        (gpu.read(RB_A2D_DEST_BUFFER_INFO as usize + 2) as u64) << 32 | gpu.read(RB_A2D_DEST_BUFFER_INFO as usize + 1) as u64,
        gpu.read(GRAS_A2D_DEST_TL as usize + 1),
        gpu.read(RB_A2D_CLEAR_COLOR_DW0 as usize),
        gpu.read(RB_CCU_CNTL as usize),
        gpu.read(RB_DBG_ECO_CNTL as usize),
        gpu.read(RBBM_INT_0_STATUS),
        gpu.read(CP_HW_FAULT),
        gpu.read(RBBM_STATUS)
    );
    if let Some((x, y, v)) = first_bad {
        crate::log!("SWEET CP: first mismatch at ({x},{y}) = {v:#010x}");
    }
    if ok && f == fseq && inside == want_in && outside == want_out {
        crate::log!("SWEET CP: 2D SOLID FILL OK ({}x{} rgba8 {WANT:#010x})", X1 - X0, Y1 - Y0);
        true
    } else {
        smmu_fault();
        false
    }
}

// ---------------------------------------------------------------------------
// Present engine: after the self tests pass the GPU stays up (gpu.rs keeps the
// OOB gpu_set vote) and AppleCLCD::present() asks it to copy the swapped
// IOSurface layer into panel memory instead of a CPU memcpy.

const TPL1_A2D_SRC_TEXTURE_INFO: u32 = 0xb4c0;
const TPL1_A2D_SRC_TEXTURE_FLAG_BASE: u32 = 0xb4ca;
const GRAS_A2D_SRC_XMIN: u32 = 0x8401;
const CACHE_INVALIDATE: u32 = 0x31;

// L1 slots of the 4 GiB GPU VA space (cb0, 4 KiB granule, T0SZ 32)
const PANEL_IOVA: u64 = 0x4000_0000;
const SURF_IOVA: u64 = 0x8000_0000;
const SURF_L3: usize = 8; // 8 L3 tables = 16 MiB of surface
const SURF_MAX: usize = SURF_L3 * 512 * PAGE;
const BLOCK_2M: u64 = 0x20_0000;

struct Engine {
    gpu: Gpu,
    mem: GpuMem,
    smmu: Mmio,
    // page 0 panel L2, page 1 surface L2, pages 2.. surface L3
    pt: Dma,
    wptr: u32,
    panel: Option<(u64, usize)>,
    fseq: u32,
    // a present was published and its fence not seen yet
    pending: bool,
    dead: bool,
    fault_count: u32,
    user: Option<cpuser::User>,
}

static mut ENGINE: Option<Engine> = None;
pub(crate) static ENGINE_READY: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
static BUSY: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

fn tlb_flush(smmu: &Mmio) {
    // TLBIALLNSNH + TLBGSYNC, as in mem_setup()
    smmu.write(0x68, 0);
    smmu.write(0x70, 0);
    for _ in 0..1000 {
        if smmu.read(0x74) & 1 == 0 {
            return;
        }
        delay_us(1);
    }
}

fn engine_init(gpu: Gpu, mem: GpuMem, wptr: u32) -> bool {
    let (Some(smmu), Some(pt)) = (Mmio::map(SMMU, SMMU_SIZE), Dma::alloc((2 + SURF_L3) * PAGE)) else {
        crate::log!("SWEET CP: present engine: no memory");
        return false;
    };
    for i in 0..pt.size / 8 {
        pt.w64(8 * i, 0);
    }
    for i in 0..SURF_L3 {
        pt.w64(PAGE + 8 * i, pte_table(pt.pa + ((2 + i) * PAGE) as u64));
    }
    pt.clean();
    mem.dma.w64(((PANEL_IOVA >> 30) as usize) * 8, pte_table(pt.pa));
    mem.dma.w64(((SURF_IOVA >> 30) as usize) * 8, pte_table(pt.pa + PAGE as u64));
    mem.dma.clean();
    tlb_flush(&smmu);
    let fseq = fence_read(&mem);
    crate::log!("SWEET CP: present engine ready (panel iova {PANEL_IOVA:#x}, surface iova {SURF_IOVA:#x} up to {SURF_MAX:#x} bytes)");
    unsafe { ENGINE = Some(Engine { gpu, mem, smmu, pt, wptr, panel: None, fseq, pending: false, dead: false, fault_count: 0, user: None }) };
    ENGINE_READY.store(true, core::sync::atomic::Ordering::Release);
    true
}

impl Engine {
    fn map_panel(&mut self, pa: u64, size: usize) -> bool {
        if self.panel == Some((pa, size)) {
            return true;
        }
        let base = pa & !(BLOCK_2M - 1);
        let n = (pa - base + size as u64).div_ceil(BLOCK_2M) as usize;
        if n > 512 {
            return false;
        }
        for i in 0..n {
            // L2 block: Normal NC (MAIR attr 0), AF, same AP bits as pte_page
            self.pt.w64(8 * i, (base + i as u64 * BLOCK_2M) | (1 << 10) | (1 << 6) | 0b01);
        }
        self.pt.clean();
        tlb_flush(&self.smmu);
        self.panel = Some((pa, size));
        crate::log!("SWEET CP: panel {pa:#x}+{size:#x} mapped at iova {:#x}", PANEL_IOVA + (pa - base));
        true
    }

    // map [0, bytes) of a surface given as physical runs
    fn map_surface(&mut self, segs: &[[u64; 2]], bytes: usize) -> bool {
        if bytes > SURF_MAX {
            return false;
        }
        let mut page = 0usize;
        let want = bytes.div_ceil(PAGE);
        for &[pa, len] in segs {
            if pa & (PAGE as u64 - 1) != 0 || len & (PAGE as u64 - 1) != 0 {
                return false;
            }
            for k in 0..(len as usize / PAGE) {
                if page == want {
                    break;
                }
                self.pt.w64(2 * PAGE + 8 * page, pte_page(pa + (k * PAGE) as u64));
                page += 1;
            }
        }
        if page < want {
            return false;
        }
        self.pt.clean();
        tlb_flush(&self.smmu);
        true
    }
}

// One copy pass: `rows` rows starting at src/dst row bases, both pitches as
// programmed (twice the real pitch for the interleaved passes).
#[allow(clippy::too_many_arguments)]
fn blit_pass(ring: &mut Ring, src: u64, spitch: u32, sx: u32, dst: u64, dpitch: u32, dx: u32, w: u32, rows: u32) {
    let fmt = FMT6_8_8_8_8_UNORM;
    // base registers want 64-byte alignment: align down, shift x instead
    let sshift = ((src & 63) / 4) as u32;
    let dshift = ((dst & 63) / 4) as u32;
    let (src, dst) = (src & !63, dst & !63);
    let (sx, dx) = (sx + sshift, dx + dshift);
    // COLOR_FORMAT, linear, WZYX, unk20 | unk22 as freedreno emit_blit_src()
    ring.reg(
        TPL1_A2D_SRC_TEXTURE_INFO,
        &[fmt | (1 << 20) | (1 << 22), (sx + w) | (rows << 15), src as u32, (src >> 32) as u32, (spitch >> 6) << 9],
    );
    ring.reg(TPL1_A2D_SRC_TEXTURE_FLAG_BASE, &[0, 0, 0]);
    ring.reg(GRAS_A2D_SRC_XMIN, &[sx << 8, (sx + w - 1) << 8, 0, (rows - 1) << 8]);
    ring.reg(RB_A2D_DEST_BUFFER_INFO, &[fmt, dst as u32, (dst >> 32) as u32, dpitch >> 6]);
    ring.reg(GRAS_A2D_DEST_TL, &[dx, (dx + w - 1) | ((rows - 1) << 16)]);
    ring.event(FD_LABEL, None);
    ring.wfi();
    ring.emit(pkt7(CP_BLIT, 1));
    ring.emit(BLIT_OP_SCALE);
}

/// Copy a w x h rectangle of a linear 32 bpp surface (physical runs `segs`,
/// covering at least `off + (sy + h) * spitch` bytes) to panel memory at
/// `panel_pa` (pitch `dpitch`, `panel_size` bytes). Returns false when the
/// GPU path is unavailable; the caller then copies with the CPU. With
/// `sync` false it returns once the copy is queued (`sweet_rs_gpu_wait`, or
/// the next call, waits for it).
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub extern "C" fn sweet_rs_gpu_blit(
    segs: *const [u64; 2],
    nseg: u32,
    off: u32,
    spitch: u32,
    sx: u32,
    sy: u32,
    w: u32,
    h: u32,
    panel_pa: u64,
    panel_size: u64,
    dpitch: u32,
    dx: u32,
    dy: u32,
    sync: bool,
) -> bool {
    use core::sync::atomic::Ordering;
    if segs.is_null() || nseg == 0 || w == 0 || h == 0 || w > 0x3000 || h > 0x3000 {
        return false;
    }
    // wait for a user submission holding the engine (as sweet_rs_gpu_wait):
    // failing here turned the GPU present off for the boot (probe123)
    while BUSY.swap(true, Ordering::Acquire) {
        core::hint::spin_loop();
    }
    let ok = blit_locked(unsafe { core::slice::from_raw_parts(segs, nseg as usize) }, off, spitch, sx, sy, w, h, panel_pa, panel_size, dpitch, dx, dy, sync);
    BUSY.store(false, Ordering::Release);
    ok
}

#[allow(clippy::too_many_arguments)]
fn blit_locked(
    segs: &[[u64; 2]],
    off: u32,
    spitch: u32,
    sx: u32,
    sy: u32,
    w: u32,
    h: u32,
    panel_pa: u64,
    panel_size: u64,
    dpitch: u32,
    dx: u32,
    dy: u32,
    sync: bool,
) -> bool {
    let Some(e) = (unsafe { (*core::ptr::addr_of_mut!(ENGINE)).as_mut() }) else {
        return false;
    };
    if e.dead || spitch % 4 != 0 || dpitch % 4 != 0 {
        return false;
    }
    // the previous present may still be reading through the surface tables
    if !e.idle() {
        return false;
    }
    let bytes = off as usize + (sy + h) as usize * spitch as usize;
    if (dy + h) as u64 * dpitch as u64 > panel_size || (dx + w) * 4 > dpitch || (sx + w) * 4 > spitch {
        return false;
    }
    if !e.map_panel(panel_pa, panel_size as usize) || !e.map_surface(segs, bytes) {
        return false;
    }
    let panel_iova = PANEL_IOVA + (panel_pa & (BLOCK_2M - 1));
    let src0 = SURF_IOVA + off as u64 + sy as u64 * spitch as u64;
    let dst0 = panel_iova + dy as u64 * dpitch as u64;
    // one pass when both pitches are 64-byte multiples, else even/odd rows
    // with doubled pitches (panel stride 4320 = 67.5 * 64)
    let step = if spitch % 64 == 0 && dpitch % 64 == 0 { 1 } else { 2 };
    if (step * spitch) % 64 != 0 || (step * dpitch) % 64 != 0 {
        return false;
    }

    let seq = e.fseq.wrapping_add(1);
    let mut ring = Ring { mem: &e.mem, wptr: e.wptr };
    let ts = e.mem.iova(CCU_TS_OFF);
    ring.event(PC_CCU_FLUSH_COLOR_TS, Some((ts, 1)));
    ring.event(PC_CCU_INVALIDATE_COLOR, None);
    ring.event(CACHE_INVALIDATE, None);
    ring.wfi();
    ring.reg(RB_CCU_CNTL, &[CCU_CNTL_BYPASS]);
    let fmt = FMT6_8_8_8_8_UNORM;
    let blt = (fmt << 8) | (0xf << 20);
    ring.reg(RB_A2D_BLT_CNTL, &[blt]);
    ring.reg(GRAS_A2D_BLT_CNTL, &[blt]);
    ring.reg(SP_A2D_OUTPUT_INFO, &[(fmt << 3) | (0xf << 12)]);
    ring.reg(RB_A2D_PIXEL_CNTL, &[0]);
    ring.reg(RB_A2D_DEST_FLAG_BUFFER_BASE, &[0, 0, 0]);
    for k in 0..step {
        let rows = (h - k).div_ceil(step);
        if rows == 0 {
            continue;
        }
        blit_pass(
            &mut ring,
            src0 + k as u64 * spitch as u64,
            step * spitch,
            sx,
            dst0 + k as u64 * dpitch as u64,
            step * dpitch,
            dx,
            w,
            rows,
        );
    }
    ring.event(PC_CCU_FLUSH_COLOR_TS, Some((ts, 2)));
    ring.wfi();
    ring.fence(seq);
    ring.publish(&e.gpu);
    e.wptr = ring.wptr;
    e.fseq = seq;
    e.pending = true;
    !sync || e.idle()
}

/// Scale the whole `sw` x `sh` BGRA surface (physical runs `segs`, first
/// pixel at byte `off`, pitch `spitch`) onto the whole `dw` x `dh` panel with
/// the 3D engine (bilinear textured quad, cp3d::compose). Same contract as
/// sweet_rs_gpu_blit: false means the caller presents with the CPU.
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub extern "C" fn sweet_rs_gpu_compose(
    segs: *const [u64; 2],
    nseg: u32,
    off: u32,
    spitch: u32,
    sw: u32,
    sh: u32,
    panel_pa: u64,
    panel_size: u64,
    dpitch: u32,
    dw: u32,
    dh: u32,
    sync: bool,
) -> bool {
    use core::sync::atomic::Ordering;
    if segs.is_null() || nseg == 0 || sw == 0 || sh == 0 || dw == 0 || dh == 0 {
        return false;
    }
    // wait for a user submission holding the engine (as sweet_rs_gpu_wait):
    // failing here turned the GPU present off for the boot (probe123)
    while BUSY.swap(true, Ordering::Acquire) {
        core::hint::spin_loop();
    }
    let ok = compose_locked(unsafe { core::slice::from_raw_parts(segs, nseg as usize) }, off, spitch, sw, sh, panel_pa, panel_size, dpitch, dw, dh, sync);
    BUSY.store(false, Ordering::Release);
    ok
}

#[allow(clippy::too_many_arguments)]
fn compose_locked(
    segs: &[[u64; 2]],
    off: u32,
    spitch: u32,
    sw: u32,
    sh: u32,
    panel_pa: u64,
    panel_size: u64,
    dpitch: u32,
    dw: u32,
    dh: u32,
    sync: bool,
) -> bool {
    let Some(e) = (unsafe { (*core::ptr::addr_of_mut!(ENGINE)).as_mut() }) else {
        return false;
    };
    if e.dead || dh as u64 * dpitch as u64 > panel_size || dw * 4 > dpitch || sw * 4 > spitch || dh % 2 != 0 {
        return false;
    }
    let c = cp3d::Compose {
        panel: PANEL_IOVA + (panel_pa & (BLOCK_2M - 1)),
        dpitch,
        dw,
        dh,
        tex: SURF_IOVA + off as u64,
        sw,
        sh,
        spitch,
    };
    if !cp3d::compose_check(&c) || !e.idle() {
        return false;
    }
    let bytes = off as usize + sh as usize * spitch as usize;
    if !e.map_panel(panel_pa, panel_size as usize) || !e.map_surface(segs, bytes) {
        return false;
    }
    let seq = e.fseq.wrapping_add(1);
    let mut ring = Ring { mem: &e.mem, wptr: e.wptr };
    cp3d::compose(&mut ring, &e.mem, &c);
    ring.fence(seq);
    ring.publish(&e.gpu);
    e.wptr = ring.wptr;
    e.fseq = seq;
    e.pending = true;
    !sync || e.idle()
}

impl Engine {
    fn check_clear_smmu_fault(&mut self) -> u32 {
        let cb = CB_BASE + CB * PAGE;
        let fsr = self.smmu.read(cb + 0x58);
        if fsr & 0x7f != 0 {
            let far = (self.smmu.read(cb + 0x64) as u64) << 32 | self.smmu.read(cb + 0x60) as u64;
            let fsynr = self.smmu.read(cb + 0x68);
            self.fault_count += 1;
            crate::log!(
                "SWEET CP: SMMU fault #{}: fsr {fsr:#x} far {far:#x} fsynr {fsynr:#x} (fence {} want {})",
                self.fault_count, fence_read(&self.mem), self.fseq
            );
            self.smmu.write(cb + 0x58, fsr);
        }
        fsr
    }

    fn idle(&mut self) -> bool {
        if !self.pending {
            return true;
        }
        if wait_fence(&self.gpu, &self.mem, self.wptr, self.fseq) {
            self.check_clear_smmu_fault();
            self.pending = false;
            return true;
        }
        // timeout: check for SMMU fault, clear it, retry once
        let fsr = self.check_clear_smmu_fault();
        if fsr != 0 {
            crate::log!("SWEET CP: cleared SMMU fault, retrying fence wait");
            if wait_fence(&self.gpu, &self.mem, self.wptr, self.fseq) {
                self.pending = false;
                return true;
            }
        }
        crate::log!(
            "SWEET CP: idle failed (fence {} want {}, faults {}), GPU present disabled",
            fence_read(&self.mem), self.fseq, self.fault_count
        );
        self.dead = true;
        false
    }
}

/// Wait until the last present submitted with `sync = false` has landed in
/// panel memory. False when it did not (the engine is then disabled).
#[unsafe(no_mangle)]
pub extern "C" fn sweet_rs_gpu_wait() -> bool {
    use core::sync::atomic::Ordering;
    while BUSY.swap(true, Ordering::Acquire) {
        core::hint::spin_loop();
    }
    let ok = match unsafe { (*core::ptr::addr_of_mut!(ENGINE)).as_mut() } {
        Some(e) => !e.dead && e.idle(),
        None => false,
    };
    BUSY.store(false, Ordering::Release);
    ok
}

/// 0 = present engine not up (yet), 1 = ready, 2 = disabled after a failure
#[unsafe(no_mangle)]
pub extern "C" fn sweet_rs_gpu_state() -> u32 {
    match unsafe { (*core::ptr::addr_of!(ENGINE)).as_ref() } {
        None => 0,
        Some(e) if e.dead => 2,
        Some(_) => 1,
    }
}
