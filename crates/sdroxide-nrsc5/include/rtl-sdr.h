/*
 * Compile-time stand-in for <rtl-sdr.h>.
 *
 * nrsc5's private.h includes <rtl-sdr.h> unconditionally, even though sdroxide
 * only ever feeds HD Radio through nrsc5's pipe API and never touches the
 * library's own device driver. Rather than make a librtlsdr development package
 * a build requirement, declare the handful of entry points nrsc5.c names; the
 * definitions in rtlsdr_stubs.c satisfy the linker.
 *
 * Every name is renamed first, for nrsc5 and the stubs alike, so the stubs are
 * private to this library: they cannot collide with, or be mistaken for, a
 * real librtlsdr anywhere else in a binary.
 *
 * Keep this in step with rtlsdr_stubs.c.
 */

#ifndef SDROXIDE_NRSC5_RTL_SDR_STUB_H
#define SDROXIDE_NRSC5_RTL_SDR_STUB_H

#define rtlsdr_open sdrx_nrsc5_rtlsdr_open
#define rtlsdr_close sdrx_nrsc5_rtlsdr_close
#define rtlsdr_set_center_freq sdrx_nrsc5_rtlsdr_set_center_freq
#define rtlsdr_get_center_freq sdrx_nrsc5_rtlsdr_get_center_freq
#define rtlsdr_set_sample_rate sdrx_nrsc5_rtlsdr_set_sample_rate
#define rtlsdr_set_tuner_gain_mode sdrx_nrsc5_rtlsdr_set_tuner_gain_mode
#define rtlsdr_set_tuner_gain sdrx_nrsc5_rtlsdr_set_tuner_gain
#define rtlsdr_get_tuner_gain sdrx_nrsc5_rtlsdr_get_tuner_gain
#define rtlsdr_get_tuner_gains sdrx_nrsc5_rtlsdr_get_tuner_gains
#define rtlsdr_set_offset_tuning sdrx_nrsc5_rtlsdr_set_offset_tuning
#define rtlsdr_set_freq_correction sdrx_nrsc5_rtlsdr_set_freq_correction
#define rtlsdr_set_bias_tee sdrx_nrsc5_rtlsdr_set_bias_tee
#define rtlsdr_set_direct_sampling sdrx_nrsc5_rtlsdr_set_direct_sampling
#define rtlsdr_reset_buffer sdrx_nrsc5_rtlsdr_reset_buffer
#define rtlsdr_read_sync sdrx_nrsc5_rtlsdr_read_sync
#define rtlsdr_read_async sdrx_nrsc5_rtlsdr_read_async
#define rtlsdr_cancel_async sdrx_nrsc5_rtlsdr_cancel_async

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
