/*
 * Compile-time stand-in for <rtl-sdr.h>.
 *
 * nrsc5's private.h includes <rtl-sdr.h> unconditionally, even though sdroxide
 * only ever feeds HD Radio through nrsc5's pipe API and never touches the
 * library's own device driver. Rather than make a librtlsdr development package
 * a build requirement, declare the handful of entry points nrsc5.c names; the
 * weak definitions in rtlsdr_stubs.c satisfy the linker. A real librtlsdr
 * linked into a final binary overrides those weak stubs, so nrsc5_open() would
 * still work where the library is present.
 *
 * Keep this in step with rtlsdr_stubs.c.
 */

#ifndef SDROXIDE_NRSC5_RTL_SDR_STUB_H
#define SDROXIDE_NRSC5_RTL_SDR_STUB_H

#include <stdint.h>

typedef struct rtlsdr_dev rtlsdr_dev_t;
typedef void (*rtlsdr_read_async_cb_t)(unsigned char *buf, uint32_t len, void *ctx);

/* The tuner identifiers rtltcp.c names when it reports a remote dongle's
 * gains. Values are librtlsdr's, so a real library agrees with them. */
typedef enum {
    RTLSDR_TUNER_UNKNOWN = 0,
    RTLSDR_TUNER_E4000,
    RTLSDR_TUNER_FC0012,
    RTLSDR_TUNER_FC0013,
    RTLSDR_TUNER_FC2580,
    RTLSDR_TUNER_R820T,
    RTLSDR_TUNER_R828D
} rtlsdr_tuner_t;

int rtlsdr_open(rtlsdr_dev_t **dev, uint32_t index);
int rtlsdr_close(rtlsdr_dev_t *dev);
int rtlsdr_set_center_freq(rtlsdr_dev_t *dev, uint32_t freq);
uint32_t rtlsdr_get_center_freq(rtlsdr_dev_t *dev);
int rtlsdr_set_sample_rate(rtlsdr_dev_t *dev, uint32_t rate);
int rtlsdr_set_tuner_gain_mode(rtlsdr_dev_t *dev, int manual);
int rtlsdr_set_tuner_gain(rtlsdr_dev_t *dev, int gain);
int rtlsdr_get_tuner_gain(rtlsdr_dev_t *dev);
int rtlsdr_get_tuner_gains(rtlsdr_dev_t *dev, int *gains);
int rtlsdr_set_offset_tuning(rtlsdr_dev_t *dev, int on);
int rtlsdr_set_freq_correction(rtlsdr_dev_t *dev, int ppm);
int rtlsdr_set_bias_tee(rtlsdr_dev_t *dev, int on);
int rtlsdr_set_direct_sampling(rtlsdr_dev_t *dev, int on);
int rtlsdr_reset_buffer(rtlsdr_dev_t *dev);
int rtlsdr_read_sync(rtlsdr_dev_t *dev, void *buf, int len, int *n_read);
int rtlsdr_read_async(rtlsdr_dev_t *dev, rtlsdr_read_async_cb_t cb, void *ctx,
                      uint32_t buf_num, uint32_t buf_len);
int rtlsdr_cancel_async(rtlsdr_dev_t *dev);

#endif
