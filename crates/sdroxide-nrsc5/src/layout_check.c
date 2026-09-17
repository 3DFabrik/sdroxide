/* Build-time check that the Rust side reads `nrsc5_event_t` the way the C
 * library writes it.
 *
 * `src/lib.rs` declares, by hand, the event numbers it matches on and the
 * `#[repr(C)]` structs it reads out of the event union. Those are correct for
 * the pinned nrsc5, but a future submodule bump that adds an event or a struct
 * member would leave them silently wrong: an audio frame read as a MER report,
 * a flags field read from the middle of a pointer. So the Rust declarations are
 * mirrored here in C, where they can be compared against the real header, and
 * any disagreement stops the build instead.
 *
 * Keep each mirror identical to its Rust twin. A `#[repr(C)]` struct is laid
 * out exactly as the same declaration in C, so a mirror that matches upstream
 * means the Rust struct does too. The one symbol below exists only so the
 * object is not empty, which some archivers warn about. */

#include <stddef.h>
#include <stdint.h>

#include "nrsc5.h"

#define SAME_OFFSET(field, mirror_type, mirror_field)                                        \
    _Static_assert(offsetof(nrsc5_event_t, field) - offsetof(nrsc5_event_t, sync) ==         \
                       offsetof(mirror_type, mirror_field),                                   \
                   "nrsc5_event_t." #field " is not where src/lib.rs reads it")

/* The event numbers `NRS_EVENT_*` in src/lib.rs. */
_Static_assert(NRSC5_EVENT_SYNC == 2, "NRSC5_EVENT_SYNC moved");
_Static_assert(NRSC5_EVENT_LOST_SYNC == 3, "NRSC5_EVENT_LOST_SYNC moved");
_Static_assert(NRSC5_EVENT_MER == 4, "NRSC5_EVENT_MER moved");
_Static_assert(NRSC5_EVENT_BER == 5, "NRSC5_EVENT_BER moved");
_Static_assert(NRSC5_EVENT_AUDIO == 7, "NRSC5_EVENT_AUDIO moved");
_Static_assert(NRSC5_EVENT_AUDIO_SERVICE == 14, "NRSC5_EVENT_AUDIO_SERVICE moved");
_Static_assert(NRSC5_EVENT_STATION_NAME == 16, "NRSC5_EVENT_STATION_NAME moved");
_Static_assert(NRSC5_EVENT_STATION_SLOGAN == 17, "NRSC5_EVENT_STATION_SLOGAN moved");
_Static_assert(NRSC5_EVENT_STATION_MESSAGE == 18, "NRSC5_EVENT_STATION_MESSAGE moved");
/* `AUDIO_FLAG_UNAVAILABLE` in src/lib.rs. */
_Static_assert(NRSC5_AUDIO_FLAGS_UNAVAILABLE == 1, "NRSC5_AUDIO_FLAGS_UNAVAILABLE moved");

/* `NrsEvent`: the event number, then the union every member of which starts at
 * the same offset. */
struct mirror_event {
    unsigned int event;
    union {
        const void *align_pointer;
        size_t align_size;
    } u;
};
_Static_assert(offsetof(nrsc5_event_t, sync) == offsetof(struct mirror_event, u),
               "the event union is not where src/lib.rs reads it");
_Static_assert(offsetof(nrsc5_event_t, audio) == offsetof(nrsc5_event_t, sync),
               "the event union's members do not share an offset");

/* `NrsSync` */
struct mirror_sync {
    float freq_offset;
    int psmi;
    int pli;
    int hppi;
    int aabi;
    int rdbi;
};
SAME_OFFSET(sync.freq_offset, struct mirror_sync, freq_offset);
SAME_OFFSET(sync.psmi, struct mirror_sync, psmi);

/* `NrsBer` */
struct mirror_ber {
    float cber;
};
SAME_OFFSET(ber.cber, struct mirror_ber, cber);

/* `NrsMer` */
struct mirror_mer {
    float lower;
    float upper;
};
SAME_OFFSET(mer.lower, struct mirror_mer, lower);
SAME_OFFSET(mer.upper, struct mirror_mer, upper);

/* `NrsAudio` */
struct mirror_audio {
    unsigned int program;
    const int16_t *data;
    size_t count;
    unsigned int flags;
};
SAME_OFFSET(audio.program, struct mirror_audio, program);
SAME_OFFSET(audio.data, struct mirror_audio, data);
SAME_OFFSET(audio.count, struct mirror_audio, count);
SAME_OFFSET(audio.flags, struct mirror_audio, flags);

/* `NrsAudioService` */
struct mirror_audio_service {
    unsigned int program;
    unsigned int access;
    unsigned int type_;
    unsigned int codec_mode;
};
SAME_OFFSET(audio_service.program, struct mirror_audio_service, program);
SAME_OFFSET(audio_service.access, struct mirror_audio_service, access);
SAME_OFFSET(audio_service.codec_mode, struct mirror_audio_service, codec_mode);

/* `NrsName`, one pointer at the start, for all three station texts. */
struct mirror_name {
    const char *name;
};
SAME_OFFSET(station_name.name, struct mirror_name, name);
SAME_OFFSET(station_slogan.slogan, struct mirror_name, name);
SAME_OFFSET(station_message.message, struct mirror_name, name);

const int sdrx_nrsc5_layout_checked = 1;
