// Golden-value generator for the WP2a port (background_noise, expand, normal,
// merge). Builds against the WebRTC sources at rev
// 6341cca49ffcad6fcb4603bd6cd1325fd0fe28bd (generic-C SPL) and prints one
// FNV-1a hash per group. neteq/src/neteq/wp2a_golden_tests.rs replays the same
// xorshift32 input stream and compares hashes. Build with build_wp2a.sh.
//
// Rng and Hash are copies of the ones in golden_gen.cc (WP1).
#include <algorithm>
#include <cstdint>
#include <cstdio>
#include <cstring>
#include <vector>

#include "api/neteq/neteq.h"
#include "api/neteq/tick_timer.h"
#include "modules/audio_coding/codecs/cng/webrtc_cng.h"
#include "modules/audio_coding/neteq/audio_multi_vector.h"
#include "modules/audio_coding/neteq/background_noise.h"
#include "modules/audio_coding/neteq/decoder_database.h"
#include "modules/audio_coding/neteq/expand.h"
#include "modules/audio_coding/neteq/merge.h"
#include "modules/audio_coding/neteq/normal.h"
#include "modules/audio_coding/neteq/random_vector.h"
#include "modules/audio_coding/neteq/statistics_calculator.h"
#include "modules/audio_coding/neteq/sync_buffer.h"

// Normal only touches DecoderDatabase in its kRfc3389Cng branch, through the
// virtual GetActiveCngDecoder(). Constructing a real DecoderDatabase needs an
// Environment and the codec factory stack, so the goldens never take that
// branch (the Rust unit tests cover it) and decoder_database.cc is not linked.
// normal.cc still references ComfortNoiseDecoder::Generate; it is unreachable.
namespace webrtc {
bool ComfortNoiseDecoder::Generate(std::span<int16_t>, bool) {
  abort();
}
}  // namespace webrtc

using namespace webrtc;

struct Rng {
  uint32_t s;
  uint32_t next() { s ^= s << 13; s ^= s >> 17; s ^= s << 5; return s; }
  uint32_t below(uint32_t n) { return next() % n; }
  int range(int lo, int hi) { return lo + (int)below((uint32_t)(hi - lo + 1)); }
  int16_t i16() {
    switch (below(8)) {
      case 0: return -32768;
      case 1: return 32767;
      case 2: case 3: return (int16_t)next();
      default: return (int16_t)((int32_t)below(2001) - 1000);
    }
  }
};
static const uint32_t kSeed = 0x9e3779b9u;

struct Hash {
  uint64_t h = 0xcbf29ce484222325ULL;
  void feed(int64_t v) { h ^= (uint64_t)v; h *= 0x100000001b3ULL; }
  void feed16(const int16_t* v, size_t n) { for (size_t i = 0; i < n; ++i) feed(v[i]); }
};
static void report(const char* name, const Hash& h) {
  printf("%s 0x%016llx\n", name, (unsigned long long)h.h);
}

static const int kRates[4] = {8000, 16000, 32000, 48000};

// ---------------------------------------------------------------------------
// Deterministic test signals (integer only, mirrored in Rust).

// Pulse train of `period` samples through a two-pole resonator, plus noise.
static void gen_voiced(Rng& r, int16_t* out, size_t len, int period, int amp,
                       int noise) {
  int64_t y1 = 0, y2 = 0;
  for (size_t n = 0; n < len; ++n) {
    int64_t x = (n % (size_t)period == 0) ? amp : 0;
    x += r.range(-noise, noise);
    int64_t y = x + ((29000 * y1 - 15000 * y2) >> 14);
    if (y > 32767) y = 32767;
    if (y < -32768) y = -32768;
    y2 = y1;
    y1 = y;
    out[n] = (int16_t)y;
  }
}

static void gen_noise(Rng& r, int16_t* out, size_t len, int amp) {
  for (size_t n = 0; n < len; ++n) out[n] = (int16_t)r.range(-amp, amp);
}

