/*
 * skyios skyprog: compile a vertex + fragment shader pair (Vulkan SPIR-V)
 * for the Adreno 618 and print a C description of the linked program for the
 * SkyIOS Metal driver: both instruction streams, the program's context
 * registers (shader, VFD, VPC, FS input/output state, ported from mesa's
 * gallium fd6_program.cc for one VS + one FS on A6XX) and the constant file
 * layout (immediates, pushed UBO ranges).
 *
 * Resources: UBO binding N = Metal buffer N of that stage, texture binding N
 * = texture/sampler slot N of the program (the generator maps them to Metal
 * indices). Vertex input location N = Metal vertex attribute N.
 *
 * usage: skyprog NAME vs.spv fs.spv > NAME.h
 */
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <unistd.h>
#include <vector>

#include "compiler/glsl_types.h"
#include "compiler/nir/nir_builder.h"
#include "compiler/spirv/nir_spirv.h"
#include "util/ralloc.h"

#include "freedreno_common.h"
#include "freedreno_dev_info.h"
#include "ir3_compiler.h"
#include "ir3_nir.h"
#include "ir3_shader.h"

#include "fd6_hw.h"

struct fd_reg_pair {
   uint32_t reg;
   uint64_t value;
   struct fd_bo *bo;
   bool is_address;
   uint32_t bo_offset;
   uint32_t bo_shift;
   uint32_t bo_low;
};
#define __bo_type struct fd_bo *
/* the pack structs without fd6_pack.h (that one needs the drm/ringbuffer code) */
#define SKY_PACK_H "a6xx-pack.xml.h"
#include SKY_PACK_H

#define CHIP A6XX

/* register list collector, stands in for fd_crb */
struct crb {
   std::vector<std::pair<uint32_t, uint32_t>> regs;
   crb &add(struct fd_reg_pair r)
   {
      regs.push_back({r.reg, (uint32_t)r.value});
      return *this;
   }
};

/* --- SPIR-V -> NIR: the descriptor lowering a Vulkan driver would do --- */

/* vulkan_resource_index(binding) -> vec2(binding + index, 0), with
 * load_vulkan_descriptor a no-op: nir_address_format_32bit_index_offset */
static bool
lower_vk_resource(nir_builder *b, nir_intrinsic_instr *intr, void *data)
{
   if (intr->intrinsic == nir_intrinsic_vulkan_resource_index) {
      b->cursor = nir_before_instr(&intr->instr);
      nir_def *idx = nir_iadd_imm(b, intr->src[0].ssa, nir_intrinsic_binding(intr));
      nir_def_replace(&intr->def, nir_vec2(b, idx, nir_imm_int(b, 0)));
      return true;
   }
   if (intr->intrinsic == nir_intrinsic_load_vulkan_descriptor) {
      nir_def_replace(&intr->def, intr->src[0].ssa);
      return true;
   }
   return false;
}

/* texture/sampler derefs -> texture_index/sampler_index = binding */
static bool
lower_tex_deref(nir_builder *b, nir_instr *instr, void *data)
{
   if (instr->type != nir_instr_type_tex)
      return false;
   nir_tex_instr *tex = nir_instr_as_tex(instr);
   bool progress = false;
   for (;;) {
      int i = nir_tex_instr_src_index(tex, nir_tex_src_texture_deref);
      int s = nir_tex_instr_src_index(tex, nir_tex_src_sampler_deref);
      if (i < 0 && s < 0)
         break;
      int k = i >= 0 ? i : s;
      nir_deref_instr *d = nir_src_as_deref(tex->src[k].src);
      nir_variable *var = nir_deref_instr_get_variable(d);
      if (i >= 0) {
         tex->texture_index = var->data.binding;
         BITSET_SET(b->shader->info.textures_used, var->data.binding);
      } else {
         tex->sampler_index = var->data.binding;
         BITSET_SET(b->shader->info.samplers_used, var->data.binding);
      }
      nir_tex_instr_remove_src(tex, k);
      progress = true;
   }
   return progress;
}

