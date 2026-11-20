/*
 * skyios ir3c: SPIR-V -> Adreno ir3 machine code, on the host.
 *
 * Used to translate CoreAnimation's shaders for the SkyIOS Adreno 618 Metal
 * driver (the driver on the phone only ships pre-built ir3 binaries today).
 * Links mesa's freedreno ir3 compiler plus the SPIR-V front end; no GPU or
 * kernel driver is needed.
 *
 * usage: ir3c [options] shader.spv
 *   -s, --stage <vs|fs|cs>   shader stage (default: from the SPIR-V entry point)
 *   -e, --entry <name>       entry point (default "main")
 *   -g, --gpu <id>           Adreno gpu id (default 618 = a618)
 *   -o, --out <file>         write the ir3 binary (words, LE) to <file>
 *   -n, --nir                print NIR before compiling
 *   -d, --disasm             print ir3 disassembly (default: on)
 *   -h, --help
 */
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <unistd.h>

#include "compiler/glsl_types.h"
#include "compiler/spirv/nir_spirv.h"
#include "util/ralloc.h"

#include "freedreno_dev_info.h"
#include "ir3_compiler.h"
#include "ir3_shader.h"
#include "ir3_nir.h"

static void usage(const char *argv0) {
   fprintf(stderr,
           "usage: %s [-s stage] [-e entry] [-g gpu] [-o out.bin] [-n] [-d] shader.spv\n"
           "  stage: vs fs cs (default: from SPIR-V)\n",
           argv0);
}

static bool stage_from_name(const char *n, mesa_shader_stage *out) {
   static const struct {
      const char *name;
      mesa_shader_stage stage;
   } t[] = {
      {"vs", MESA_SHADER_VERTEX},     {"vert", MESA_SHADER_VERTEX},
      {"fs", MESA_SHADER_FRAGMENT},   {"frag", MESA_SHADER_FRAGMENT},
      {"cs", MESA_SHADER_COMPUTE},    {"comp", MESA_SHADER_COMPUTE},
      {"gs", MESA_SHADER_GEOMETRY},   {"tcs", MESA_SHADER_TESS_CTRL},
      {"tes", MESA_SHADER_TESS_EVAL},
   };
   for (unsigned i = 0; i < ARRAY_SIZE(t); i++)
      if (!strcmp(n, t[i].name)) {
         *out = t[i].stage;
         return true;
      }
   return false;
}

/* first OpEntryPoint in the module (the metallib translation produces one) */
static bool find_entry(const uint32_t *w, size_t n, char *name, size_t namesz,
                       mesa_shader_stage *stage) {
   const uint32_t *p = w + 5, *end = w + n;
   while (p < end) {
      uint32_t op = p[0] & 0xffff, cnt = p[0] >> 16;
      if (op == 15 /* SpvOpEntryPoint */ && cnt >= 3) {
         const char *s = (const char *)&p[3];
         size_t len = strnlen(s, (cnt - 3) * 4);
         if (len >= namesz)
            len = namesz - 1;
         memcpy(name, s, len);
         name[len] = 0;
         /* OpEntryPoint: execution model == stage */
         switch (p[1]) {
         case 0: *stage = MESA_SHADER_VERTEX; break;
         case 4: *stage = MESA_SHADER_FRAGMENT; break;
         case 5: *stage = MESA_SHADER_COMPUTE; break;
         case 3: *stage = MESA_SHADER_GEOMETRY; break;
         case 1: *stage = MESA_SHADER_TESS_CTRL; break;
         case 2: *stage = MESA_SHADER_TESS_EVAL; break;
         default: *stage = MESA_SHADER_NONE; break;
         }
         return true;
      }
      p += cnt;
   }
   return false;
}