// kind: 0 voiced, 1 noise, 2 low noise, 3 loud voiced (saturating),
// 4 silence, 5 onset (quiet, then voiced at the end), 6 DC (singular LPC).
static void gen_signal(Rng& r, int kind, int16_t* out, size_t len,
                       int fs_mult) {
  switch (kind) {
    case 0: {
      // Draws are hoisted: C++ argument evaluation order is unspecified.
      int period = r.range(20 * fs_mult, 115 * fs_mult);
      int amp = r.range(200, 1500);
      gen_voiced(r, out, len, period, amp, 30);
      break;
    }
    case 1: gen_noise(r, out, len, r.range(500, 4000)); break;
    case 2: gen_noise(r, out, len, r.range(1, 40)); break;
    case 3: gen_voiced(r, out, len, r.range(20 * fs_mult, 115 * fs_mult), 9000, 3000); break;
    case 4: memset(out, 0, len * sizeof(int16_t)); break;
    case 6: {
      int16_t dc = (int16_t)r.range(-20000, 20000);
      for (size_t n = 0; n < len; ++n) out[n] = dc;
      break;
    }
    default: {
      size_t tail = std::min(len, (size_t)(r.range(10, 150) * fs_mult));
      gen_noise(r, out, len - tail, 10);
      gen_voiced(r, out + len - tail, tail, r.range(20 * fs_mult, 115 * fs_mult), 1200, 30);
      break;
    }
  }
}

static void fill_sync(Rng& r, SyncBuffer& sb, int kind, int fs_mult) {
  std::vector<int16_t> tmp(sb.Size());
  for (size_t ch = 0; ch < sb.Channels(); ++ch) {
    gen_signal(r, kind, tmp.data(), tmp.size(), fs_mult);
    sb.Channel(ch).OverwriteAt(tmp.data(), tmp.size(), 0);
  }
}

// Low-level noise into a separate vector, then BackgroundNoise::Update a few
// times so the estimate is initialized.
static void prime_bgn(Rng& r, BackgroundNoise& bgn, size_t channels) {
  AudioMultiVector v(channels, 256);
  int16_t tmp[256];
  int rounds = r.range(1, 3);
  for (int k = 0; k < rounds; ++k) {
    for (size_t ch = 0; ch < channels; ++ch) {
      gen_noise(r, tmp, 256, r.range(1, 40));
      v[ch].OverwriteAt(tmp, 256, 0);
    }
    bgn.Update(v);
  }
}

static void feed_vec(Hash& h, const AudioVector& v) {
  h.feed((int64_t)v.Size());
  for (size_t i = 0; i < v.Size(); ++i) h.feed(v[i]);
}
static void feed_multi(Hash& h, const AudioMultiVector& v) {
  h.feed((int64_t)v.Channels());
  for (size_t ch = 0; ch < v.Channels(); ++ch) feed_vec(h, v[ch]);
}
static void feed_tail(Hash& h, const SyncBuffer& sb, size_t n) {
  h.feed((int64_t)sb.Size());
  h.feed((int64_t)sb.next_index());
  n = std::min(n, sb.Size());
  for (size_t ch = 0; ch < sb.Channels(); ++ch)
    for (size_t i = sb.Size() - n; i < sb.Size(); ++i) h.feed(sb[ch][i]);
}
static void feed_bgn(Hash& h, const BackgroundNoise& bgn, size_t channels) {
  h.feed(bgn.initialized());
  for (size_t ch = 0; ch < channels; ++ch) {
    h.feed(bgn.Energy(ch));
    h.feed(bgn.MuteFactor(ch));
    h.feed(bgn.Scale(ch));
    h.feed(bgn.ScaleShift(ch));
    h.feed16(bgn.Filter(ch), BackgroundNoise::kMaxLpcOrder + 1);
    h.feed16(bgn.FilterState(ch), BackgroundNoise::kMaxLpcOrder);
  }
}
static void feed_stats(Hash& h, const StatisticsCalculator& stats) {
  NetEqLifetimeStatistics s = stats.GetLifetimeStatistics();
  h.feed((int64_t)s.delayed_packet_outage_samples);
  h.feed((int64_t)s.delayed_packet_outage_events);
  h.feed((int64_t)s.concealed_samples);
  h.feed((int64_t)s.concealment_events);
  h.feed(s.interruption_count);
  h.feed(s.total_interruption_duration_ms);
}
static void feed_expand(Hash& h, const Expand& e, size_t channels) {
  for (size_t ch = 0; ch < channels; ++ch) h.feed(e.MuteFactor(ch));
  h.feed(e.Muted());
  h.feed((int64_t)e.max_lag());
  h.feed((int64_t)e.overlap_length());
}

// Bundles what NetEqImpl owns around Expand.
struct Rig {
  int fs;
  int fs_mult;
  size_t channels;
  TickTimer timer;
  BackgroundNoise bgn;
  SyncBuffer sb;
  RandomVector rv;
  StatisticsCalculator stats;
  Expand expand;
  Rig(int fs, size_t channels, size_t sb_len)
      : fs(fs), fs_mult(fs / 8000), channels(channels), bgn(channels),
        sb(channels, sb_len), stats(&timer),
        expand(&bgn, &sb, &rv, &stats, fs, channels) {}
};