static struct ir3_shader_variant *
compile(struct ir3_compiler *compiler, const char *path, mesa_shader_stage stage)
{
   int fd = open(path, O_RDONLY);
   struct stat st;
   if (fd < 0 || fstat(fd, &st) != 0 || (st.st_size % 4)) {
      fprintf(stderr, "skyprog: cannot read %s\n", path);
      exit(1);
   }
   const uint32_t *words =
      (const uint32_t *)mmap(NULL, st.st_size, PROT_READ, MAP_PRIVATE, fd, 0);

   struct spirv_to_nir_options sopts = {};
   sopts.environment = NIR_SPIRV_VULKAN;
   sopts.ubo_addr_format = nir_address_format_32bit_index_offset;
   sopts.ssbo_addr_format = nir_address_format_32bit_index_offset;
   sopts.push_const_addr_format = nir_address_format_32bit_offset;
   sopts.shared_addr_format = nir_address_format_32bit_offset;
   sopts.global_addr_format = nir_address_format_64bit_global;
   sopts.temp_addr_format = nir_address_format_32bit_offset;
   sopts.constant_addr_format = nir_address_format_32bit_offset;

   nir_shader *nir = spirv_to_nir(words, st.st_size / 4, NULL, stage, "main",
                                  &sopts, &compiler->nir_options);
   if (!nir) {
      fprintf(stderr, "skyprog: spirv_to_nir failed for %s\n", path);
      exit(1);
   }

   NIR_PASS(_, nir, nir_lower_global_vars_to_local);
   NIR_PASS(_, nir, nir_lower_variable_initializers, nir_var_all);
   NIR_PASS(_, nir, nir_split_var_copies);
   NIR_PASS(_, nir, nir_lower_var_copies);
   NIR_PASS(_, nir, nir_lower_vars_to_ssa);
   NIR_PASS(_, nir, nir_lower_system_values);
   NIR_PASS(_, nir, nir_shader_intrinsics_pass, lower_vk_resource,
            nir_metadata_control_flow, NULL);
   NIR_PASS(_, nir, nir_opt_deref);
   NIR_PASS(_, nir, nir_lower_explicit_io, nir_var_mem_ubo,
            nir_address_format_32bit_index_offset);
   NIR_PASS(_, nir, nir_shader_instructions_pass, lower_tex_deref,
            nir_metadata_control_flow, NULL);
   NIR_PASS(_, nir, nir_opt_copy_prop);
   NIR_PASS(_, nir, nir_opt_constant_folding);
   NIR_PASS(_, nir, nir_opt_dce);
   NIR_PASS(_, nir, nir_opt_cse);
   NIR_PASS(_, nir, nir_remove_dead_variables,
            (nir_variable_mode)(nir_var_uniform | nir_var_mem_ubo), NULL);

   unsigned num_ubos = 0, num_tex = 0;
   nir_foreach_variable_with_modes(var, nir, nir_var_mem_ubo)
      num_ubos = MAX2(num_ubos, var->data.binding + 1);
   nir_foreach_variable_with_modes(var, nir, nir_var_uniform)
      num_tex = MAX2(num_tex, var->data.binding + 1);
   nir->info.num_ubos = num_ubos;
   nir->info.num_textures = num_tex;

   nir_assign_io_var_locations(nir, nir_var_shader_in);
   nir_assign_io_var_locations(nir, nir_var_shader_out);
   ir3_nir_lower_io(nir);
   if (getenv("SKYPROG_NIR"))
      nir_print_shader(nir, stderr);

   struct ir3_shader_options shader_opts = {};
   shader_opts.api_wavesize = IR3_SINGLE_OR_DOUBLE;
   shader_opts.real_wavesize = IR3_SINGLE_OR_DOUBLE;
   struct ir3_shader *shader = ir3_shader_from_nir(compiler, nir, &shader_opts);
   struct ir3_shader_key key = {};
   struct ir3_shader_variant *v = ir3_shader_create_variant(shader, &key, false);
   if (!v || !v->bin) {
      fprintf(stderr, "skyprog: ir3 compile failed for %s\n", path);
      exit(1);
   }
   return v;
}

/* --- program state: fd6_program.cc, VS + FS only, A6XX --- */

static inline uint32_t
next_regid(uint32_t reg, uint32_t increment)
{
   return VALIDREG(reg) ? reg + increment : INVALID_REG;
}

static uint32_t
sp_xs_config(const struct ir3_shader_variant *v)
{
   return A6XX_SP_VS_CONFIG_ENABLED |
          COND(v->bindless_tex, A6XX_SP_VS_CONFIG_BINDLESS_TEX) |
          COND(v->bindless_samp, A6XX_SP_VS_CONFIG_BINDLESS_SAMP) |
          COND(v->bindless_ibo, A6XX_SP_VS_CONFIG_BINDLESS_UAV) |
          COND(v->bindless_ubo, A6XX_SP_VS_CONFIG_BINDLESS_UBO) |
          A6XX_SP_VS_CONFIG_NUAV(ir3_shader_num_uavs(v)) |
          A6XX_SP_VS_CONFIG_NTEX(v->num_samp) |
          A6XX_SP_VS_CONFIG_NSAMP(v->num_samp);
}

