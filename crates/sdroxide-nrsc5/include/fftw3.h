/* The single-precision FFTW entry points declared to nrsc5 by this header.
 *
 * Implemented by `src/fftwf_compat.c`. nrsc5 only ever creates power-of-two
 * transforms (2048 in FM, 256 in AM), so the stand-in is a plain radix-2
 * Cooley-Tukey with no mixed-radix or Bluestein machinery. Only the five
 * symbols below are used anywhere in the receive path.
 *
 * Unlike the DRM build (`crates/sdroxide-drm`), which shadows the real FFTW
 * plan type, nrsc5 never publishes these pointers, so an opaque handle is
 * enough here.
 */

#ifndef SDRX_NRSC5_FFTW3_H
#define SDRX_NRSC5_FFTW3_H

#include <complex.h>
#include <stdlib.h>

#ifdef __cplusplus
extern "C" {
#endif

/* FFTW's own typedef: `fftwf_complex` is the C99 complex type, not a pair of
 * floats — nrsc5's acquire path passes `float complex *` around. */
typedef float complex fftwf_complex;
typedef void *fftwf_plan;

#define FFTW_FORWARD (-1)
#define FFTW_BACKWARD (+1)
#define FFTW_ESTIMATE (1U << 6)

fftwf_plan fftwf_plan_dft_1d(int n, fftwf_complex *in, fftwf_complex *out,
                             int sign, unsigned flags);
void fftwf_execute(fftwf_plan p);
void fftwf_destroy_plan(fftwf_plan p);
fftwf_complex *fftwf_alloc_complex(int n);
void fftwf_free(void *p);

#ifdef __cplusplus
}
#endif

#endif /* SDRX_NRSC5_FFTW3_H */