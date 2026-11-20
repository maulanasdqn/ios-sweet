// Command submission for user clients (SkyAdrenoAccel, xnu patch 0109): GPU
// buffers mapped into the last 1 GiB L1 slot of the GPU VA space, user
// command streams run as CP_INDIRECT_BUFFER from the kernel ring with CP
// register protection on (a630 list, as Linux a6xx_set_cp_protect), fenced
// with the same CACHE_FLUSH_TS sequence as the presents.

use super::*;

const CP_PROTECT_CNTL: usize = 0x084f;
const CP_PROTECT_0: usize = 0x0850;
const CP_PROTECT_COUNT: usize = 32;
const CP_INDIRECT_BUFFER: u32 = 0x3f;
const CP_SET_PROTECTED_MODE: u32 = 0x5f;

const USER_IOVA: u64 = 0xc000_0000;
// 256 L3 tables = 512 MiB of user buffers (128 MiB ran out in Settings,
// probe116: 105 MiB live, map -> 0, SMMU fault at far 0)
const USER_L3: usize = 256;
const USER_PAGES: usize = USER_L3 * 512;
const IB_MAX_DWORDS: u32 = 0x000f_ffff;

const fn nordwr(reg: u32, len: u32) -> u32 {
    (1 << 31) | ((len & 0x3fff) << 18) | (reg & 0x3ffff)
}

const fn rdonly(reg: u32, len: u32) -> u32 {
    ((len & 0x3fff) << 18) | (reg & 0x3ffff)
}

// a630_protect_regs (a6xx_catalog.c), used for a615/a618 too
const PROTECT: [u32; 32] = [
    rdonly(0x00000, 0x04ff),
    rdonly(0x00501, 0x0005),
    rdonly(0x0050b, 0x02f4),
    nordwr(0x0050e, 0x0000),
    nordwr(0x00510, 0x0000),
    nordwr(0x00534, 0x0000),
    nordwr(0x00800, 0x0082),
    nordwr(0x008a0, 0x0008),
    nordwr(0x008ab, 0x0024),
    rdonly(0x008de, 0x00ae),
    nordwr(0x00900, 0x004d),
    nordwr(0x0098d, 0x0272),
    nordwr(0x00e00, 0x0001),
    nordwr(0x00e03, 0x000c),
    nordwr(0x03c00, 0x00c3),
    rdonly(0x03cc4, 0x1fff),
    nordwr(0x08630, 0x01cf),
    nordwr(0x08e00, 0x0000),
    nordwr(0x08e08, 0x0000),
    nordwr(0x08e50, 0x001f),
    nordwr(0x09624, 0x01db),
    nordwr(0x09e70, 0x0001),
    nordwr(0x09e78, 0x0187),
    nordwr(0x0a630, 0x01cf),
    nordwr(0x0ae02, 0x0000),
    nordwr(0x0ae50, 0x032f),
    nordwr(0x0b604, 0x0000),
    nordwr(0x0be02, 0x0001),
    nordwr(0x0be20, 0x17df),
    nordwr(0x0f000, 0x0bff),
    rdonly(0x0fc00, 0x1fff),
    nordwr(0x11c00, 0x0000),
];

// 16 KiB XNU pages, each one physically contiguous: 4 L3 tables per chunk
const CHUNK: usize = 0x4000;
const L3_CHUNKS: usize = USER_L3 * PAGE / CHUNK;

// Page tables for the user slot, allocated as single XNU pages: a 65-page
// contiguous allocation after boot (0111d/0111e) hung the whole phone,
// presumably in kmem_alloc_contig's scan of fragmented memory.
// The L3 chunks and the page bitmap live in statics: with 512 MiB they are
// too big for the 16 KiB kernel stack the open path runs on (probe118:
// "Invalid kernel stack pointer (probable overflow)" in backboardd).
static mut L3: [Option<Dma>; L3_CHUNKS] = [const { None }; L3_CHUNKS];
static mut USED: [u64; USER_PAGES / 64] = [0; USER_PAGES / 64];

pub struct Tables {
    l2: Dma,
    l3: &'static mut [Option<Dma>; L3_CHUNKS],
}

impl Tables {
    // called once (user open is guarded by OPEN)
    fn alloc() -> Option<Tables> {
        let t = Tables { l2: Dma::alloc(CHUNK)?, l3: unsafe { &mut *core::ptr::addr_of_mut!(L3) } };
        for c in t.l3.iter_mut() {
            if c.is_none() {
                *c = Some(Dma::alloc(CHUNK)?);
            }
        }
        Some(t)
    }