static void
emit_config(crb &crb, const ir3_shader_variant *vs, const ir3_shader_variant *fs)
{
   crb.add(SP_VS_CONST_CONFIG(CHIP, .constlen = vs->constlen, .enabled = true));
   crb.add(SP_HS_CONST_CONFIG(CHIP, .constlen = 0, .enabled = false));
   crb.add(SP_DS_CONST_CONFIG(CHIP, .constlen = 0, .enabled = false));
   crb.add(SP_GS_CONST_CONFIG(CHIP, .constlen = 0, .enabled = false));
   crb.add(SP_PS_CONST_CONFIG(CHIP, .constlen = fs->constlen, .enabled = true));
   crb.add(A6XX_SP_VS_CONFIG(.dword = sp_xs_config(vs)));
   crb.add(A6XX_SP_HS_CONFIG(.dword = 0));
   crb.add(A6XX_SP_DS_CONFIG(.dword = 0));
   crb.add(A6XX_SP_GS_CONFIG(.dword = 0));
   crb.add(A6XX_SP_PS_CONFIG(.dword = sp_xs_config(fs)));
   crb.add(SP_GFX_USIZE(CHIP, ir3_shader_num_uavs(fs)));
}

/* emit_shader_regs without the addresses (SP_xS_BASE is set by the driver)
 * and without private memory (refused below) */
static void
emit_shader_regs(crb &crb, const ir3_shader_variant *so)
{
   enum a6xx_threadsize thrsz = so->info.double_threadsize ? THREAD128 : THREAD64;
   if (so->type == MESA_SHADER_VERTEX) {
      crb.add(A6XX_SP_VS_CNTL_0(.halfregfootprint = (uint32_t)(so->info.max_half_reg + 1),
                                .fullregfootprint = (uint32_t)(so->info.max_reg + 1),
                                .branchstack = ir3_shader_branchstack_hw(so),
                                .mergedregs = so->mergedregs,
                                .earlypreamble = so->early_preamble, ));
      crb.add(A6XX_SP_VS_INSTR_SIZE(so->instrlen));
      crb.add(A6XX_SP_VS_PROGRAM_COUNTER_OFFSET());
      crb.add(A6XX_SP_VS_PVT_MEM_PARAM(.memsizeperitem = 0));
      crb.add(A6XX_SP_VS_PVT_MEM_SIZE(.totalpvtmemsize = 0, .perwavememlayout = false));
      crb.add(A6XX_SP_VS_PVT_MEM_STACK_OFFSET(.offset = 0));
   } else {
      crb.add(A6XX_SP_PS_CNTL_0(.halfregfootprint = (uint32_t)(so->info.max_half_reg + 1),
                                .fullregfootprint = (uint32_t)(so->info.max_reg + 1),
                                .branchstack = ir3_shader_branchstack_hw(so),
                                .threadsize = thrsz,
                                .varying = so->total_in != 0,
                                .lodpixmask = so->need_full_quad,
                                .inoutregoverlap = true,
                                .pixlodenable = so->need_pixlod,
                                .earlypreamble = so->early_preamble,
                                .mergedregs = so->mergedregs, ));
      crb.add(A6XX_SP_PS_INSTR_SIZE(so->instrlen));
      crb.add(A6XX_SP_PS_PROGRAM_COUNTER_OFFSET());
      crb.add(A6XX_SP_PS_PVT_MEM_PARAM(.memsizeperitem = 0));
      crb.add(A6XX_SP_PS_PVT_MEM_SIZE(.totalpvtmemsize = 0, .perwavememlayout = false));
      crb.add(A6XX_SP_PS_PVT_MEM_STACK_OFFSET(.offset = 0));
   }
}

static void
emit_vfd_dest(crb &crb, const struct ir3_shader_variant *vs)
{
   uint32_t attr_count = 0;
   for (uint32_t i = 0; i < vs->inputs_count; i++)
      if (!vs->inputs[i].sysval)
         attr_count++;
   crb.add(A6XX_VFD_CNTL_0(.fetch_cnt = attr_count, .decode_cnt = attr_count));
   for (uint32_t i = 0; i < attr_count; i++)
      crb.add(A6XX_VFD_DEST_CNTL_INSTR(i, .writemask = vs->inputs[i].compmask,
                                       .regid = vs->inputs[i].regid));
}