int main(int argc, char **argv) {
   const char *path = NULL, *out = NULL, *entry = NULL;
   mesa_shader_stage stage = MESA_SHADER_NONE;
   uint32_t gpu_id = 618;
   bool want_nir = false, want_disasm = true;

   for (int i = 1; i < argc; i++) {
      const char *a = argv[i];
      if (!strcmp(a, "-h") || !strcmp(a, "--help")) {
         usage(argv[0]);
         return 0;
      } else if ((!strcmp(a, "-s") || !strcmp(a, "--stage")) && i + 1 < argc) {
         if (!stage_from_name(argv[++i], &stage)) {
            fprintf(stderr, "ir3c: unknown stage '%s'\n", argv[i]);
            return 2;
         }
      } else if ((!strcmp(a, "-e") || !strcmp(a, "--entry")) && i + 1 < argc) {
         entry = argv[++i];
      } else if ((!strcmp(a, "-g") || !strcmp(a, "--gpu")) && i + 1 < argc) {
         gpu_id = (uint32_t)strtoul(argv[++i], NULL, 0);
      } else if ((!strcmp(a, "-o") || !strcmp(a, "--out")) && i + 1 < argc) {
         out = argv[++i];
      } else if (!strcmp(a, "-n") || !strcmp(a, "--nir")) {
         want_nir = true;
      } else if (!strcmp(a, "-d") || !strcmp(a, "--disasm")) {
         want_disasm = true;
      } else if (!strcmp(a, "--no-disasm")) {
         want_disasm = false;
      } else if (a[0] == '-') {
         fprintf(stderr, "ir3c: unknown option %s\n", a);
         usage(argv[0]);
         return 2;
      } else {
         path = a;
      }
   }
   if (!path) {
      usage(argv[0]);
      return 2;
   }

   int fd = open(path, O_RDONLY);
   if (fd < 0) {
      fprintf(stderr, "ir3c: open %s: %s\n", path, strerror(errno));
      return 1;
   }
   struct stat st;
   if (fstat(fd, &st) != 0 || (st.st_size % 4) || st.st_size < 20) {
      fprintf(stderr, "ir3c: %s is not a SPIR-V module\n", path);
      return 1;
   }
   const uint32_t *words = mmap(NULL, st.st_size, PROT_READ, MAP_PRIVATE, fd, 0);
   if (words == MAP_FAILED) {
      fprintf(stderr, "ir3c: mmap: %s\n", strerror(errno));
      return 1;
   }
   size_t word_count = st.st_size / 4;
   if (words[0] != 0x07230203) {
      fprintf(stderr, "ir3c: bad SPIR-V magic 0x%08x\n", words[0]);
      return 1;
   }

   char ent[256] = "main";
   mesa_shader_stage mod_stage = MESA_SHADER_NONE;
   find_entry(words, word_count, ent, sizeof(ent), &mod_stage);
   if (entry)
      snprintf(ent, sizeof(ent), "%s", entry);
   if (stage == MESA_SHADER_NONE)
      stage = mod_stage != MESA_SHADER_NONE ? mod_stage : MESA_SHADER_FRAGMENT;

   void *mem_ctx = ralloc_context(NULL);
   glsl_type_singleton_init_or_ref();
   fprintf(stderr, "ir3c: stage %d entry %s\n", stage, ent);

   struct fd_dev_id dev_id = {.gpu_id = gpu_id};
   const struct fd_dev_info *dev_info = fd_dev_info_raw(&dev_id);
   if (!dev_info) {
      fprintf(stderr, "ir3c: unknown gpu id %u\n", gpu_id);
      return 1;
   }
   struct ir3_compiler_options copts = {.disable_cache = true};
   struct ir3_compiler *compiler = ir3_compiler_create(NULL, &dev_id, dev_info, &copts);
   fprintf(stderr, "ir3c: compiler %p (gen %d)\n", (void *)compiler, compiler->gen);

   struct spirv_to_nir_options sopts = {
      .environment = NIR_SPIRV_VULKAN,
      .ubo_addr_format = nir_address_format_32bit_offset_as_64bit,
      .ssbo_addr_format = nir_address_format_64bit_global,
      .phys_ssbo_addr_format = nir_address_format_64bit_global,
      .push_const_addr_format = nir_address_format_32bit_offset_as_64bit,
      .shared_addr_format = nir_address_format_32bit_offset,
      .global_addr_format = nir_address_format_64bit_global,
      .temp_addr_format = nir_address_format_32bit_offset,
      .constant_addr_format = nir_address_format_32bit_offset_as_64bit,
   };

   nir_shader *nir = spirv_to_nir(words, word_count, NULL, stage, ent, &sopts,
                                  &compiler->nir_options);
   if (!nir) {
      fprintf(stderr, "ir3c: spirv_to_nir failed\n");
      return 1;
   }
   if (want_nir)
      nir_print_shader(nir, stderr);

   /* lower subpassInput → txf_ms_fb (fb read) early, before var lowering */
   {
      nir_input_attachment_options ia_opts = {
         .use_ia_coord_intrin = false,
         .use_view_id_for_layer = false,
      };
      NIR_PASS(_, nir, nir_lower_input_attachments, &ia_opts);
   }
   /* the API driver normally does this before handing NIR to ir3: get rid of
    * derefs/variables so ir3 sees intrinsics only (turnip does the same) */
   NIR_PASS(_, nir, nir_lower_global_vars_to_local);
   NIR_PASS(_, nir, nir_lower_variable_initializers, ~0);
   NIR_PASS(_, nir, nir_split_var_copies);
   NIR_PASS(_, nir, nir_lower_var_copies);
   NIR_PASS(_, nir, nir_lower_vars_to_ssa);
   NIR_PASS(_, nir, nir_opt_deref);
   NIR_PASS(_, nir, nir_opt_copy_prop);
   NIR_PASS(_, nir, nir_opt_dce);
   NIR_PASS(_, nir, nir_opt_cse);
   nir_assign_io_var_locations(nir, nir_var_shader_in);
   nir_assign_io_var_locations(nir, nir_var_shader_out);
   nir->info.io_lowered = false;
   ir3_nir_lower_io(nir);
   if (want_nir) {
      fprintf(stderr, "ir3c: --- nir after io lowering ---\n");
      nir_print_shader(nir, stderr);
   }

   struct ir3_shader_options shader_opts = {
      .api_wavesize = IR3_DOUBLE_ONLY,
      .real_wavesize = IR3_SINGLE_OR_DOUBLE,
   };
   struct ir3_shader *shader = ir3_shader_from_nir(compiler, nir, &shader_opts);
   if (!shader) {
      fprintf(stderr, "ir3c: ir3_shader_from_nir failed\n");
      return 1;
   }

   struct ir3_shader_key key = {0};
   struct ir3_shader_variant *variant = ir3_shader_create_variant(shader, &key, false);
   if (!variant || !variant->bin) {
      fprintf(stderr, "ir3c: ir3 compile/assemble failed (%s)\n", nir->info.name ?: "?");
      return 1;
   }

   unsigned n = variant->info.sizedwords;
   fprintf(stderr, "ir3c: %s %s: %u dwords (%u bytes), max_reg %d, "
                   "constlen %u, double_threadsize %d, size %u\n",
           stage == MESA_SHADER_FRAGMENT ? "fs" : stage == MESA_SHADER_VERTEX ? "vs" : "sh",
           ent, n, n * 4, variant->info.max_reg, variant->info.constlen,
           variant->info.double_threadsize, variant->info.size);

   if (want_disasm) {
      /* print raw dwords first (useful when ir3_shader_disasm crashes) */
      fprintf(stderr, "ir3c: raw dwords:");
      for (unsigned i = 0; i < n; i++)
         fprintf(stderr, " %08x", variant->bin[i]);
      fprintf(stderr, "\n");
      /* ir3_shader_disasm crashes in standalone builds (missing disk_cache
       * / fd_bo context); raw dwords above are sufficient for now */
   }

   if (out) {
      FILE *f = fopen(out, "wb");
      if (!f) {
         fprintf(stderr, "ir3c: open %s: %s\n", out, strerror(errno));
         return 1;
      }
      if (fwrite(variant->bin, 4, n, f) != n) {
         fprintf(stderr, "ir3c: short write\n");
         return 1;
      }
      fclose(f);
      fprintf(stderr, "ir3c: wrote %s\n", out);
   }

   glsl_type_singleton_decref();
   ralloc_free(mem_ctx);
   return 0;
}