// Common setup: fill the sync buffer, maybe prime BGN and the random vector.
static void setup_rig(Rng& r, Rig& rig, int kind) {
  fill_sync(r, rig.sb, kind, rig.fs_mult);
  if (r.below(2) == 0) prime_bgn(r, rig.bgn, rig.channels);
  if (r.below(3) == 0) rig.rv.set_seed_increment((int16_t)r.range(-300, 300));
  rig.stats.DecodedOutputPlayed();
}

// One Expand::Process call with the NetEqImpl-style bookkeeping around it.
static void expand_once(Rng& r, Hash& h, Rig& rig, AudioMultiVector& out,
                        bool first) {
  if (r.below(3) == 0) out.Clear();
  h.feed(rig.expand.Process(&out));
  rig.stats.ExpandedVoiceSamples(out.Size(), first);
  feed_multi(h, out);
  feed_expand(h, rig.expand, rig.channels);
  feed_tail(h, rig.sb, 2 * rig.expand.overlap_length());
  h.feed(rig.bgn.MuteFactor(0));
  h.feed16(rig.bgn.FilterState(0), BackgroundNoise::kMaxLpcOrder);
  h.feed(rig.rv.seed_increment());
  if (r.below(2) == 0) rig.sb.PushBack(out);
}

// ---------------------------------------------------------------------------

static void wp2a_background_noise() {
  Rng r{kSeed}; Hash h;
  BackgroundNoise bgn1(1), bgn2(2);
  for (int n = 0; n < 400; ++n) {
    size_t channels = 1 + r.below(2);
    BackgroundNoise& bgn = channels == 1 ? bgn1 : bgn2;
    int fs_mult = 1 << r.below(3);
    if (r.below(40) == 0) bgn.Reset();
    size_t len = 256 + r.below(300);
    AudioMultiVector v(channels, len);
    std::vector<int16_t> tmp(len);
    for (size_t ch = 0; ch < channels; ++ch) {
      // Bias towards quiet signals so the estimate actually updates.
      int kind = r.below(3) == 0 ? (int)r.below(7) : 2;
      gen_signal(r, kind, tmp.data(), len, fs_mult);
      v[ch].OverwriteAt(tmp.data(), len, 0);
    }
    h.feed(bgn.Update(v));
    feed_bgn(h, bgn, channels);
    if (r.below(2) == 0) {
      size_t ch = r.below(channels);
      size_t num = r.range(8, 750);
      std::vector<int16_t> random(750);
      for (auto& x : random) x = (int16_t)r.range(-8000, 8000);
      bgn.SetMuteFactor(ch, (int16_t)r.range(0, 16384));
      std::vector<int16_t> buffer(8 + num);
      for (auto& x : buffer) x = r.i16();
      int mute_slope = r.range(0, 5000);
      bool too_many = r.below(2) == 1;
      bgn.GenerateBackgroundNoise(random, ch, mute_slope, too_many, num,
                                  buffer.data());
      h.feed16(buffer.data(), buffer.size());
      feed_bgn(h, bgn, channels);
    }
  }
  report("wp2a_background_noise", h);
}

static void wp2a_expand() {
  Rng r{kSeed}; Hash h;
  for (int fs : kRates) {
    for (size_t channels = 1; channels <= 2; ++channels) {
      for (int kind = 0; kind < 7; ++kind) {
        int fs_mult = fs / 8000;
        Rig rig(fs, channels, (size_t)(300 + r.below(100)) * fs_mult);
        setup_rig(r, rig, kind);
        AudioMultiVector out(channels);
        // One long run per rate goes past kMaxConsecutiveExpands.
        int calls = kind == 0 && channels == 1 ? 210 : 14;
        bool first = true;
        for (int k = 0; k < calls; ++k) {
          // The long run takes no detours so it reaches the cap.
          switch (calls > 100 ? 99u : r.below(16)) {
            case 0: rig.expand.Reset(); first = true; break;
            case 1: rig.expand.SetParametersForMergeAfterExpand(); break;
            case 2: rig.expand.SetParametersForNormalAfterExpand(); break;
            default: break;
          }
          expand_once(r, h, rig, out, first);
          first = false;
        }
        feed_stats(h, rig.stats);
        feed_bgn(h, rig.bgn, channels);
      }
    }
  }
  report("wp2a_expand", h);
}