static void
emit_vpc(crb &crb, const ir3_shader_variant *vs, const ir3_shader_variant *fs)
{
   struct ir3_shader_linkage linkage = {};
   linkage.primid_loc = 0xff;
   linkage.clip0_loc = 0xff;
   linkage.clip1_loc = 0xff;
   ir3_link_shaders(&linkage, vs, fs, true);

   /* emit_vs_system_values */
   const uint32_t vertexid_regid = ir3_find_sysval_regid(vs, SYSTEM_VALUE_VERTEX_ID);
   const uint32_t instanceid_regid = ir3_find_sysval_regid(vs, SYSTEM_VALUE_INSTANCE_ID);
   crb.add(A6XX_VFD_CNTL_1(.regid4vtx = vertexid_regid, .regid4inst = instanceid_regid,
                           .regid4primid = INVALID_REG, .regid4viewid = INVALID_REG));
   crb.add(A6XX_VFD_CNTL_2(.regid_hsrelpatchid = INVALID_REG, .regid_invocationid = INVALID_REG));
   crb.add(A6XX_VFD_CNTL_3(.regid_dsprimid = INVALID_REG, .regid_dsrelpatchid = INVALID_REG,
                           .regid_tessx = INVALID_REG, .regid_tessy = INVALID_REG));
   crb.add(A6XX_VFD_CNTL_4(.regid_dsviewid = INVALID_REG));
   crb.add(A6XX_VFD_CNTL_5(.regid_gsheader = INVALID_REG, .regid_gsviewid = INVALID_REG));
   crb.add(A6XX_VFD_CNTL_6(.primid4psen = fs->reads_primid));

   for (unsigned i = 0; i < 4; i++)
      crb.add(VPC_VARYING_LM_TRANSFER_CNTL_DISABLE(CHIP, i, ~linkage.varmask[i]));

   const uint32_t position_regid = ir3_find_output_regid(vs, VARYING_SLOT_POS);
   const uint32_t pointsize_regid = ir3_find_output_regid(vs, VARYING_SLOT_PSIZ);
   uint32_t pointsize_loc = 0xff, position_loc = 0xff;
   if (position_regid != INVALID_REG) {
      position_loc = linkage.max_loc;
      ir3_link_add(&linkage, VARYING_SLOT_POS, position_regid, 0xf, linkage.max_loc);
   }
   if (pointsize_regid != INVALID_REG) {
      pointsize_loc = linkage.max_loc;
      ir3_link_add(&linkage, VARYING_SLOT_PSIZ, pointsize_regid, 0x1, linkage.max_loc);
   }
   if (linkage.cnt == 0)
      ir3_link_add(&linkage, 0, 0, 0x1, linkage.max_loc);

   const uint32_t sp_out_count = DIV_ROUND_UP(linkage.cnt, 2);
   const uint32_t sp_vpc_dst_count = DIV_ROUND_UP(linkage.cnt, 4);
   uint16_t sp_out[32] = {0};
   uint8_t sp_vpc_dst[32] = {0};
   for (uint32_t i = 0; i < linkage.cnt; i++) {
      sp_out[i] = A6XX_SP_VS_OUTPUT_REG_A_REGID(linkage.var[i].regid) |
                  A6XX_SP_VS_OUTPUT_REG_A_COMPMASK(linkage.var[i].compmask);
      sp_vpc_dst[i] = A6XX_SP_VS_VPC_DEST_REG_OUTLOC0(linkage.var[i].loc);
   }
   uint32_t *regs = (uint32_t *)sp_out;
   for (unsigned i = 0; i < sp_out_count; i++)
      crb.add(A6XX_SP_VS_OUTPUT_REG(i, .dword = regs[i]));
   regs = (uint32_t *)sp_vpc_dst;
   for (unsigned i = 0; i < sp_vpc_dst_count; i++)
      crb.add(A6XX_SP_VS_VPC_DEST_REG(i, .dword = regs[i]));

   crb.add(VPC_VS_CNTL(CHIP, .stride_in_vpc = linkage.max_loc, .positionloc = position_loc,
                       .psizeloc = pointsize_loc, ));
   crb.add(VPC_VS_CLIP_CULL_CNTL(CHIP, .clip_mask = 0, .clip_dist_03_loc = 0xff,
                                 .clip_dist_47_loc = 0xff, ));
   crb.add(VPC_VS_CLIP_CULL_CNTL_V2(CHIP, .clip_mask = 0, .clip_dist_03_loc = 0xff,
                                    .clip_dist_47_loc = 0xff, ));
   crb.add(GRAS_CL_VS_CLIP_CULL_DISTANCE(CHIP, .clip_mask = 0, .cull_mask = 0, ));
   crb.add(PC_VS_CNTL(CHIP, .stride_in_vpc = linkage.max_loc,
                      .psize = VALIDREG(pointsize_regid), .layer = false, .view = false,
                      .primitive_id = false, .clip_mask = 0, ));
   crb.add(A6XX_SP_VS_OUTPUT_CNTL(.out = linkage.cnt));
   crb.add(VPC_VS_SIV_CNTL(CHIP, .layerloc = 0xff, .viewloc = 0xff, .shadingrateloc = 0xff, ));
   crb.add(VPC_VS_SIV_CNTL_V2(CHIP, .layerloc = 0xff, .viewloc = 0xff, .shadingrateloc = 0xff, ));
   crb.add(GRAS_SU_VS_SIV_CNTL(CHIP, .writes_layer = false, .writes_view = false, ));
   crb.add(PC_PS_CNTL(CHIP, fs->reads_primid));
   crb.add(VPC_PS_CNTL(CHIP, .numnonposvar = fs->total_in, .primidloc = linkage.primid_loc,
                       .varying = !!fs->total_in, .viewidloc = linkage.viewid_loc, ));
}

