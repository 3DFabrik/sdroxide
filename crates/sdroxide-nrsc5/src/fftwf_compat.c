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
 * The trigonometry is done once, when the plan is made. Computed in the inner
 * loop, as it first was, it cost two libm calls per coefficient per transform
 * — some 22,000 of them for each 2048-point FFT, several hundred FFTs a second
 * — for numbers that never change. The table holds the same `double`s the loop
 * computed, so the transform's output is bit for bit what it was.
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
    int     n;        /* the transform length the caller asked for */
    float  *in;
    float  *out;
    float  *work;
    double *wr;       /* cos(-2*pi*k/n), for k in [0, n/2) */
    double *wi;       /* sin(-2*pi*k/n), negated for FFTW_BACKWARD */
};

/* In-place, on interleaved re/im. `wr` and `wi` are the plan's twiddle table
 * for length `m`; the stage of length `len` uses every `m/len`-th entry. */
static void fft_pow2(float *a, int m, const double *wr_table, const double *wi_table)
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
                const double wr = wr_table[j * step];
                const double wi = wi_table[j * step];
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
    p->in = (float *)in;
    p->out = (float *)out;
    const size_t half = n > 1 ? (size_t)n / 2 : 1;
    p->work = (float *)malloc(sizeof(float) * 2 * (size_t)n);
    p->wr = (double *)malloc(sizeof(double) * half);
    p->wi = (double *)malloc(sizeof(double) * half);
    if (p->work == NULL || p->wr == NULL || p->wi == NULL) {
        free(p->work);
        free(p->wr);
        free(p->wi);
        free(p);
        return NULL;
    }
    const int inverse = sign == FFTW_BACKWARD;
    for (size_t k = 0; k < half; k++) {
        const double ang = -2.0 * M_PI * (double)k / (double)n;
        p->wr[k] = cos(ang);
        p->wi[k] = inverse ? -sin(ang) : sin(ang);
    }
    return (fftwf_plan)p;
}

void fftwf_execute(fftwf_plan plan)
{
    struct sdrx_nrsc5_plan_s *p = (struct sdrx_nrsc5_plan_s *)plan;
    if (p == NULL)
        return;
    memcpy(p->work, p->in, sizeof(float) * 2 * (size_t)p->n);
    fft_pow2(p->work, p->n, p->wr, p->wi);
    memcpy(p->out, p->work, sizeof(float) * 2 * (size_t)p->n);
}

void fftwf_destroy_plan(fftwf_plan plan)
{
    struct sdrx_nrsc5_plan_s *p = (struct sdrx_nrsc5_plan_s *)plan;
    if (p == NULL)
        return;
    free(p->work);
    free(p->wr);
    free(p->wi);
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