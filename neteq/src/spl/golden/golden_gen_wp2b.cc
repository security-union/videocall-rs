// WP2b golden-value generator: drives the real WebRTC Accelerate,
// PreemptiveExpand, ComfortNoise, ComfortNoiseDecoder and ComfortNoiseEncoder
// (rev 6341cca49ffcad6fcb4603bd6cd1325fd0fe28bd, generic C SPL) on a
// deterministic speech-like signal and prints one FNV-1a hash per test, plus
// the time_stretch_unittest-style counters. Build and run with build_wp2b.sh.
// The Rust tests (`wp2b_golden` modules in neteq/src/neteq/{accelerate,
// preemptive_expand,comfort_noise}.rs and neteq/src/codecs/cng.rs) replay the
// same xorshift32 stream and compare.
//
// The background noise estimate is never updated here (its own golden tests
// cover BackgroundNoise), so TimeStretch takes its fixed-threshold VAD path.
#include <algorithm>
#include <cstdint>
#include <cstdio>
#include <vector>

#include "modules/audio_coding/codecs/cng/webrtc_cng.h"
#include "modules/audio_coding/neteq/accelerate.h"
#include "modules/audio_coding/neteq/audio_multi_vector.h"
#include "modules/audio_coding/neteq/background_noise.h"
#include "modules/audio_coding/neteq/comfort_noise.h"
#include "modules/audio_coding/neteq/decoder_database.h"
#include "modules/audio_coding/neteq/packet.h"
#include "modules/audio_coding/neteq/preemptive_expand.h"
#include "modules/audio_coding/neteq/sync_buffer.h"
#include "rtc_base/buffer.h"

using namespace webrtc;