    fn chunk(&self, p: usize) -> &Dma {
        self.l3[p / 512 / (CHUNK / PAGE)].as_ref().unwrap()
    }

    fn set(&self, p: usize, pte: u64) {
        self.chunk(p).w64((p / 512 % (CHUNK / PAGE)) * PAGE + (p % 512) * 8, pte);
    }

    fn clean(&self, first: usize, n: usize) {
        let mut last = usize::MAX;
        for p in first..first + n {
            let c = p / 512 / (CHUNK / PAGE);
            if c != last {
                self.l3[c].as_ref().unwrap().clean();
                last = c;
            }
        }
    }
}

pub struct User {
    pt: Tables,
    used: &'static mut [u64; USER_PAGES / 64],
}

impl User {
    fn is_used(&self, p: usize) -> bool {
        self.used[p / 64] & (1 << (p % 64)) != 0
    }

    fn set_used(&mut self, p: usize, on: bool) {
        if on {
            self.used[p / 64] |= 1 << (p % 64);
        } else {
            self.used[p / 64] &= !(1 << (p % 64));
        }
    }

    // first fit, one guard page after every buffer
    fn find(&self, n: usize) -> Option<usize> {
        let mut run = 0;
        for p in 0..USER_PAGES {
            if self.is_used(p) {
                run = 0;
                continue;
            }
            run += 1;
            if run == n + 1 {
                return Some(p + 1 - run);
            }
        }
        None
    }

    fn range(iova: u64, bytes: u64) -> Option<(usize, usize)> {
        if iova < USER_IOVA || iova % PAGE as u64 != 0 || bytes == 0 {
            return None;
        }
        let first = ((iova - USER_IOVA) / PAGE as u64) as usize;
        let n = bytes.div_ceil(PAGE as u64) as usize;
        (first + n <= USER_PAGES).then_some((first, n))
    }

    fn mapped(&self, iova: u64, bytes: u64) -> bool {
        Self::range(iova & !(PAGE as u64 - 1), bytes + (iova & (PAGE as u64 - 1)))
            .is_some_and(|(first, n)| (first..first + n).all(|p| self.is_used(p)))
    }
}

impl Engine {
    fn user_open(&mut self, pt: Tables) -> bool {
        if self.user.is_some() {
            return true;
        }
        crate::log!("SWEET CP: user open: waiting for idle");
        if !self.idle() {
            return false;
        }
        for i in 0..CHUNK / 8 {
            pt.l2.w64(8 * i, 0);
        }
        for c in pt.l3.iter().flatten() {
            for i in 0..CHUNK / 8 {
                c.w64(8 * i, 0);
            }
            c.clean();
        }
        for i in 0..USER_L3 {
            let c = pt.l3[i / (CHUNK / PAGE)].as_ref().unwrap();
            pt.l2.w64(8 * i, pte_table(c.pa + ((i % (CHUNK / PAGE)) * PAGE) as u64));
        }
        pt.l2.clean();
        self.mem.dma.w64(((USER_IOVA >> 30) as usize) * 8, pte_table(pt.l2.pa));
        self.mem.dma.clean();
        tlb_flush(&self.smmu);
        crate::log!("SWEET CP: user open: L2 at {:#x}, cp protect next", pt.l2.pa);
        let g = &self.gpu;
        g.write(CP_PROTECT_CNTL, (1 << 3) | (1 << 1) | 1);
        for (i, &v) in PROTECT[..CP_PROTECT_COUNT - 1].iter().enumerate() {
            g.write(CP_PROTECT_0 + i, v);
        }
        g.write(CP_PROTECT_0 + CP_PROTECT_COUNT - 1, PROTECT[CP_PROTECT_COUNT - 1]);
        crate::log!(
            "SWEET CP: user submissions on: buffers at iova {USER_IOVA:#x} (up to {} MiB), cp protect cntl {:#x}",
            USER_PAGES * PAGE >> 20,
            g.read(CP_PROTECT_CNTL)
        );
        let used = unsafe { &mut *core::ptr::addr_of_mut!(USED) };
        used.fill(0);
        self.user = Some(User { pt, used });
        true
    }

