/*
 * Compile-time shim for <complex.h>.
 *
 * nrsc5's defines.h uses the C11 CMPLXF() constructor. MinGW-w64's <complex.h>
 * does not provide it, so the Windows build fails on it with an implicit
 * declaration. Pull in the real header by its next-in-path name and supply the
 * macro only where it is missing; on every other platform this is a no-op.
 */

#ifndef SDROXIDE_NRSC5_COMPLEX_H
#define SDROXIDE_NRSC5_COMPLEX_H

#if defined(__has_include_next)
#if __has_include_next(<complex.h>)
#include_next <complex.h>
#endif
#else
#include_next <complex.h>
#endif

#ifndef CMPLXF
#define CMPLXF(r, i) __builtin_complex((float)(r), (float)(i))
#endif

#endif /* SDROXIDE_NRSC5_COMPLEX_H */