static void
emit_fs_inputs(crb &crb, const struct ir3_shader_variant *fs, const struct fd_dev_info *info)
{
   uint32_t face_regid, coord_regid, zwcoord_regid, samp_id_regid;
   uint32_t ij_regid[IJ_COUNT];
   uint32_t smask_in_regid;
   bool sample_shading = fs->sample_shading;
   bool enable_varyings = fs->total_in > 0;

   samp_id_regid = ir3_find_sysval_regid(fs, SYSTEM_VALUE_SAMPLE_ID);
   smask_in_regid = ir3_find_sysval_regid(fs, SYSTEM_VALUE_SAMPLE_MASK_IN);
   face_regid = ir3_find_sysval_regid(fs, SYSTEM_VALUE_FRONT_FACE);
   coord_regid = ir3_find_sysval_regid(fs, SYSTEM_VALUE_FRAG_COORD);
   zwcoord_regid = VALIDREG(coord_regid) ? coord_regid + 2 : INVALID_REG;
   for (unsigned i = 0; i < ARRAY_SIZE(ij_regid); i++)
      ij_regid[i] = ir3_find_sysval_regid(fs, SYSTEM_VALUE_BARYCENTRIC_PERSP_PIXEL + i);

   crb.add(A6XX_SP_PS_INITIAL_TEX_LOAD_CNTL(.count = fs->num_sampler_prefetch,
                                            .ij_write_disable = !VALIDREG(ij_regid[IJ_PERSP_PIXEL]),
                                            .endofquad = fs->prefetch_end_of_quad, ));
   for (int i = 0; i < fs->num_sampler_prefetch; i++) {
      const struct ir3_sampler_prefetch *prefetch = &fs->sampler_prefetch[i];
      crb.add(SP_PS_INITIAL_TEX_LOAD_CMD(CHIP, i, .src = prefetch->src,
                                         .samp_id = prefetch->samp_id,
                                         .tex_id = prefetch->tex_id, .dst = prefetch->dst,
                                         .wrmask = prefetch->wrmask,
                                         .half = prefetch->half_precision,
                                         .bindless = prefetch->bindless,
                                         .cmd = TEX_PREFETCH_SAM, ));
   }
   crb.add(SP_LB_PARAM_LIMIT(CHIP, info->props.prim_alloc_threshold));
   crb.add(SP_REG_PROG_ID_0(CHIP, .faceregid = face_regid, .sampleid = samp_id_regid,
                            .samplemask = smask_in_regid,
                            .centerrhw = ij_regid[IJ_PERSP_CENTER_RHW], ));
   crb.add(SP_REG_PROG_ID_1(CHIP, .ij_persp_pixel = ij_regid[IJ_PERSP_PIXEL],
                            .ij_linear_pixel = ij_regid[IJ_LINEAR_PIXEL],
                            .ij_persp_centroid = ij_regid[IJ_PERSP_CENTROID],
                            .ij_linear_centroid = ij_regid[IJ_LINEAR_CENTROID], ));
   crb.add(SP_REG_PROG_ID_2(CHIP, .ij_persp_sample = ij_regid[IJ_PERSP_SAMPLE],
                            .ij_linear_sample = ij_regid[IJ_LINEAR_SAMPLE],
                            .xycoordregid = coord_regid, .zwcoordregid = zwcoord_regid, ));
   crb.add(SP_REG_PROG_ID_3(CHIP, .linelengthregid = INVALID_REG,
                            .foveationqualityregid = INVALID_REG, ));
   enum a6xx_threadsize thrsz = fs->info.double_threadsize ? THREAD128 : THREAD64;
   crb.add(SP_PS_WAVE_CNTL(CHIP, .threadsize = thrsz, .varyings = enable_varyings, ));

   bool need_size = !info->props.has_implicit_fragface_fragcoord_ij_linear &&
                    (fs->frag_face || fs->fragcoord_compmask != 0);
   bool need_size_persamp = false;
   if (VALIDREG(ij_regid[IJ_PERSP_CENTER_RHW])) {
      if (sample_shading)
         need_size_persamp = true;
      else
         need_size = true;
   }
   crb.add(GRAS_CL_INTERP_CNTL(CHIP, .ij_persp_pixel = VALIDREG(ij_regid[IJ_PERSP_PIXEL]),
                               .ij_persp_centroid = VALIDREG(ij_regid[IJ_PERSP_CENTROID]),
                               .ij_persp_sample = VALIDREG(ij_regid[IJ_PERSP_SAMPLE]),
                               .ij_linear_pixel = VALIDREG(ij_regid[IJ_LINEAR_PIXEL]) || need_size,
                               .ij_linear_centroid = VALIDREG(ij_regid[IJ_LINEAR_CENTROID]),
                               .ij_linear_sample = VALIDREG(ij_regid[IJ_LINEAR_SAMPLE]) || need_size_persamp,
                               .coord_mask = fs->fragcoord_compmask,
                               .faceness = fs->frag_face,
                               .centerrhw = VALIDREG(ij_regid[IJ_PERSP_CENTER_RHW]), ));
   crb.add(A6XX_RB_INTERP_CNTL(.ij_persp_pixel = VALIDREG(ij_regid[IJ_PERSP_PIXEL]),
                               .ij_persp_centroid = VALIDREG(ij_regid[IJ_PERSP_CENTROID]),
                               .ij_persp_sample = VALIDREG(ij_regid[IJ_PERSP_SAMPLE]),
                               .ij_linear_pixel = VALIDREG(ij_regid[IJ_LINEAR_PIXEL]) || need_size,
                               .ij_linear_centroid = VALIDREG(ij_regid[IJ_LINEAR_CENTROID]),
                               .ij_linear_sample = VALIDREG(ij_regid[IJ_LINEAR_SAMPLE]) || need_size_persamp,
                               .coord_mask = fs->fragcoord_compmask,
                               .interp_en = enable_varyings, ));
   crb.add(A6XX_RB_PS_INPUT_CNTL(.samplemask = VALIDREG(smask_in_regid),
                                 .postdepthcoverage = fs->post_depth_coverage,
                                 .faceness = fs->frag_face,
                                 .sampleid = VALIDREG(samp_id_regid),
                                 .fragcoordsamplemode = sample_shading ? FRAGCOORD_SAMPLE : FRAGCOORD_CENTER,
                                 .centerrhw = VALIDREG(ij_regid[IJ_PERSP_CENTER_RHW])));
   crb.add(A6XX_RB_PS_SAMPLEFREQ_CNTL(sample_shading));
   crb.add(GRAS_LRZ_PS_INPUT_CNTL(CHIP, .sampleid = VALIDREG(samp_id_regid),
                                  .fragcoordsamplemode = sample_shading ? FRAGCOORD_SAMPLE : FRAGCOORD_CENTER, ));
   crb.add(A6XX_GRAS_LRZ_PS_SAMPLEFREQ_CNTL(sample_shading));
}