// Same as golden_gen.cc.
struct Rng {
  uint32_t s;
  uint32_t next() { s ^= s << 13; s ^= s >> 17; s ^= s << 5; return s; }
  uint32_t below(uint32_t n) { return next() % n; }
  int range(int lo, int hi) { return lo + (int)below((uint32_t)(hi - lo + 1)); }
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

static const int kRates[] = {8000, 16000, 32000, 48000};

// Speech-like test signal in 20 ms segments: quiet noise, loud noise, a
// repeated random pitch period with a little noise, or a ramped pitch period.
static std::vector<int16_t> make_signal(Rng& r, size_t len, int fs) {
  std::vector<int16_t> out(len);
  size_t seg = (size_t)fs / 50;
  std::vector<int16_t> base(fs / 80 + 1);
  for (size_t start = 0; start < len; start += seg) {
    uint32_t mode = r.below(4);
    size_t period = (size_t)r.range(fs / 400, fs / 80);
    for (size_t k = 0; k < period; ++k) base[k] = (int16_t)((int)r.below(16001) - 8000);
    size_t end = std::min(len, start + seg);
    for (size_t i = start; i < end; ++i) {
      size_t j = i - start;
      int v;
      switch (mode) {
        case 0: v = (int)r.below(201) - 100; break;
        case 1: v = (int16_t)r.next() / 2; break;
        case 2: v = base[j % period] + (int)r.below(401) - 200; break;
        default: v = base[j % period] * (int)(j + 1) / (int)seg; break;
      }
      out[i] = (int16_t)v;
    }
  }
  return out;
}

// Interleaves `sig` into `channels` channels; channel c > 0 is a noisy,
// halved copy.
static std::vector<int16_t> interleave(Rng& r, const std::vector<int16_t>& sig,
                                       size_t channels) {
  std::vector<int16_t> in(sig.size() * channels);
  for (size_t i = 0; i < sig.size(); ++i) {
    for (size_t c = 0; c < channels; ++c) {
      in[i * channels + c] =
          c == 0 ? sig[i] : (int16_t)(sig[i] / 2 + (int)r.below(101) - 50);
    }
  }
  return in;
}

// Per-channel input length: mostly 30 to 32.5 ms (NetEq always hands over at
// least 30 ms), sometimes too short so Process takes its error path. (C++
// fast-mode Accelerate reads past the end of a 239 * fs_mult input, the
// shortest it accepts, so that length is avoided.)
static size_t block_length(Rng& r, int fs_mult) {
  if (r.below(10) == 0) return (size_t)r.range(200 * fs_mult, 239 * fs_mult - 1);
  return (size_t)r.range(240 * fs_mult, 260 * fs_mult);
}

static void feed_output(Hash& h, int ret, size_t change, const AudioMultiVector& out) {
  h.feed(ret);
  h.feed((int64_t)change);
  h.feed((int64_t)out.Size());
  std::vector<int16_t> o(out.Size() * out.Channels());
  out.ReadInterleaved(out.Size(), o.data());
  h.feed16(o.data(), o.size());
}

static void accelerate_golden() {
  Rng r{kSeed}; Hash h;
  for (int fs : kRates) {
    for (size_t channels = 1; channels <= 2; ++channels) {
      for (int fast = 0; fast <= 1; ++fast) {
        BackgroundNoise bgn(channels);
        Accelerate accelerate(fs, channels, bgn);
        int fs_mult = fs / 8000;
        for (int n = 0; n < 60; ++n) {
          size_t len = block_length(r, fs_mult);
          std::vector<int16_t> sig = make_signal(r, len, fs);
          std::vector<int16_t> in = interleave(r, sig, channels);
          AudioMultiVector out(channels);
          size_t change = 12345;
          int ret = accelerate.Process(in.data(), in.size(), fast != 0, &out, &change);
          feed_output(h, ret, change, out);
        }
      }
    }
  }
  report("accelerate", h);
}

static void preemptive_expand_golden() {
  Rng r{kSeed}; Hash h;
  for (int fs : kRates) {
    for (size_t channels = 1; channels <= 2; ++channels) {
      size_t overlap = 5 * fs / 8000;
      BackgroundNoise bgn(channels);
      PreemptiveExpand pe(fs, channels, bgn, overlap);
      int fs_mult = fs / 8000;
      for (int n = 0; n < 80; ++n) {
        size_t len = block_length(r, fs_mult);
        size_t old_len = (size_t)r.range(0, 260 * fs_mult);
        std::vector<int16_t> sig = make_signal(r, len, fs);
        std::vector<int16_t> in = interleave(r, sig, channels);
        AudioMultiVector out(channels);
        size_t change = 12345;
        int ret = pe.Process(in.data(), in.size(), old_len, &out, &change);
        feed_output(h, ret, change, out);
      }
    }
  }
  report("preemptive_expand", h);
}

// time_stretch_unittest.cc's TimeStretchTest on the synthetic signal instead
// of testfile32kHz.pcm: 100 blocks of 30 ms at 32 kHz, mono.
static void time_stretch_unittest_counts(bool fast_mode) {
  Rng r{kSeed};
  const int fs = 32000;
  const size_t block = 30 * fs / 1000;
  std::vector<int16_t> sig = make_signal(r, 100 * block, fs);
  BackgroundNoise bgn(1);
  Accelerate accelerate(fs, 1, bgn);
  size_t total = 0;
  int stats[3] = {0, 0, 0};
  for (size_t i = 0; i < 100; ++i) {
    AudioMultiVector out(1);
    size_t change = 0;
    int ret = accelerate.Process(&sig[i * block], block, fast_mode, &out, &change);
    if (ret < 0) { printf("error\n"); return; }
    ++stats[ret];
    total += change;
  }
  printf("time_stretch_%s total=%zu success=%d low_energy=%d no_stretch=%d\n",
         fast_mode ? "fast" : "normal", total, stats[0], stats[1], stats[2]);
}

static void cng_decoder_golden() {
  Rng r{kSeed}; Hash h;
  ComfortNoiseDecoder decoder;
  for (int n = 0; n < 400; ++n) {
    if (r.below(4) == 0) {
      std::vector<uint8_t> sid((size_t)r.range(1, 16));
      for (auto& b : sid) b = (uint8_t)r.next();
      decoder.UpdateSid(sid);
    }
    if (r.below(50) == 0) decoder.Reset();
    size_t len = r.below(20) == 0 ? 641 : (size_t)r.range(0, 640);
    bool new_period = r.below(5) == 0;
    std::vector<int16_t> out(len, 0);
    bool ok = decoder.Generate(std::span<int16_t>(out.data(), len), new_period);
    h.feed(ok);
    if (ok) h.feed16(out.data(), len);
  }
  report("cng_decoder", h);
}

static void cng_encoder_golden() {
  Rng r{kSeed}; Hash h;
  for (int fs : {8000, 16000, 32000, 48000, 64000}) {
    for (int quality : {1, 8, 12}) {
      for (int interval : {1, 100}) {
        ComfortNoiseEncoder encoder(fs, interval, quality);
        ComfortNoiseDecoder decoder;
        for (int n = 0; n < 30; ++n) {
          size_t len = std::min<size_t>(640, r.below(3) == 0 ? fs / 200 : fs / 100);
          std::vector<int16_t> sig = make_signal(r, len, fs);
          if (r.below(6) == 0) {
            for (auto& s : sig) s = (int16_t)(s >> 12);
          }
          bool force_sid = r.below(4) == 0;
          Buffer sid;
          size_t ret = encoder.Encode(std::span<const int16_t>(sig.data(), len), force_sid, &sid);
          h.feed((int64_t)ret);
          h.feed((int64_t)sid.size());
          for (size_t k = 0; k < sid.size(); ++k) h.feed(sid[k]);
          if (ret > 0) {
            decoder.UpdateSid(sid);
            std::vector<int16_t> out(160);
            decoder.Generate(std::span<int16_t>(out.data(), out.size()), n % 3 == 0);
            h.feed16(out.data(), out.size());
          }
        }
      }
    }
  }
  report("cng_encoder", h);
}

static void comfort_noise_golden() {
  Rng r{kSeed}; Hash h;
  for (int fs : kRates) {
    DecoderDatabase db;
    SyncBuffer sync_buffer(1, 1000);
    std::vector<int16_t> sig = make_signal(r, 1000, fs);
    for (size_t i = 0; i < 1000; ++i) sync_buffer[0][i] = sig[i];
    ComfortNoise cn(fs, &db, &sync_buffer);
    size_t max_request = std::min(600, fs / 50);
    for (int n = 0; n < 60; ++n) {
      if (r.below(3) == 0) {
        Packet packet;
        packet.payload_type = r.below(4) == 0 ? 99 : 13;
        packet.payload.resize((size_t)r.range(1, 14));
        for (auto& b : packet.payload) b = (uint8_t)r.next();
        h.feed(cn.UpdateParameters(packet));
      }
      if (r.below(8) == 0) cn.Reset();
      size_t requested = r.below(15) == 0 ? 640 : (size_t)r.range(1, (int)max_request);
      size_t channels = r.below(20) == 0 ? 2 : 1;
      AudioMultiVector out(channels);
      int ret = cn.Generate(requested, &out);
      h.feed(ret);
      h.feed((int64_t)out.Size());
      std::vector<int16_t> o(out.Size() * out.Channels());
      out.ReadInterleaved(out.Size(), o.data());
      h.feed16(o.data(), o.size());
      for (size_t i = sync_buffer.Size() - 64; i < sync_buffer.Size(); ++i) h.feed(sync_buffer[0][i]);
    }
  }
  report("comfort_noise", h);
}

// cng_unittest.cc scenarios on the synthetic signal instead of
// testfile32kHz.pcm; prints the return values the Rust port asserts.
static void cng_unittest_values() {
  Rng r{kSeed};
  std::vector<int16_t> speech = make_signal(r, 640, 32000);
  auto enc = [&](ComfortNoiseEncoder& e, size_t offset, size_t len, bool force) {
    Buffer sid;
    return e.Encode(std::span<const int16_t>(speech.data() + offset, len), force, &sid);
  };
  printf("cng_unittest encode:");
  for (int fs : {8000, 16000, 32000, 48000, 64000}) {
    int quality = fs == 32000 ? 12 : 8;
    ComfortNoiseEncoder e(fs, 100, quality);
    printf(" %zu", enc(e, 0, fs / 100, false));
    printf(" %zu", enc(e, 0, fs / 100, true));
  }
  ComfortNoiseEncoder e(16000, 100, 8);
  printf(" | update_sid: %zu", enc(e, 0, 160, true));
  e.Reset(16000, 100, 12);
  printf(" %zu", enc(e, 0, 160, true));
  printf(" %zu", enc(e, 160, 160, true));
  ComfortNoiseEncoder a(16000, 100, 8);
  printf(" | auto_sid:");
  for (int i = 0; i < 11; ++i) printf(" %zu", enc(a, 0, 160, false));
  ComfortNoiseEncoder s(16000, 1, 8);
  printf(" | auto_sid_short:");
  for (int i = 0; i < 11; ++i) printf(" %zu", enc(s, 0, 160, false));
  printf("\n");
}

int main() {
  accelerate_golden();
  preemptive_expand_golden();
  time_stretch_unittest_counts(false);
  time_stretch_unittest_counts(true);
  cng_decoder_golden();
  cng_encoder_golden();
  comfort_noise_golden();
  cng_unittest_values();
  return 0;
}
