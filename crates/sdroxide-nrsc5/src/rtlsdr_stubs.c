/*
 * Weak stubs for the librtlsdr entry points nrsc5.c references.
 *
 * sdroxide always feeds HD Radio through nrsc5's pipe API, never through the
 * library's own device driver, but nrsc5.c links against librtlsdr
 * unconditionally. These weak definitions satisfy the linker so the pipe
 * decoder builds and tests without an SDR library. A real librtlsdr linked
 * into a final binary overrides them — strong beats weak — so nrsc5_open()
 * would still work there.
 *
 * None of these run on the pipe path, so they all report failure.
 */

#include <stdint.h>

typedef struct rtlsdr_dev rtlsdr_dev_t;
typedef void (*rtlsdr_read_async_cb_t)(unsigned char *buf, uint32_t len, void *ctx);

#if defined(__GNUC__) || defined(__clang__)
#define WEAK __attribute__((weak))
#else
#define WEAK
#endif

WEAK int rtlsdr_open(rtlsdr_dev_t **dev, uint32_t index)
{
    (void)dev;
    (void)index;
    return -1;
}

WEAK int rtlsdr_close(rtlsdr_dev_t *dev)
{
    (void)dev;
    return -1;
}

WEAK int rtlsdr_set_center_freq(rtlsdr_dev_t *dev, uint32_t freq)
{
    (void)dev;
    (void)freq;
    return -1;
}

WEAK uint32_t rtlsdr_get_center_freq(rtlsdr_dev_t *dev)
{
    (void)dev;
    return 0;
}

WEAK int rtlsdr_set_sample_rate(rtlsdr_dev_t *dev, uint32_t rate)
{
    (void)dev;
    (void)rate;
    return -1;
}

WEAK int rtlsdr_set_tuner_gain_mode(rtlsdr_dev_t *dev, int manual)
{
    (void)dev;
    (void)manual;
    return -1;
}

WEAK int rtlsdr_set_tuner_gain(rtlsdr_dev_t *dev, int gain)
{
    (void)dev;
    (void)gain;
    return -1;
}

WEAK int rtlsdr_get_tuner_gain(rtlsdr_dev_t *dev)
{
    (void)dev;
    return 0;
}

WEAK int rtlsdr_get_tuner_gains(rtlsdr_dev_t *dev, int *gains)
{
    (void)dev;
    (void)gains;
    return 0;
}

WEAK int rtlsdr_set_offset_tuning(rtlsdr_dev_t *dev, int on)
{
    (void)dev;
    (void)on;
    return -1;
}

WEAK int rtlsdr_set_freq_correction(rtlsdr_dev_t *dev, int ppm)
{
    (void)dev;
    (void)ppm;
    return -1;
}

WEAK int rtlsdr_set_bias_tee(rtlsdr_dev_t *dev, int on)
{
    (void)dev;
    (void)on;
    return -1;
}

WEAK int rtlsdr_set_direct_sampling(rtlsdr_dev_t *dev, int on)
{
    (void)dev;
    (void)on;
    return -1;
}

WEAK int rtlsdr_reset_buffer(rtlsdr_dev_t *dev)
{
    (void)dev;
    return -1;
}

WEAK int rtlsdr_read_sync(rtlsdr_dev_t *dev, void *buf, int len, int *n_read)
{
    (void)dev;
    (void)buf;
    (void)len;
    (void)n_read;
    return -1;
}

WEAK int rtlsdr_read_async(rtlsdr_dev_t *dev, rtlsdr_read_async_cb_t cb, void *ctx,
                           uint32_t buf_num, uint32_t buf_len)
{
    (void)dev;
    (void)cb;
    (void)ctx;
    (void)buf_num;
    (void)buf_len;
    return -1;
}

WEAK int rtlsdr_cancel_async(rtlsdr_dev_t *dev)
{
    (void)dev;
    return -1;
}
