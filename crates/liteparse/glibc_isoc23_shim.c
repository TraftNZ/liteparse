/*
 * glibc C23 strtol-family compatibility shim.
 *
 * The prebuilt ONNX Runtime static library that the `ort`/`ort-sys` crate
 * downloads is compiled against glibc >= 2.38, where strtol/strtoul/strtoll/
 * strtoull (and their _l locale variants) resolve to the C23 entry points
 * __isoc23_strtol etc. Those symbols do not exist in older glibc, so linking
 * the OCR binary on a host with glibc < 2.38 fails with "undefined symbol".
 *
 * The C23 variants differ from the classic ones only in that base-0 parsing
 * recognises the "0b"/"0B" binary prefix. For the integer bases ONNX Runtime
 * actually uses this is behaviourally identical, so forwarding to the classic
 * glibc functions is a safe, root-cause fix for the symbol-version mismatch
 * rather than a behavioural workaround.
 *
 * The classic functions are declared here with explicit assembler names
 * instead of through <stdlib.h>. On glibc >= 2.38 the header redirects
 * strtol to __isoc23_strtol under _GNU_SOURCE or C23, which would turn each
 * shim into a call to itself (an infinite `jmp` loop at -O2). The asm labels
 * bind to the classic symbols whatever the header does.
 *
 * build.rs links this shim only when the build host's glibc predates 2.38; a
 * newer glibc exports the __isoc23_* symbols itself.
 */
#include <locale.h>

#define ISOC23_WEAK __attribute__((weak))

extern long classic_strtol(const char *, char **, int) __asm__("strtol");
extern unsigned long classic_strtoul(const char *, char **, int) __asm__("strtoul");
extern long long classic_strtoll(const char *, char **, int) __asm__("strtoll");
extern unsigned long long classic_strtoull(const char *, char **, int) __asm__("strtoull");
extern long long classic_strtoll_l(const char *, char **, int, locale_t) __asm__("strtoll_l");
extern unsigned long long classic_strtoull_l(const char *, char **, int, locale_t) __asm__("strtoull_l");

ISOC23_WEAK long __isoc23_strtol(const char *nptr, char **endptr, int base) {
    return classic_strtol(nptr, endptr, base);
}

ISOC23_WEAK unsigned long __isoc23_strtoul(const char *nptr, char **endptr, int base) {
    return classic_strtoul(nptr, endptr, base);
}

ISOC23_WEAK long long __isoc23_strtoll(const char *nptr, char **endptr, int base) {
    return classic_strtoll(nptr, endptr, base);
}

ISOC23_WEAK unsigned long long __isoc23_strtoull(const char *nptr, char **endptr, int base) {
    return classic_strtoull(nptr, endptr, base);
}

ISOC23_WEAK long long __isoc23_strtoll_l(const char *nptr, char **endptr, int base, locale_t loc) {
    return classic_strtoll_l(nptr, endptr, base, loc);
}

ISOC23_WEAK unsigned long long __isoc23_strtoull_l(const char *nptr, char **endptr, int base, locale_t loc) {
    return classic_strtoull_l(nptr, endptr, base, loc);
}