    fn user_map(&mut self, segs: &[[u64; 2]], bytes: u64) -> u64 {
        let Some(u) = self.user.as_mut() else { return 0 };
        let n = bytes.div_ceil(PAGE as u64) as usize;
        let Some(first) = u.find(n) else { return 0 };
        let mut p = first;
        for &[pa, len] in segs {
            if pa % PAGE as u64 != 0 || len % PAGE as u64 != 0 {
                return 0;
            }
            for k in 0..len as usize / PAGE {
                if p == first + n {
                    break;
                }
                u.pt.set(p, pte_page(pa + (k * PAGE) as u64));
                p += 1;
            }
        }
        if p < first + n {
            for q in first..p {
                u.pt.set(q, 0);
            }
            return 0;
        }
        for q in first..p {
            u.set_used(q, true);
        }
        u.pt.clean(first, n);
        tlb_flush(&self.smmu);
        crate::log!("SWEET CP: user map {:#x} bytes at iova {:#x}", bytes, USER_IOVA + (first * PAGE) as u64);
        USER_IOVA + (first * PAGE) as u64
    }

    fn user_unmap(&mut self, iova: u64, bytes: u64) -> bool {
        // the GPU may still use the buffer: everything submitted must finish
        if !self.idle() {
            return false;
        }
        let Some(u) = self.user.as_mut() else { return false };
        let Some((first, n)) = User::range(iova, bytes) else { return false };
        for p in first..first + n {
            u.pt.set(p, 0);
            u.set_used(p, false);
        }
        u.pt.clean(first, n);
        tlb_flush(&self.smmu);
        crate::log!("SWEET CP: user unmap {:#x} bytes at iova {:#x} (after fence {})", bytes, iova, self.fseq);
        true
    }

    fn ring_space(&self, need: u32) -> bool {
        let size = (RING_BYTES / 4) as u32;
        for _ in 0..50_000 {
            let rptr = self.gpu.read(CP_RB_RPTR);
            if (rptr + size - self.wptr - 1) % size >= need {
                return true;
            }
            delay_us(2);
        }
        false
    }

    fn user_submit(&mut self, iova: u64, dwords: u32) -> u32 {
        if self.dead || dwords == 0 || dwords > IB_MAX_DWORDS || iova % 4 != 0 {
            return 0;
        }
        if !self.user.as_ref().is_some_and(|u| u.mapped(iova, 4 * dwords as u64)) {
            return 0;
        }
        if !self.ring_space(64) {
            crate::log!("SWEET CP: user submit: ring full, GPU stuck");
            self.dead = true;
            return 0;
        }
        let seq = self.fseq.wrapping_add(1);
        static LOGGED: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
        if LOGGED.fetch_add(1, core::sync::atomic::Ordering::Relaxed) < 20 {
            crate::log!("SWEET CP: user submit seq {seq}: ib {iova:#x} {dwords} dwords, wptr {}", self.wptr);
        }
        let mut ring = Ring { mem: &self.mem, wptr: self.wptr };
        // CPU writes went to memory (uncached user mappings): drop GPU cache lines
        ring.event(CACHE_INVALIDATE, None);
        ring.wfi();
        ring.emit(pkt7(CP_SET_PROTECTED_MODE, 1));
        ring.emit(1);
        ring.emit(pkt7(CP_INDIRECT_BUFFER, 3));
        ring.emit(iova as u32);
        ring.emit((iova >> 32) as u32);
        ring.emit(dwords);
        ring.emit(pkt7(CP_SET_PROTECTED_MODE, 1));
        ring.emit(0);
        let ts = self.mem.iova(CCU_TS_OFF);
        ring.event(PC_CCU_FLUSH_COLOR_TS, Some((ts, 3)));
        ring.event(PC_CCU_FLUSH_DEPTH_TS, Some((ts, 4)));
        ring.wfi();
        ring.fence(seq);
        ring.publish(&self.gpu);
        self.wptr = ring.wptr;
        self.fseq = seq;
        self.pending = true;
        seq
    }
}

/// Busy-wait `us` microseconds on the ARM generic timer (no scheduler, no
/// timer interrupt involved).
fn spin_us(us: u64) {
    let (freq, start): (u64, u64);
    unsafe {
        core::arch::asm!("mrs {}, cntfrq_el0", out(reg) freq);
        core::arch::asm!("isb", "mrs {}, cntvct_el0", out(reg) start);
    }
    let ticks = freq / 1_000_000 * us;
    loop {
        let now: u64;
        unsafe { core::arch::asm!("isb", "mrs {}, cntvct_el0", out(reg) now) };
        if now.wrapping_sub(start) >= ticks {
            return;
        }
        core::hint::spin_loop();
    }
}

