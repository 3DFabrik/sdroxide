/* The five single-precision FFTW entry points nrsc5 uses, implemented from
 * scratch because the DRM build already shadows the double-precision FFTW
 * names and the real library is a system dependency not to be assumed.
 *
 * A radix-2 Cooley-Tukey transform covers the two lengths the receive path
 * creates (2048 in FM, 256 in AM). Anything else returns a NULL plan, exactly
 * how a plan allocation failure surfaces to the caller. Both directions are
 * unnormalised, as in FFTW: a forward transform followed by a backward one
 * multiplies by the length.
 *
 * `fftwf_alloc_complex` returns 16-byte-aligned memory like FFTW's own
 * allocator; the peers only store sample pointers, so any alignment works, but
 * staying aligned costs nothing and matches the upstream contract. */

#include "fftw3.h"

#include <math.h>
#include <stdlib.h>
#include <string.h>

#ifdef _WIN32
#include <malloc.h>
#endif

#ifndef M_PI
# define M_PI 3.14159265358979323846
#endif

struct sdrx_nrsc5_plan_s {
    int    n;        /* the transform length the caller asked for */
    int    inverse;  /* FFTW_BACKWARD */
    float *in;
    float *out;
    float *work;
};

/* In-place, on interleaved re/im. */
static void fft_pow2(float *a, int m, int inverse)
{
    for (int i = 1, j = 0; i < m; i++) {
        int bit = m >> 1;
        for (; j & bit; bit >>= 1)
            j ^= bit;
        j ^= bit;
        if (i < j) {
            float tr = a[2 * i], ti = a[2 * i + 1];
            a[2 * i] = a[2 * j];
            a[2 * i + 1] = a[2 * j + 1];
            a[2 * j] = tr;
            a[2 * j + 1] = ti;
        }
    }
    for (int len = 2; len <= m; len <<= 1) {
        const int half = len >> 1;
        const int step = m / len;
        for (int i = 0; i < m; i += len) {
            for (int j = 0; j < half; j++) {
                const double ang = -2.0 * M_PI * (double)(j * step) / (double)m;
                const double wr = cos(ang);
                const double wi = inverse ? -sin(ang) : sin(ang);
                float *p = a + 2 * (i + j);
                float *q = a + 2 * (i + j + half);
                const float xr = (float)(q[0] * wr - q[1] * wi);
                const float xi = (float)(q[0] * wi + q[1] * wr);
                q[0] = p[0] - xr;
                q[1] = p[1] - xi;
                p[0] += xr;
                p[1] += xi;
            }
        }
    }
}

fftwf_plan fftwf_plan_dft_1d(int n, fftwf_complex *in, fftwf_complex *out,
                             int sign, unsigned flags)
{
    (void)flags;
    if (n < 1 || (n & (n - 1)) != 0)
        return NULL;
    struct sdrx_nrsc5_plan_s *p = (struct sdrx_nrsc5_plan_s *)calloc(1, sizeof(*p));
    if (p == NULL)
        return NULL;
    p->n = n;
    p->inverse = sign == FFTW_BACKWARD;
    p->in = (float *)in;
    p->out = (float *)out;
    p->work = (float *)malloc(sizeof(float) * 2 * (size_t)n);
    if (p->work == NULL) {
        free(p);
        return NULL;
    }
    return (fftwf_plan)p;
}

void fftwf_execute(fftwf_plan plan)
{
    struct sdrx_nrsc5_plan_s *p = (struct sdrx_nrsc5_plan_s *)plan;
    if (p == NULL)
        return;
    memcpy(p->work, p->in, sizeof(float) * 2 * (size_t)p->n);
    fft_pow2(p->work, p->n, p->inverse);
    memcpy(p->out, p->work, sizeof(float) * 2 * (size_t)p->n);
}

void fftwf_destroy_plan(fftwf_plan plan)
{
    struct sdrx_nrsc5_plan_s *p = (struct sdrx_nrsc5_plan_s *)plan;
    if (p == NULL)
        return;
    free(p->work);
    free(p);
}

fftwf_complex *fftwf_alloc_complex(int n)
{
    const size_t size = sizeof(float) * 2 * (size_t)(n > 0 ? n : 1);
    void *p = NULL;
#if defined(_WIN32)
    p = _aligned_malloc(size, 16);
    if (p == NULL)
        return NULL;
#else
    if (posix_memalign(&p, 16, size) != 0)
        return NULL;
#endif
    memset(p, 0, size);
    return (fftwf_complex *)p;
}

void fftwf_free(void *p)
{
#if defined(_WIN32)
    _aligned_free(p);
#else
    free(p);
#endif
}