static void
emit_fs_outputs(crb &crb, const struct ir3_shader_variant *fs)
{
   uint32_t posz_regid = ir3_find_output_regid(fs, FRAG_RESULT_DEPTH);
   uint32_t stencilref_regid = ir3_find_output_regid(fs, FRAG_RESULT_STENCIL);
   int output_reg_count = 0;
   uint32_t fragdata_regid[8];
   for (uint32_t i = 0; i < ARRAY_SIZE(fragdata_regid); i++) {
      unsigned slot = fs->color0_mrt ? FRAG_RESULT_COLOR : FRAG_RESULT_DATA0 + i;
      int output_idx = ir3_find_output(fs, (gl_varying_slot)slot);
      if (output_idx < 0) {
         fragdata_regid[i] = INVALID_REG;
         continue;
      }
      fragdata_regid[i] = ir3_get_output_regid(&fs->outputs[output_idx]);
      if (VALIDREG(fragdata_regid[i]))
         output_reg_count = i + 1;
   }
   crb.add(A6XX_SP_PS_OUTPUT_CNTL(.dual_color_in_enable = fs->dual_src_blend,
                                  .depth_regid = posz_regid, .sampmask_regid = INVALID_REG,
                                  .stencilref_regid = stencilref_regid, ));
   for (int i = 0; i < output_reg_count; i++)
      crb.add(A6XX_SP_PS_OUTPUT_REG(i, .regid = fragdata_regid[i] & ~HALF_REG_ID,
                                    .half_precision = !!(fragdata_regid[i] & HALF_REG_ID), ));
}