fn locked<T>(f: impl FnOnce(&mut Engine) -> T, fail: T) -> T {
    use core::sync::atomic::Ordering;
    while BUSY.swap(true, Ordering::Acquire) {
        core::hint::spin_loop();
    }
    let r = match unsafe { (*core::ptr::addr_of_mut!(ENGINE)).as_mut() } {
        Some(e) if !e.dead => f(e),
        _ => fail,
    };
    BUSY.store(false, Ordering::Release);
    r
}

/// Turn on user submissions (page tables for user buffers, CP protection).
#[unsafe(no_mangle)]
pub extern "C" fn sweet_rs_gpu_user_open() -> bool {
    static OPEN: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
    use core::sync::atomic::Ordering;
    if OPEN.load(Ordering::Acquire) {
        return true;
    }
    // wait for the present engine to finish init (solid fill + 3D tests)
    if !super::ENGINE_READY.load(Ordering::Acquire) {
        crate::log!("SWEET CP: user open: waiting for engine init");
        for _ in 0..300 {
            // spin on the generic timer: delay() of 100 ms never returned here
            // (probe75, backboardd blocked for good)
            spin_us(100_000);
            if super::ENGINE_READY.load(Ordering::Acquire) {
                break;
            }
        }
        if !super::ENGINE_READY.load(Ordering::Acquire) {
            crate::log!("SWEET CP: user open: engine not ready after 30 s");
            return false;
        }
        crate::log!("SWEET CP: user open: engine ready, proceeding");
    }
    // allocate outside the engine lock: kmem_alloc_contig may take long
    crate::log!("SWEET CP: user open: allocating page tables");
    let Some(pt) = Tables::alloc() else {
        crate::log!("SWEET CP: user: no memory for page tables");
        return false;
    };
    crate::log!("SWEET CP: user open: tables allocated, l2 {:#x}", pt.l2.pa);
    let ok = locked(|e| e.user_open(pt), false);
    OPEN.store(ok, Ordering::Release);
    ok
}

/// Map `bytes` of a buffer given as page-aligned physical runs; returns the
/// GPU address or 0.
#[unsafe(no_mangle)]
pub extern "C" fn sweet_rs_gpu_user_map(segs: *const [u64; 2], nseg: u32, bytes: u64) -> u64 {
    if segs.is_null() || nseg == 0 || bytes == 0 || bytes > (USER_PAGES * PAGE) as u64 {
        return 0;
    }
    let segs = unsafe { core::slice::from_raw_parts(segs, nseg as usize) };
    locked(|e| e.user_map(segs, bytes), 0)
}

/// Unmap after all submitted work finished (waits for it).
#[unsafe(no_mangle)]
pub extern "C" fn sweet_rs_gpu_user_unmap(iova: u64, bytes: u64) -> bool {
    locked(|e| e.user_unmap(iova, bytes), false)
}

/// Run `dwords` of commands at GPU address `iova` (inside one mapped user
/// buffer range); returns the fence sequence number, 0 on failure.
#[unsafe(no_mangle)]
pub extern "C" fn sweet_rs_gpu_user_submit(iova: u64, dwords: u32) -> u32 {
    locked(|e| e.user_submit(iova, dwords), 0)
}

/// 1 when work up to `seq` has finished, 0 when not yet, -1 when the engine
/// is dead. Does not block.
#[unsafe(no_mangle)]
pub extern "C" fn sweet_rs_gpu_user_done(seq: u32) -> i32 {
    match unsafe { (*core::ptr::addr_of!(ENGINE)).as_ref() } {
        Some(e) if !e.dead => (fence_read(&e.mem).wrapping_sub(seq) as i32 >= 0) as i32,
        _ => -1,
    }
}

/// A wait for `seq` timed out: log the GPU state and stop using the engine.
#[unsafe(no_mangle)]
pub extern "C" fn sweet_rs_gpu_user_hung(seq: u32) {
    locked(
        |e| {
            crate::log!(
                "SWEET CP: user job {seq} hung: rptr {} wptr {} fence {} status {:#010x} int0 {:#010x} protect {:#010x}; GPU disabled",
                e.gpu.read(CP_RB_RPTR),
                e.wptr,
                fence_read(&e.mem),
                e.gpu.read(RBBM_STATUS),
                e.gpu.read(RBBM_INT_0_STATUS),
                e.gpu.read(CP_PROTECT_STATUS)
            );
            smmu_fault();
            e.dead = true;
        },
        (),
    )
}