static void wp2a_normal() {
  Rng r{kSeed}; Hash h;
  for (int fs : kRates) {
    for (size_t channels = 1; channels <= 2; ++channels) {
      for (int iter = 0; iter < 12; ++iter) {
        int fs_mult = fs / 8000;
        Rig rig(fs, channels, (size_t)(300 + r.below(100)) * fs_mult);
        setup_rig(r, rig, (int)r.below(7));
        // `decoder_database` is only used for kRfc3389Cng (see above).
        Normal normal(fs, /*decoder_database=*/nullptr, rig.bgn,
                      &rig.expand, &rig.stats);
        AudioMultiVector out(channels);
        int expands = (int)r.below(4);
        for (int k = 0; k < expands; ++k) expand_once(r, h, rig, out, k == 0);

        NetEq::Mode mode;
        switch (r.below(8)) {
          case 0: mode = NetEq::Mode::kNormal; break;
          case 1: mode = NetEq::Mode::kCodecPlc; break;
          case 2: mode = NetEq::Mode::kMerge; break;
          default: mode = NetEq::Mode::kExpand; break;
        }
        size_t per_channel;
        switch (r.below(4)) {
          case 0: per_channel = fs / 100; break;
          case 1: per_channel = fs / 50; break;
          case 2: per_channel = 63; break;
          default: per_channel = r.range(1, 3 * fs / 100); break;
        }
        size_t length = per_channel * channels;
        if (r.below(10) == 0) length += 1;
        std::vector<int16_t> input(length + 1);
        gen_signal(r, (int)r.below(4), input.data(), input.size(), fs_mult);
        out.Clear();
        h.feed(normal.Process(input.data(), length, mode, &out));
        feed_multi(h, out);
        feed_expand(h, rig.expand, channels);
        feed_stats(h, rig.stats);
        feed_tail(h, rig.sb, 2 * rig.expand.overlap_length());
      }
    }
  }
  report("wp2a_normal", h);
}

static void wp2a_merge() {
  Rng r{kSeed}; Hash h;
  for (int fs : kRates) {
    for (size_t channels = 1; channels <= 2; ++channels) {
      for (int iter = 0; iter < 12; ++iter) {
        int fs_mult = fs / 8000;
        size_t overlap = 5 * fs_mult;
        size_t tpc = fs / 100;
        Rig rig(fs, channels, (size_t)(720 * fs / 1000));
        setup_rig(r, rig, (int)r.below(7));
        Merge merge(fs, channels, &rig.expand, &rig.sb);
        AudioMultiVector out(channels);
        int expands = 1 + (int)r.below(3);
        for (int k = 0; k < expands; ++k) expand_once(r, h, rig, out, k == 0);

        // How much of the sync buffer is still unplayed (Merge's old_length).
        size_t future;
        switch (r.below(4)) {
          case 0: future = overlap; break;
          case 1: future = tpc / 2; break;
          case 2: future = tpc + overlap; break;
          default: future = r.range((int)overlap, (int)(tpc + overlap)); break;
        }
        bool big = fs == 48000 && r.below(4) == 0;
        if (big) future = 1260 + r.below(200);  // The >= 210 * 6 branch.
        rig.sb.set_next_index(rig.sb.Size() - future);

        size_t per_channel;
        switch (r.below(7)) {
          case 0: per_channel = 1; break;
          case 1: per_channel = 2; break;
          case 2: per_channel = 3; break;
          case 3: per_channel = tpc; break;
          case 4: per_channel = tpc + 1; break;
          case 5: per_channel = 2 * tpc; break;
          default: per_channel = r.range(1, 4 * (int)tpc); break;
        }
        // Keep the peak search inside its buffer (C++ over-reads otherwise).
        size_t start = std::min<size_t>(future, 1260);
        start = std::max(start, tpc + overlap);
        size_t pad = overlap - 1;
        if (per_channel < start && (start - per_channel) / (2 * fs_mult) > 60 + pad)
          per_channel = start;

        std::vector<int16_t> input(per_channel * channels);
        gen_signal(r, (int)r.below(4), input.data(), input.size(), fs_mult);
        out.Clear();
        h.feed((int64_t)merge.Process(input.data(), input.size(), &out));
        feed_multi(h, out);
        feed_expand(h, rig.expand, channels);
        feed_stats(h, rig.stats);
        h.feed((int64_t)rig.sb.FutureLength());
        feed_tail(h, rig.sb, future + 2 * overlap);
        h.feed((int64_t)merge.RequiredFutureSamples());
      }
    }
  }
  report("wp2a_merge", h);
}

int main() {
  wp2a_background_noise();
  wp2a_expand();
  wp2a_normal();
  wp2a_merge();
  return 0;
}