static void
emit_interp_state(crb &crb, const struct ir3_shader_variant *fs)
{
   uint32_t vinterp[8] = {0}, vpsrepl[8] = {0};
   for (int j = -1; (j = ir3_next_varying(fs, j)) < (int)fs->inputs_count;) {
      unsigned compmask = fs->inputs[j].compmask;
      uint32_t inloc = fs->inputs[j].inloc;
      if (fs->inputs[j].flat) {
         uint32_t loc = inloc;
         for (int i = 0; i < 4; i++) {
            if (compmask & (1 << i)) {
               vinterp[loc / 16] |= INTERP_FLAT << ((loc % 16) * 2);
               loc++;
            }
         }
      }
   }
   for (int i = 0; i < 8; i++)
      crb.add(VPC_VARYING_INTERP_MODE_MODE(CHIP, i, vinterp[i]));
   for (int i = 0; i < 8; i++)
      crb.add(VPC_VARYING_REPLACE_MODE_MODE(CHIP, i, vpsrepl[i]));
}

/* --- output --- */

static void
print_words(const char *name, const char *suffix, const uint32_t *w, unsigned n)
{
   printf("static const uint32_t %s_%s[%u] = {", name, suffix, n ? n : 1);
   for (unsigned i = 0; i < n; i++)
      printf("%s0x%08x,", i % 8 ? " " : "\n    ", w[i]);
   printf("%s\n};\n", n ? "" : "0");
}

/* immediates and pushed UBO ranges of one stage */
static void
print_consts(const char *name, const char *st, const struct ir3_shader_variant *v)
{
   const struct ir3_const_state *cs = ir3_const_state(v);
   uint32_t base = cs->allocs.max_const_offset_vec4;
   int size = DIV_ROUND_UP(v->imm_state.count, 4);
   size = MIN2(size + (int)base, (int)v->constlen) - (int)base;
   size = MAX2(size, 0);
   char sfx[16];
   snprintf(sfx, sizeof(sfx), "%s_imm", st);
   std::vector<uint32_t> imm(size * 4, 0);
   for (unsigned i = 0; i < (unsigned)size * 4 && i < v->imm_state.count; i++)
      imm[i] = v->imm_state.values[i];
   print_words(name, sfx, imm.data(), imm.size());
   printf("static const SkyProgUBO %s_%s_ubo[%u] = {", name, st, MAX2(cs->ubo_state.num_enabled, 1u));
   unsigned n = 0;
   for (unsigned i = 0; i < cs->ubo_state.num_enabled; i++) {
      const struct ir3_ubo_range *r = &cs->ubo_state.range[i];
      if (r->ubo.bindless || (int)r->ubo.block == cs->consts_ubo.idx) {
         fprintf(stderr, "skyprog: %s %s: unsupported UBO range (bindless or constant data)\n", name, st);
         exit(1);
      }
      if (16 * v->constlen <= r->offset)
         continue;
      uint32_t sz = MIN2(r->end - r->start, 16 * v->constlen - r->offset);
      printf("\n    {%u, %u, %u, %u},", r->ubo.block, r->start, sz, r->offset);
      n++;
   }
   printf("%s\n};\n", n ? "" : "{0}");
   printf("#define %s_%s_NUBO %u\n#define %s_%s_IMMBASE %u\n#define %s_%s_NIMM %zu\n", name, st, n, name, st,
          base * 4, name, st, imm.size());
   if (cs->consts_ubo.idx >= 0 && v->constant_data_size) {
      fprintf(stderr, "skyprog: %s %s: shader constant data not supported\n", name, st);
      exit(1);
   }
}

int
main(int argc, char **argv)
{
   if (argc != 4) {
      fprintf(stderr, "usage: skyprog NAME vs.spv fs.spv > NAME.h\n");
      return 2;
   }
   const char *name = argv[1];
   glsl_type_singleton_init_or_ref();
   struct fd_dev_id dev_id = {.gpu_id = 618};
   const struct fd_dev_info *info = fd_dev_info_raw(&dev_id);
   struct ir3_compiler_options copts = {};
   copts.disable_cache = true;
   struct ir3_compiler *compiler = ir3_compiler_create(NULL, &dev_id, info, &copts);

   struct ir3_shader_variant *vs = compile(compiler, argv[2], MESA_SHADER_VERTEX);
   struct ir3_shader_variant *fs = compile(compiler, argv[3], MESA_SHADER_FRAGMENT);
   for (const struct ir3_shader_variant *v : {vs, fs}) {
      if (v->pvtmem_size || v->early_preamble || ir3_shader_num_uavs(v)) {
         fprintf(stderr, "skyprog: %s: private memory/preamble/UAVs not supported\n", name);
         return 1;
      }
   }

   crb c;
   emit_config(c, vs, fs);
   emit_shader_regs(c, vs);
   emit_shader_regs(c, fs);
   c.add(PC_STEREO_RENDERING_CNTL(CHIP));
   emit_vfd_dest(c, vs);
   emit_vpc(c, vs, fs);
   emit_fs_inputs(c, fs, info);
   emit_fs_outputs(c, fs);
   emit_interp_state(c, fs);

   printf("/* skyprog %s: generated, do not edit */\n", name);
   print_words(name, "vs", vs->bin, vs->info.sizedwords);
   print_words(name, "fs", fs->bin, fs->info.sizedwords);
   printf("static const uint32_t %s_regs[%zu][2] = {", name, c.regs.size());
   for (auto &r : c.regs)
      printf("\n    {0x%04x, 0x%08x},", r.first, r.second);
   printf("\n};\n");
   print_consts(name, "vs", vs);
   print_consts(name, "fs", fs);

   /* vertex attributes in VFD fetch order: Metal attribute index */
   unsigned nattr = 0;
   printf("static const uint8_t %s_attrs[] = {", name);
   for (uint32_t i = 0; i < vs->inputs_count; i++) {
      if (vs->inputs[i].sysval)
         continue;
      /* holes left by unused inputs decode nothing: 255 */
      printf("%s%u", nattr++ ? ", " : "",
             vs->inputs[i].compmask ? vs->inputs[i].slot - VERT_ATTRIB_GENERIC0 : 255);
   }
   printf("%s};\n", nattr ? "" : "0");

   unsigned preload = info->props.instr_cache_size;
   printf("static const SkyProg %s = {\n"
          "    \"%s\", %s_vs, %u, %s_fs, %u, %s_regs, %zu,\n"
          "    %u, %s_vs_imm, %s_vs_NIMM, %s_vs_IMMBASE, %s_vs_ubo, %s_vs_NUBO,\n"
          "    %u, %s_fs_imm, %s_fs_NIMM, %s_fs_IMMBASE, %s_fs_ubo, %s_fs_NUBO,\n"
          "    %s_attrs, %u, %u, %u, %u,\n};\n",
          name, name, name, vs->info.sizedwords, name, fs->info.sizedwords, name, c.regs.size(),
          vs->constlen, name, name, name, name, name,
          fs->constlen, name, name, name, name, name,
          name, nattr, MIN2(vs->instrlen, preload), MIN2(fs->instrlen, preload), fs->num_samp);
   fprintf(stderr, "skyprog: %s: vs %u dw constlen %u, fs %u dw constlen %u in %u samp %u prefetch %u\n",
           name, vs->info.sizedwords, vs->constlen, fs->info.sizedwords, fs->constlen, fs->total_in,
           fs->num_samp, fs->num_sampler_prefetch);
   return 0;
}
